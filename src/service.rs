//! The device service behind the window: one thread that owns the GoXLR Mini connection, the
//! loaded profile and the input engine. It takes JSON requests from the window and pushes state,
//! packets and status back as small JavaScript calls (`goxlr.state(...)` and so on).

use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{Value, json};

use crate::autotune::{self, MicSettings, Phase, Segment};
use crate::dcp::hex;
use crate::engine::Engine;
use crate::profile::Input;
use crate::state::{Command, State};
use crate::{asio, control, dcp, profile_file, tusb};

/// Where the official app keeps things, from %APPDATA%\TC-Helicon\GoXLR\GoXLR.settings.
pub struct Paths {
    pub profiles: PathBuf,
    pub mic_profiles: PathBuf,
    pub last_profile: String,
    pub last_mic_profile: String,
    pub backups: PathBuf,
    /// Our own record of the last loaded profiles. The official app's GoXLR.settings is only the
    /// first-run fallback: with that app uninstalled it never changes again.
    pub settings: PathBuf,
    /// "Voice Chat Mute All Also Mutes Mic To Chat Mic". The official app keeps it in GoXLR.settings
    /// as `voiceChatMuteMutesMic`, default on.
    pub chat_mute_mutes_mic: bool,
}

impl Paths {
    pub fn discover() -> Result<Self> {
        let appdata = PathBuf::from(std::env::var("APPDATA")?);
        let docs = crate::desktop::documents().join("GoXLR");
        let text = std::fs::read_to_string(appdata.join("TC-Helicon/GoXLR/GoXLR.settings")).unwrap_or_default();
        let value = |name: &str| -> Option<String> {
            let key = format!("name=\"{name}\" val=\"");
            let start = text.find(&key)? + key.len();
            Some(unescape_xml(&text[start..start + text[start..].find('"')?]))
        };
        let ours = crate::desktop::data_dir()?;
        let settings = ours.join("settings.json");
        let saved: Value = std::fs::read_to_string(&settings)
            .ok()
            .and_then(|t| serde_json::from_str(t.trim_start_matches('\u{feff}')).ok())
            .unwrap_or_default();
        let remembered = |key: &str| saved[key].as_str().map(str::to_string);
        Ok(Self {
            profiles: value("profilePath").map_or(docs.join("Profiles"), PathBuf::from),
            mic_profiles: value("micProfilesPath").map_or(docs.join("MicProfiles"), PathBuf::from),
            last_profile: remembered("profile").or_else(|| value("lastLoadedProfile")).unwrap_or_else(|| "Default.goxlr".into()),
            last_mic_profile: remembered("micProfile")
                .or_else(|| value("LastLoadedMicProfile"))
                .unwrap_or_else(|| "Default.goxlrMicProfile".into()),
            backups: ours.join("backups"),
            chat_mute_mutes_mic: saved["chatMuteMutesMic"]
                .as_bool()
                .or_else(|| value("voiceChatMuteMutesMic").map(|v| v != "0"))
                .unwrap_or(true),
            settings,
        })
    }
}

/// Decodes the five predefined XML entities in an attribute value.
fn unescape_xml(s: &str) -> String {
    s.replace("&quot;", "\"").replace("&apos;", "'").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

fn has_ext(name: &str, ext: &str) -> bool {
    name.len() > ext.len()
        && name.is_char_boundary(name.len() - ext.len())
        && name[name.len() - ext.len()..].eq_ignore_ascii_case(ext)
}

/// A profile's name without its extension, matched case-insensitively.
fn stem<'n>(name: &'n str, ext: &str) -> &'n str {
    if has_ext(name, ext) { &name[..name.len() - ext.len()] } else { name }
}

fn list(dir: &Path, ext: &str) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|d| {
            d.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| has_ext(n, ext)).collect()
        })
        .unwrap_or_default();
    names.sort_by_key(|n| n.to_lowercase());
    names
}

struct Service<'a, E: Fn(String)> {
    emit: E,
    paths: Paths,
    profile: String,
    mic_profile: String,
    engine: Option<Engine>,
    dev: Option<dcp::Device<'a>>,
    status: (String, String),
    /// The window shows a level meter: poll the mic level (0x80c, read-only).
    meter_on: bool,
    /// A profile problem to show in the window (also re-sent when the page reports ready).
    problem: Value,
    /// Firmware versions of the connected Mini (also re-sent when the page reports ready).
    device: Value,
    /// Mic auto-tune: the guided recording in progress, and the session around it.
    recording: Option<Recording>,
    tuning: Tuning,
}

/// A guided auto-tune recording in progress.
struct Recording {
    recorder: asio::Recorder,
    full: bool,
    phase: usize,
    phase_start: Instant,
    segments: Vec<Segment>,
    last_level: Instant,
}

/// The auto-tune between its first recording and Keep or Undo.
#[derive(Default)]
struct Tuning {
    /// The mic settings before the auto-tune changed anything: what Undo goes back to.
    original: Option<MicSettings>,
    /// The latest proposal, not applied yet.
    proposal: Option<MicSettings>,
    /// The reading from the full test (old settings): processed and dry.
    before: Option<(Vec<f32>, Vec<f32>)>,
    sample_rate: f64,
    folder: Option<PathBuf>,
}

fn phases(full: bool) -> &'static [Phase] {
    if full { &autotune::FULL } else { &autotune::CHECK }
}

pub fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            out.push(if i <= c.len() { T[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

/// Local date and time, e.g. ("2026-10-12", "21.05").
fn local_stamp() -> (String, String) {
    let mut t = unsafe { std::mem::zeroed::<windows_sys::Win32::Foundation::SYSTEMTIME>() };
    unsafe { windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut t) };
    (format!("{:04}-{:02}-{:02}", t.wYear, t.wMonth, t.wDay), format!("{:02}.{:02}", t.wHour, t.wMinute))
}

impl<'a, E: Fn(String)> Service<'a, E> {
    fn set_problem(&mut self, v: Value) {
        self.problem = v.clone();
        self.js("problem", v);
    }

    fn js(&self, func: &str, v: Value) {
        (self.emit)(format!("window.goxlr && goxlr.{func}({v})"));
    }

    fn set_status(&mut self, kind: &str, message: &str) {
        if self.status.0 != kind || self.status.1 != message {
            self.status = (kind.into(), message.into());
            self.js("status", json!({ "kind": kind, "message": message }));
        }
    }

    fn push_state(&self) {
        if let Some(engine) = &self.engine {
            let mut snap = control::snapshot(engine);
            snap["profile"] = json!(stem(&self.profile, ".goxlr"));
            snap["micProfile"] = json!(stem(&self.mic_profile, ".goxlrMicProfile"));
            self.js("state", snap);
        }
    }

    /// Drops the device after a transport error; the main loop reconnects.
    fn disconnect(&mut self, error: &str) {
        self.dev = None;
        self.set_status("error", &format!("Lost the connection to the Mini ({error}). Reconnecting…"));
    }

    fn push_lists(&self) {
        self.js(
            "lists",
            json!({
                "profiles": list(&self.paths.profiles, ".goxlr"),
                "micProfiles": list(&self.paths.mic_profiles, ".goxlrMicProfile"),
            }),
        );
    }

    fn push_app(&self) {
        let (on, here) = crate::desktop::autostart();
        let allowlist: Vec<Value> =
            dcp::ALLOWED_CATEGORIES.iter().map(|&(cat, what)| json!({ "cat": cat, "what": what })).collect();
        self.js(
            "app",
            json!({
                "autostart": on,
                "autostartHere": here,
                "chatMuteMutesMic": self.paths.chat_mute_mutes_mic,
                "version": env!("CARGO_PKG_VERSION"),
                "allowlist": allowlist,
            }),
        );
    }

    fn event(&self, text: &str) {
        self.js("event", json!(text));
    }

    fn load(&mut self, profile: &str, mic: &str) -> Result<()> {
        let mut state = State::load(&self.paths.profiles.join(profile), &self.paths.mic_profiles.join(mic))?;
        state.chat_mute_mutes_mic = self.paths.chat_mute_mutes_mic;
        self.engine = Some(Engine::new(state));
        self.profile = profile.into();
        self.mic_profile = mic.into();
        self.set_problem(Value::Null);
        self.write_settings();
        Ok(())
    }

    /// Remembers the loaded profiles and app settings for next start (best effort).
    fn write_settings(&self) {
        let _ = std::fs::create_dir_all(self.paths.settings.parent().unwrap_or(Path::new(".")));
        let settings = json!({
            "profile": self.profile,
            "micProfile": self.mic_profile,
            "chatMuteMutesMic": self.paths.chat_mute_mutes_mic,
        });
        let _ = std::fs::write(&self.paths.settings, settings.to_string());
    }

    /// Startup load. If a remembered file is gone, falls back to one that exists and says so.
    fn load_startup(&mut self, profile: &str, mic: &str) {
        let pick = |dir: &Path, want: &str, ext: &str| -> String {
            if dir.join(want).is_file() {
                return want.to_string();
            }
            let all = list(dir, ext);
            let default = format!("Default{ext}");
            if all.contains(&default) { default } else { all.into_iter().next().unwrap_or_else(|| want.to_string()) }
        };
        let p = pick(&self.paths.profiles.clone(), profile, ".goxlr");
        let m = pick(&self.paths.mic_profiles.clone(), mic, ".goxlrMicProfile");
        match self.load(&p, &m) {
            Ok(()) => {
                let mut swapped = Vec::new();
                if p != profile {
                    swapped.push(format!("profile {profile} is missing, loaded {p}"));
                }
                if m != mic {
                    swapped.push(format!("mic profile {mic} is missing, loaded {m}"));
                }
                if !swapped.is_empty() {
                    self.set_problem(json!(format!("Your {}. Pick another on the Device page if you like.", swapped.join("; "))));
                }
            }
            Err(e) => self.set_problem(json!(format!(
                "Couldn't load a profile ({e:#}). Nothing is sent to the Mini until one loads. Check the Device page."
            ))),
        }
    }

    /// Sends commands, logging each to the window. Drops the connection on failure.
    fn send(&mut self, commands: &[Command], what: &str) {
        let Some(dev) = self.dev.as_mut() else { return };
        let mut failure = None;
        for c in commands {
            let shown = &c.body[..c.body.len().min(48)];
            match dev.request(c.category, c.opcode, &c.body) {
                Ok(_) => (self.emit)(format!(
                    "window.goxlr && goxlr.packet({})",
                    json!({ "cat": c.category, "op": c.opcode, "len": c.body.len(), "hex": hex(shown), "what": what, "ok": true })
                )),
                Err(e) => {
                    failure = Some(format!("{e:#}"));
                    break;
                }
            }
        }
        if let Some(e) = failure {
            self.js("packet", json!({ "what": what, "ok": false, "error": e }));
            self.disconnect(&e);
        }
    }

    fn push_profile_to_device(&mut self) {
        if let Some(engine) = &self.engine {
            let commands = engine.state.startup();
            self.send(&commands, "Profile");
        }
    }

    fn save(&mut self) -> Result<String> {
        // Settings the auto-tune is only trying out are never saved into the mic profile.
        self.recording = None;
        if let Some(original) = self.tuning.original.take() {
            self.apply_mic(&original, "Auto-tune undo");
            self.tuning = Tuning::default();
        }
        let Some(engine) = self.engine.as_mut() else { return Ok("Nothing loaded".into()) };
        let profile_path = self.paths.profiles.join(&self.profile);
        let mic_path = self.paths.mic_profiles.join(&self.mic_profile);
        let p = profile_file::save_edits(&profile_path, &engine.state.changed_edits(), &self.paths.backups)?;
        let m = profile_file::save_xml_edits(&mic_path, &engine.state.changed_mic_edits(), &self.paths.backups)?;
        engine.state.mark_saved();
        Ok(match (p, m) {
            (None, None) => "No changes to save".into(),
            _ => format!("Saved. Previous versions are in {}", self.paths.backups.display()),
        })
    }

    fn tune(&self, v: Value) {
        self.js("tune", v);
    }

    /// Sets mic settings through the same path as the window's controls, then sends them once
    /// (every mic control returns the full mic command list).
    fn apply_mic(&mut self, settings: &MicSettings, what: &str) {
        let Some(engine) = self.engine.as_mut() else { return };
        let mut last = Vec::new();
        let mut errors = Vec::new();
        for c in settings.commands() {
            match control::apply(engine, &c) {
                Ok(commands) => last = commands,
                Err(e) => errors.push(format!("{e:#}")),
            }
        }
        for e in errors {
            self.event(&e);
        }
        self.send(&last, what);
        self.push_state();
    }

    fn autotune(&mut self, v: &Value) {
        match v["action"].as_str().unwrap_or_default() {
            "start" => {
                let full = v["mode"] != "check";
                let problem = if self.engine.is_none() {
                    Some("No mic profile is loaded.")
                } else if self.dev.is_none() {
                    Some("The Mini isn't connected.")
                } else if !full && self.tuning.original.is_none() {
                    Some("Run the full test first.")
                } else {
                    None
                };
                if let Some(p) = problem {
                    return self.tune(json!({ "type": "error", "message": p }));
                }
                if self.tuning.original.is_none() {
                    self.tuning.original = self.engine.as_ref().map(control::mic_settings);
                }
                self.recording = None;
                match asio::Recorder::start() {
                    Ok(recorder) => {
                        let now = Instant::now();
                        self.recording =
                            Some(Recording { recorder, full, phase: 0, phase_start: now, segments: Vec::new(), last_level: now });
                        self.tune_phase();
                    }
                    Err(e) => self.tune(json!({ "type": "error", "message": format!("Couldn't start recording: {e:#}") })),
                }
            }
            "cancel" => self.recording = None,
            "try" => {
                if let Some(p) = self.tuning.proposal.take() {
                    self.apply_mic(&p, "Auto-tune");
                }
                self.tune(json!({ "type": "applied" }));
            }
            "undo" => {
                self.recording = None;
                if let Some(o) = self.tuning.original.take()
                    && self.engine.as_ref().is_some_and(|e| control::mic_settings(e) != o)
                {
                    self.apply_mic(&o, "Auto-tune undo");
                    self.event("Your mic settings are back to how they were");
                }
                self.tuning = Tuning::default();
                self.tune(json!({ "type": "closed" }));
            }
            "keep" => match self.keep_tuned() {
                Ok(name) => {
                    self.tuning = Tuning::default();
                    self.event(&format!("Saved as the mic profile \"{name}\". Your previous mic profile is unchanged."));
                    self.tune(json!({ "type": "kept", "name": name }));
                }
                Err(e) => self.tune(json!({ "type": "error", "message": format!("Couldn't save: {e:#}") })),
            },
            "openFolder" => {
                if let Some(f) = &self.tuning.folder {
                    let _ = std::process::Command::new("explorer.exe").arg(f).spawn();
                }
            }
            _ => {}
        }
    }

    fn tune_phase(&self) {
        let Some(rec) = &self.recording else { return };
        let list = phases(rec.full);
        let p = &list[rec.phase];
        self.tune(json!({ "type": "phase", "mode": if rec.full { "full" } else { "check" }, "index": rec.phase, "count": list.len(), "key": p.key, "secs": p.secs }));
    }

    /// Called every loop: live level, step timing, and the analysis at the end.
    fn tick_recording(&mut self) {
        let Some(rec) = self.recording.as_mut() else { return };
        let now = Instant::now();
        let level = (now.duration_since(rec.last_level) >= Duration::from_millis(50)).then(|| {
            rec.last_level = now;
            rec.recorder.take_peak_db()
        });
        let list = phases(rec.full);
        let mut advanced = false;
        if now.duration_since(rec.phase_start).as_secs_f64() >= list[rec.phase].secs {
            let start = rec.segments.last().map_or(0, |s| s.end);
            rec.segments.push(Segment { key: list[rec.phase].key, start, end: rec.recorder.len() });
            rec.phase += 1;
            rec.phase_start = now;
            advanced = true;
        }
        let done = rec.phase >= list.len();
        if let Some(db) = level {
            self.tune(json!({ "type": "level", "db": (db * 10.0).round() / 10.0 }));
        }
        if done {
            if let Some(rec) = self.recording.take() {
                self.finish_recording(rec);
            }
        } else if advanced {
            self.tune_phase();
        }
    }

    fn finish_recording(&mut self, rec: Recording) {
        self.tune(json!({ "type": "analysing" }));
        let take = match rec.recorder.finish() {
            Ok(t) => t,
            Err(e) => {
                return self.tune(json!({ "type": "error", "message": format!("The recording failed: {e:#}"), "retry": true }));
            }
        };
        let analysis = match autotune::analyse(&take.dry, &take.processed, take.sample_rate, &rec.segments, rec.full) {
            Ok(a) => a,
            Err(message) => return self.tune(json!({ "type": "error", "message": message, "retry": true })),
        };
        let Some(engine) = self.engine.as_ref() else { return };
        let current = control::mic_settings(engine);
        let proposal = autotune::propose(&analysis, &current, rec.full);
        let (c0, c1) = analysis.clip;
        let clip = |x: &[f32]| x.get(c0..c1.min(x.len())).unwrap_or_default().to_vec();
        if rec.full {
            self.tuning.before = Some((clip(&take.processed), clip(&take.dry)));
            self.tuning.sample_rate = take.sample_rate;
        }
        self.tuning.proposal = (proposal.settings != current).then_some(proposal.settings);
        self.tune(json!({
            "type": "result", "mode": if rec.full { "full" } else { "check" },
            "analysis": analysis.to_json(), "changes": proposal.changes, "notes": proposal.notes,
            "discord": autotune::discord_sensitivity(&analysis),
            "current": current.to_json(), "proposed": proposal.settings.to_json(),
        }));
        if !rec.full {
            self.before_after(clip(&take.processed), clip(&take.dry));
        }
    }

    /// Exports the before/after pair to the Desktop and hands both clips to the window.
    fn before_after(&mut self, after: Vec<f32>, after_dry: Vec<f32>) {
        let Some((before, before_dry)) = &self.tuning.before else { return };
        let fs = self.tuning.sample_rate;
        let (gb, ga) = autotune::matched_gains(before, &after, fs);
        let (date, time) = local_stamp();
        let desktop = crate::desktop::desktop();
        let root = if desktop.is_dir() { desktop.join("GoXLR mic test") } else { self.paths.backups.join("mic tests") };
        let folder = root.join(format!("Auto-tune {date} {time}"));
        let files: [(&str, &[f32], f64); 6] = [
            ("1 - BEFORE (your settings).wav", before, 1.0),
            ("2 - AFTER (auto-tune).wav", &after, 1.0),
            ("3 - BEFORE, volume matched.wav", before, gb),
            ("4 - AFTER, volume matched.wav", &after, ga),
            ("5 - raw mic, no processing (before).wav", before_dry, 1.0),
            ("6 - raw mic, no processing (after).wav", &after_dry, 1.0),
        ];
        let saved = std::fs::create_dir_all(&folder).is_ok()
            && files.iter().all(|(name, x, g)| std::fs::write(folder.join(name), autotune::wav(x, fs, *g)).is_ok());
        let before_wav = base64(&autotune::wav(before, fs, 1.0));
        let after_wav = base64(&autotune::wav(&after, fs, 1.0));
        // Volumes for the matched comparison: the louder clip is turned down.
        let top = gb.max(ga);
        self.tune(json!({ "type": "audio", "which": "before", "wav": before_wav, "matched": gb / top }));
        self.tune(json!({ "type": "audio", "which": "after", "wav": after_wav, "matched": ga / top,
            "folder": if saved { json!(folder.display().to_string()) } else { Value::Null } }));
        self.tuning.folder = saved.then_some(folder);
    }

    /// Saves the tuned settings as a new mic profile next to the current one, and switches to it.
    /// The current mic profile file isn't touched.
    fn keep_tuned(&mut self) -> Result<String> {
        use anyhow::Context;
        let engine = self.engine.as_mut().context("no mic profile is loaded")?;
        let current = stem(&self.mic_profile, ".goxlrMicProfile");
        let base = current.split(" (Auto-tune").next().unwrap_or(current).trim().to_string();
        let (date, _) = local_stamp();
        let mut name = format!("{base} (Auto-tune {date}).goxlrMicProfile");
        let mut n = 2;
        while self.paths.mic_profiles.join(&name).exists() {
            name = format!("{base} (Auto-tune {date} {n}).goxlrMicProfile");
            n += 1;
        }
        let mut xml = std::fs::read_to_string(self.paths.mic_profiles.join(&self.mic_profile))?;
        for (element, attr, value) in engine.state.changed_mic_edits() {
            xml = profile_file::set_attribute(&xml, &element, &attr, &value)?;
        }
        std::fs::write(self.paths.mic_profiles.join(&name), xml)?;
        engine.state.mark_mic_saved();
        self.mic_profile = name.clone();
        self.write_settings();
        self.push_lists();
        self.push_state();
        Ok(stem(&name, ".goxlrMicProfile").to_string())
    }

    fn handle(&mut self, v: &Value) -> bool {
        match v["cmd"].as_str().unwrap_or_default() {
            "ready" => {
                self.push_lists();
                self.push_app();
                self.js("problem", self.problem.clone());
                if !self.device.is_null() {
                    self.js("device", self.device.clone());
                }
                let (k, m) = self.status.clone();
                self.js("status", json!({ "kind": k, "message": m }));
                self.push_state();
            }
            "save" => {
                match self.save() {
                    Ok(msg) => self.event(&msg),
                    Err(e) => self.event(&format!("Couldn't save: {e:#}")),
                }
                self.push_state();
            }
            "loadProfile" | "loadMicProfile" => {
                if let Err(e) = self.save() {
                    self.event(&format!("Couldn't save before switching: {e:#}"));
                    return true;
                }
                let name = v["name"].as_str().unwrap_or_default().to_string();
                let (p, m) =
                    if v["cmd"] == "loadProfile" { (name, self.mic_profile.clone()) } else { (self.profile.clone(), name) };
                match self.load(&p, &m) {
                    Ok(()) => {
                        self.event(&format!("Loaded {}", stem(&p, ".goxlr")));
                        self.push_profile_to_device();
                    }
                    Err(e) => self.event(&format!("Couldn't load: {e:#}")),
                }
                self.push_state();
            }
            "meter" => self.meter_on = v["on"].as_bool().unwrap_or(false),
            "autotune" => self.autotune(v),
            "autostart" => {
                let on = v["on"].as_bool().unwrap_or(false);
                match crate::desktop::set_autostart(on) {
                    Ok(()) => self.event(if on { "The app will start with Windows" } else { "The app won't start with Windows" }),
                    Err(e) => self.event(&format!("Couldn't change autostart: {e:#}")),
                }
                self.push_app();
            }
            "chatMuteMutesMic" => {
                let on = v["on"].as_bool().unwrap_or(true);
                self.paths.chat_mute_mutes_mic = on;
                self.write_settings();
                let rows = self.engine.as_mut().map(|engine| {
                    engine.state.chat_mute_mutes_mic = on;
                    engine.state.routing_rows(Input::Mic)
                });
                if let Some(rows) = rows {
                    self.send(&rows, "Mic routing");
                }
                self.push_app();
            }
            "quit" => {
                self.recording = None;
                match self.save() {
                    Ok(_) => {}
                    Err(e) => eprintln!("save on exit failed: {e:#}"),
                }
                return false;
            }
            _ => {
                let Some(engine) = self.engine.as_mut() else { return true };
                match control::apply(engine, v) {
                    Ok(commands) => {
                        let what = v["cmd"].as_str().unwrap_or_default().to_string();
                        self.send(&commands, &what);
                    }
                    Err(e) => self.event(&format!("{e:#}")),
                }
                self.push_state();
            }
        }
        true
    }
}

/// The quietest level the 0x80c meter can show: one step of 4096.
const MIC_LEVEL_FLOOR_DB: f64 = -72.247_198_959_355_48; // 20·log10(1/4096)

/// Converts the 0x80c mic level to dBFS the way the official app's mic setup meter does:
/// 20·log10(level / 4096), clamped to the floor…0 (a level of 0 is silence).
fn mic_level_db(level: u16) -> f64 {
    if level == 0 {
        return MIC_LEVEL_FLOOR_DB;
    }
    (20.0 * (f64::from(level) / 4096.0).log10()).clamp(MIC_LEVEL_FLOOR_DB, 0.0)
}

/// Runs the service until the window asks it to quit. `emit` runs JavaScript in the window.
pub fn run(rx: Receiver<Value>, emit: impl Fn(String)) {
    let paths = match Paths::discover() {
        Ok(p) => p,
        Err(e) => {
            emit(format!("window.goxlr && goxlr.status({})", json!({ "kind": "error", "message": format!("{e:#}") })));
            return;
        }
    };
    let (profile, mic) = (paths.last_profile.clone(), paths.last_mic_profile.clone());
    let api = tusb::Api::load();
    let mut svc = Service {
        emit,
        paths,
        profile: String::new(),
        mic_profile: String::new(),
        engine: None,
        dev: None,
        status: Default::default(),
        meter_on: false,
        problem: Value::Null,
        device: Value::Null,
        recording: None,
        tuning: Tuning::default(),
    };
    svc.load_startup(&profile, &mic);

    let mut next_connect = Instant::now();
    let mut last_poll = Instant::now();
    let mut last_raw = Vec::new();
    let mut last_meter = Instant::now();
    // Windows peak meters for the PC channels; looked up again every few seconds until found.
    let mut pc_meters: Option<crate::meters::Meters> = None;
    let mut next_meter_scan = Instant::now();
    loop {
        // Requests from the window. Wait a little when there is no device to watch.
        let wait = if svc.dev.is_some() { Duration::ZERO } else { Duration::from_millis(50) };
        let mut first = rx.recv_timeout(wait).ok();
        while let Some(v) = first.take().or_else(|| rx.try_recv().ok()) {
            if !svc.handle(&v) {
                return;
            }
        }

        if svc.dev.is_none() && Instant::now() >= next_connect {
            next_connect = Instant::now() + Duration::from_secs(2);
            match &api {
                Err(e) => svc.set_status("error", &format!("The GoXLR driver isn't available: {e:#}")),
                Ok(_) if crate::official_app_running() => {
                    svc.set_status("waiting", "The official GoXLR App is open. Close it to control the Mini from here.")
                }
                Ok(api) => match dcp::Device::open_mini(api) {
                    Ok(mut dev) => {
                        if let Some(v) = dev.request(0x80f, 0, &[]).ok().and_then(|r| dcp::Versions::parse(&r.body)) {
                            let ((a, b, c, build), (x, y, z, _)) = (v.firmware, v.dice);
                            svc.device = json!({
                                "firmware": format!("{a}.{b}.{c} build {build}"),
                                "dice": format!("{x}.{y}.{z}"),
                                // Submix arrived in Mini firmware 1.2.0 build 47 (the vendor's release notes).
                                "submix": v.firmware >= (1, 2, 0, 47),
                            });
                            svc.js("device", svc.device.clone());
                        }
                        svc.dev = Some(dev);
                        svc.set_status("connected", "GoXLR Mini connected");
                        svc.push_profile_to_device();
                        svc.push_state();
                        last_raw.clear();
                    }
                    Err(e) => svc.set_status("waiting", &format!("Looking for the GoXLR Mini… ({e:#})")),
                },
            }
        }

        svc.tick_recording();
        let Some(dev) = svc.dev.as_mut() else { continue };
        let changed = match dev.wait_input(15) {
            Ok(c) => c,
            Err(e) => {
                svc.disconnect(&format!("{e:#}"));
                continue;
            }
        };
        let now = Instant::now();
        let mut out = Vec::new();
        if changed || now.duration_since(last_poll) >= Duration::from_millis(250) {
            last_poll = now;
            match dev.request(0x800, 0, &[]) {
                Ok(reply) => {
                    if reply.body != last_raw {
                        last_raw = reply.body.clone();
                        if let Some(engine) = svc.engine.as_mut() {
                            out.extend(engine.on_input(&reply.body, now));
                        }
                    }
                }
                Err(e) => {
                    svc.disconnect(&format!("{e:#}"));
                    continue;
                }
            }
        }
        // Levels for the meters, about 20 times a second. Not logged to the wire view (too chatty).
        if svc.meter_on && now.duration_since(last_meter) >= Duration::from_millis(50) {
            last_meter = now;
            if pc_meters.as_ref().is_none_or(|m| m.is_empty()) && now >= next_meter_scan {
                next_meter_scan = now + Duration::from_secs(5);
                pc_meters = crate::meters::Meters::open().ok();
            }
            let channels: serde_json::Map<String, Value> = match pc_meters.as_ref().map(|m| m.read()) {
                Some(Some(levels)) => levels.into_iter().map(|(n, db)| (n.to_string(), json!(db))).collect(),
                Some(None) => {
                    pc_meters = None; // a device went away; look again later
                    Default::default()
                }
                None => Default::default(),
            };
            match dev.request(0x80c, 0, &[]) {
                Ok(reply) if reply.body.len() >= 2 => {
                    let level = u16::from_le_bytes([reply.body[0], reply.body[1]]);
                    svc.js("meter", json!({ "mic": mic_level_db(level), "channels": channels }));
                }
                Ok(_) => {}
                Err(e) => {
                    svc.disconnect(&format!("{e:#}"));
                    continue;
                }
            }
        }
        let mut lines = Vec::new();
        if let Some(engine) = svc.engine.as_mut() {
            out.extend(engine.tick(now));
            lines = std::mem::take(&mut engine.events);
        }
        if !out.is_empty() {
            svc.send(&out, "Hardware");
        }
        if !lines.is_empty() || !out.is_empty() {
            // Hardware actions show on the controls themselves; the wire view keeps a line for each.
            for l in &lines {
                svc.js("packet", json!({ "note": l }));
            }
            svc.push_state();
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn profile_names_and_settings_values() {
        assert_eq!(super::stem("Live.GOXLR", ".goxlr"), "Live");
        assert_eq!(super::stem("a.goxlr.goxlr", ".goxlr"), "a.goxlr");
        assert!(!super::has_ext(".goxlr", ".goxlr"));
        assert_eq!(super::unescape_xml("C:\\A &amp; B &quot;x&quot; &amp;lt;"), "C:\\A & B \"x\" &lt;");
    }

    #[test]
    fn base64_matches_the_standard_alphabet() {
        assert_eq!(super::base64(b"Man"), "TWFu");
        assert_eq!(super::base64(b"Ma"), "TWE=");
        assert_eq!(super::base64(b"M"), "TQ==");
        assert_eq!(super::base64(b""), "");
    }

    #[test]
    fn mic_level_scale() {
        assert_eq!(super::mic_level_db(0), super::MIC_LEVEL_FLOOR_DB);
        assert!((super::mic_level_db(1) - super::MIC_LEVEL_FLOOR_DB).abs() < 1e-9);
        assert_eq!(super::mic_level_db(4096), 0.0);
        assert_eq!(super::mic_level_db(u16::MAX), 0.0);
        assert!((super::mic_level_db(2048) + 6.0206).abs() < 1e-3);
    }
}
