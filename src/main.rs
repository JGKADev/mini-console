//! Mini Console: an unofficial control app for the TC-Helicon GoXLR Mini.
//!
//! Protocol knowledge comes from studying how the official app drives the device (USB traffic and
//! its settings tables). This program never flashes firmware; see dcp::ALLOWED_CATEGORIES.

// Release builds are a windowed app (no console flash). The command-line tools attach to the
// terminal they were started from instead.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
// The DSP and matrix code reads more clearly with index loops.
#![allow(clippy::needless_range_loop)]

mod asio;
mod autotune;
mod colours;
mod compare;
mod control;
mod dcp;
mod desktop;
mod engine;
mod golden;
mod meters;
mod profile;
mod profile_file;
mod service;
mod state;
mod tusb;
mod ui;
mod xml;

use anyhow::{Context, Result, bail};
use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};

fn main() -> Result<()> {
    let cmd = std::env::args().nth(1).unwrap_or_default();
    if !cmd.is_empty() && cmd != desktop::MINIMIZED_FLAG {
        attach_parent_console();
    }
    match cmd.as_str() {
        "info" => info(),
        "run" => run(&std::env::args().skip(2).collect::<Vec<_>>()),
        "golden" => {
            // Offline: prints everything the app derives from a profile, and what a fixed script of
            // controls sends. Two builds with the same output behave the same.
            let a: Vec<String> = std::env::args().skip(2).collect();
            let [p, m] = a.as_slice() else { bail!("golden <profile> <mic>") };
            golden::run(p, m)
        }
        "apply" => apply(&std::env::args().skip(2).collect::<Vec<_>>()),
        "volume" => volume(&std::env::args().skip(2).collect::<Vec<_>>()),
        "plan" => plan(&std::env::args().skip(2).collect::<Vec<_>>()),
        "meters" => meters(),
        "snapshot" => snapshot(&std::env::args().skip(2).collect::<Vec<_>>()),
        "record" => record(&std::env::args().skip(2).collect::<Vec<_>>()),
        "autotune-wav" => autotune_wav(&std::env::args().skip(2).collect::<Vec<_>>()),
        "" | desktop::MINIMIZED_FLAG => {
            hide_own_console();
            if !desktop::claim_single_instance() {
                return Ok(()); // the open window was brought to the front instead
            }
            ui::run(cmd == desktop::MINIMIZED_FLAG)
        }
        _ => {
            eprintln!("usage: mini-console        (opens the app window)");
            eprintln!("       mini-console info    (read-only: versions, serial, hardware state)");
            eprintln!(
                "       mini-console run <profile.goxlr> <mic.goxlrMicProfile> [seconds]   (apply, then handle buttons and faders)"
            );
            eprintln!("       mini-console apply <profile.goxlr> <mic.goxlrMicProfile>   (sends the full startup state)");
            eprintln!("       mini-console volume <channel id 0-10> <0-255>   (writes one channel volume)");
            eprintln!("       mini-console plan <profile.goxlr> <mic.goxlrMicProfile> [capture.jsonl]");
            eprintln!("            (offline: build the startup state push; with a capture, compare against it)");
            eprintln!(
                "       mini-console snapshot <profile.goxlr> <mic.goxlrMicProfile>   (offline: the window's state as JSON)"
            );
            eprintln!("       mini-console meters    (PC channel levels from Windows)");
            eprintln!("       mini-console record <seconds> <dir>   (records the dry and processed mic via ASIO)");
            eprintln!("       mini-console autotune-wav <dir> <silence secs> <profile.goxlr> <mic.goxlrMicProfile>");
            eprintln!("            (offline: mic auto-tune analysis of a recorded take)");
            Ok(())
        }
    }
}

/// Read-only: prints device identity, firmware versions and the current hardware state.
fn info() -> Result<()> {
    let api = tusb::Api::load()?;
    let mut dev = open_device(&api)?;

    let props = dev.properties()?;
    println!(
        "device       : {} {} (VID {:04x} PID {:04x})",
        props.manufacturer(),
        props.model(),
        props.vendor_id,
        props.product_id
    );

    let v = dcp::Versions::parse(&dev.request(0x80f, 0, &[])?.body).context("short version reply")?;
    let (a, b, c, d) = v.firmware;
    println!("firmware     : {a}.{b}.{c}.{d}");
    let (a, b, c, d) = v.dice;
    println!("DICE         : {a}.{b}.{c}.{d}");

    let s = dev.request(0x80f, 1, &[])?.body;
    let text = |b: &[u8]| String::from_utf8_lossy(&b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())]).into_owned();
    println!("serial       : {}", text(&s[..24.min(s.len())]));
    println!("manufactured : {}", text(&s[24.min(s.len())..]));

    let st = dev.request(0x800, 0, &[])?.body;
    if st.len() >= 12 {
        let buttons = u32::from_le_bytes(st[0..4].try_into()?);
        let encoders: Vec<i8> = st[4..8].iter().map(|&b| b as i8).collect();
        println!("buttons mask : {buttons:#010x}");
        println!("encoders     : {encoders:?}");
        println!("faders       : {:?}", &st[8..12]);
    }
    Ok(())
}

/// Applies a profile, then reacts to the hardware: mute, cough and bleep buttons, and faders.
/// Runs until Ctrl+C, or for the given number of seconds.
fn run(args: &[String]) -> Result<()> {
    let [profile, mic, rest @ ..] = args else { bail!("run needs <profile.goxlr> <mic.goxlrMicProfile> [seconds]") };
    let limit = rest.first().map(|s| s.parse::<u64>()).transpose()?.map(std::time::Duration::from_secs);
    let mut engine = engine::Engine::new(state::State::load(profile.as_ref(), mic.as_ref())?);
    let api = tusb::Api::load()?;
    let mut dev = open_device(&api)?;
    let start = std::time::Instant::now();
    let stamp = || format!("{:7.2}", start.elapsed().as_secs_f32());

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = stop.clone();
    ctrlc::set_handler(move || flag.store(true, std::sync::atomic::Ordering::SeqCst))?;

    let commands = engine.state.startup();
    for c in &commands {
        dev.request(c.category, c.opcode, &c.body)?;
    }
    println!("{} applied profile ({} commands); listening for buttons and faders", stamp(), commands.len());

    // A device error ends the loop, but what changed until then is still saved below.
    let result = listen(&mut dev, &mut engine, limit, &stop, &stamp);
    println!("{} stopped", stamp());

    // Like the official app, keep the profile in step with the hardware: write back volumes and
    // mute states. Only those attributes change; a backup of the previous file is kept first.
    let backups = desktop::data_dir()?.join("backups");
    match profile_file::save_edits(profile.as_ref(), &engine.state.changed_edits(), &backups)? {
        Some(backup) => println!("{} saved changes to the profile (previous version: {})", stamp(), backup.display()),
        None => println!("{} profile unchanged, nothing saved", stamp()),
    }
    result
}

/// The `run` loop: reads the hardware, lets the engine react, and sends what it asks for.
fn listen(
    dev: &mut dcp::Device,
    engine: &mut engine::Engine,
    limit: Option<std::time::Duration>,
    stop: &std::sync::atomic::AtomicBool,
    stamp: &dyn Fn() -> String,
) -> Result<()> {
    let start = std::time::Instant::now();
    let mut last_raw = Vec::new();
    let mut last_poll = std::time::Instant::now() - std::time::Duration::from_secs(10);
    while limit.is_none_or(|l| start.elapsed() < l) && !stop.load(std::sync::atomic::Ordering::SeqCst) {
        let changed = dev.wait_input(25)?;
        let now = std::time::Instant::now();
        let mut out = Vec::new();
        // Read on every driver signal, plus a safety poll: a button press can arrive without one.
        if changed || now.duration_since(last_poll) >= std::time::Duration::from_millis(250) {
            last_poll = now;
            let raw = dev.request(0x800, 0, &[])?.body;
            if raw != last_raw {
                println!("{} raw state {}{}", stamp(), dcp::hex(&raw), if changed { "" } else { " (poll)" });
                last_raw = raw.clone();
            }
            out.extend(engine.on_input(&raw, now));
        }
        out.extend(engine.tick(now));
        for line in engine.events.drain(..) {
            println!("{} {line}", stamp());
        }
        for c in &out {
            dev.request(c.category, c.opcode, &c.body)?;
        }
        if !out.is_empty() {
            let sent: Vec<String> = out.iter().map(|c| format!("{:x}/{:x}", c.category, c.opcode)).collect();
            println!("{} sent {}", stamp(), sent.join(" "));
        }
    }
    Ok(())
}

/// Sends the whole startup state push built from a profile, like the official app does on connect.
fn apply(args: &[String]) -> Result<()> {
    let [profile, mic] = args else { bail!("apply needs <profile.goxlr> <mic.goxlrMicProfile>") };
    let commands = state::State::load(profile.as_ref(), mic.as_ref())?.startup();
    let api = tusb::Api::load()?;
    let mut dev = open_device(&api)?;
    let started = std::time::Instant::now();
    for (i, c) in commands.iter().enumerate() {
        dev.request(c.category, c.opcode, &c.body)
            .with_context(|| format!("command {} of {} ({:#x}/{:#x})", i + 1, commands.len(), c.category, c.opcode))?;
    }
    println!("applied {} commands in {:.2} s", commands.len(), started.elapsed().as_secs_f32());
    Ok(())
}

/// Writes one main-bank channel volume (0x806, device channel ids).
fn volume(args: &[String]) -> Result<()> {
    let [id, value] = args else { bail!("volume needs <channel id> <value>") };
    let (id, value): (u16, u8) = (id.parse()?, value.parse()?);
    if id > 10 {
        bail!("channel id {id} out of range (0-10)");
    }
    let api = tusb::Api::load()?;
    let mut dev = open_device(&api)?;
    let reply = dev.request(0x806, id, &[value])?;
    println!("0x806/{id} = {value}: accepted (reply body {} bytes)", reply.body.len());
    Ok(())
}

/// Offline: builds the startup state push from a profile. Sends nothing to the device.
fn plan(args: &[String]) -> Result<()> {
    let [profile, mic, rest @ ..] = args else { bail!("plan needs <profile.goxlr> <mic.goxlrMicProfile>") };
    let commands = state::State::load(profile.as_ref(), mic.as_ref())?.startup();
    match rest.first() {
        Some(capture) => {
            let captured = compare::load_capture(capture.as_ref())?;
            if compare::report(&commands, &captured) > 0 {
                std::process::exit(1);
            }
        }
        None => {
            for c in &commands {
                println!("{:#05x} {:#05x} {:4} {}", c.category, c.opcode, c.body.len(), dcp::hex(&c.body));
            }
        }
    }
    Ok(())
}

/// Prints the Windows peak levels of the GoXLR playback channels for 3 seconds (no USB).
fn meters() -> Result<()> {
    let m = meters::Meters::open()?;
    for _ in 0..6 {
        let levels = m.read().unwrap_or_default();
        println!("{}", levels.iter().map(|(n, db)| format!("{n} {db:6.1}")).collect::<Vec<_>>().join("  "));
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    Ok(())
}

/// Offline: prints the JSON the window renders from (used to preview the UI in a browser).
fn snapshot(args: &[String]) -> Result<()> {
    let [p, m] = args else { bail!("snapshot <profile> <mic>") };
    let engine = engine::Engine::new(state::State::load(p.as_ref(), m.as_ref())?);
    println!("{}", control::snapshot(&engine));
    Ok(())
}

/// Records the dry and processed mic through the GoXLR ASIO driver (nothing is sent to the Mini)
/// into <dir>/asio_in20.wav and asio_in6.wav, for `autotune-wav`.
fn record(args: &[String]) -> Result<()> {
    let [secs, dir] = args else { bail!("record <seconds> <dir>") };
    let rec = asio::Recorder::start()?;
    println!("recording at {} Hz", rec.sample_rate);
    let end = std::time::Instant::now() + std::time::Duration::from_secs_f64(secs.parse()?);
    while std::time::Instant::now() < end {
        std::thread::sleep(std::time::Duration::from_millis(500));
        println!("{:6.1} dB peak, {} samples", rec.take_peak_db(), rec.len());
    }
    let take = rec.finish()?;
    let dir = std::path::Path::new(dir);
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join("asio_in20.wav"), autotune::wav(&take.dry, take.sample_rate, 1.0))?;
    std::fs::write(dir.join("asio_in6.wav"), autotune::wav(&take.processed, take.sample_rate, 1.0))?;
    println!("{} dry and {} processed samples", take.dry.len(), take.processed.len());
    Ok(())
}

/// Offline: runs the mic auto-tune analysis on a take from `record` (asio_in20.wav = dry,
/// asio_in6.wav = processed), with the first `silence` seconds as the silent step and the rest as
/// reading. The profiles give the settings the take was recorded with. Prints the measurements and
/// the proposal.
fn autotune_wav(args: &[String]) -> Result<()> {
    let [dir, silence, profile, mic] = args else {
        bail!("autotune-wav <dir> <silence secs> <profile.goxlr> <mic.goxlrMicProfile>")
    };
    let dir = std::path::Path::new(dir);
    let (dry, rate) = read_wav(&dir.join("asio_in20.wav"))?;
    let (processed, _) = read_wav(&dir.join("asio_in6.wav"))?;
    let split = ((silence.parse::<f64>()? * rate) as usize).min(dry.len());
    let segments = [
        autotune::Segment { key: "silence", start: 0, end: split },
        autotune::Segment { key: "read", start: split, end: dry.len() },
    ];
    let analysis = autotune::analyse(&dry, &processed, rate, &segments, true).map_err(|e| anyhow::anyhow!(e))?;
    println!("{:#}", analysis.to_json());
    let cur = control::mic_settings(&engine::Engine::new(state::State::load(profile.as_ref(), mic.as_ref())?));
    let p = autotune::propose(&analysis, &cur, true);
    println!(
        "{:#}",
        serde_json::json!({ "settings": p.settings.to_json(), "changes": p.changes, "notes": p.notes, "discord": autotune::discord_sensitivity(&analysis) })
    );
    Ok(())
}

/// Reads a 16-bit mono PCM WAV with the plain 44-byte header `autotune::wav` writes.
/// Returns the samples and the sample rate.
fn read_wav(path: &std::path::Path) -> Result<(Vec<f32>, f64)> {
    let b = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let u16_at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
    if b.len() < 44 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" || u16_at(22) != 1 || u16_at(34) != 16 {
        bail!("{} isn't a 16-bit mono WAV", path.display());
    }
    let rate = u32::from_le_bytes(b[24..28].try_into()?);
    let samples = b[44..].as_chunks::<2>().0.iter().map(|c| f32::from(i16::from_le_bytes([c[0], c[1]])) / 32768.0).collect();
    Ok((samples, f64::from(rate)))
}

/// Opens the Mini, unless the official app has it (only one program can control the device).
fn open_device(api: &tusb::Api) -> Result<dcp::Device<'_>> {
    if official_app_running() {
        bail!("the official GoXLR App is running; close it first (only one program can control the device)");
    }
    dcp::Device::open_mini(api)
}

pub fn official_app_running() -> bool {
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return false;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut found = false;
        let mut ok = Process32FirstW(snap, &mut entry);
        while ok != 0 {
            let end = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
            if String::from_utf16_lossy(&entry.szExeFile[..end]).eq_ignore_ascii_case("GoXLR App.exe") {
                found = true;
                break;
            }
            ok = Process32NextW(snap, &mut entry);
        }
        CloseHandle(snap);
        found
    }
}

/// Release builds have no console of their own: print to the terminal that started us, if any.
fn attach_parent_console() {
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}

/// When started from Explorer the console window belongs to us alone: hide it so only the app
/// window shows. When started from a terminal, leave the terminal alone.
fn hide_own_console() {
    use windows_sys::Win32::System::Console::{FreeConsole, GetConsoleProcessList};
    let mut pids = [0u32; 2];
    if unsafe { GetConsoleProcessList(pids.as_mut_ptr(), 2) } == 1 {
        unsafe { FreeConsole() };
    }
}
