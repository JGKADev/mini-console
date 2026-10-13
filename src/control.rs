//! Commands from the window: applies a JSON request to the engine's profile and returns the DCP
//! commands to send. Also builds the JSON snapshot the window renders from.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::engine::{CoughEvent, Engine};
use crate::profile::{
    ANIMATIONS, Channel, DSP, FADERS, Fader, FaderDisplay, INPUTS, Input, Leds, MINI_EQ, Mix, MuteFunction, Output, Rgb, SETUP,
};
use crate::state::Command;

const CHANNELS: [(&str, Channel); 11] = [
    ("mic", Channel::Mic),
    ("lineIn", Channel::LineIn),
    ("console", Channel::Console),
    ("system", Channel::System),
    ("game", Channel::Game),
    ("chat", Channel::Chat),
    ("sample", Channel::Sample),
    ("music", Channel::Music),
    ("headphones", Channel::Headphones),
    ("micMonitor", Channel::MicMonitor),
    ("lineOut", Channel::LineOut),
];

const OUTPUT_NAMES: [(&str, Output); 5] = [
    ("hp", Output::Headphones),
    ("stream", Output::Stream),
    ("chat", Output::Chat),
    ("sampler", Output::Sampler),
    ("lineOut", Output::LineOut),
];

const STYLES: [(&str, FaderDisplay); 4] = [
    ("two", FaderDisplay::TwoColour),
    ("grad", FaderDisplay::Gradient),
    ("meter", FaderDisplay::Meter),
    ("gradmeter", FaderDisplay::GradientMeter),
];

fn channel(name: &str) -> Result<Channel> {
    CHANNELS.iter().find(|(n, _)| *n == name).map(|(_, c)| *c).with_context(|| format!("unknown channel {name}"))
}

fn channel_name(c: Channel) -> &'static str {
    CHANNELS.iter().find(|(_, x)| *x == c).map(|(n, _)| *n).unwrap_or("?")
}

fn input(name: &str) -> Result<Input> {
    INPUTS.into_iter().find(|i| i.name() == name).with_context(|| format!("unknown input {name}"))
}

fn output(name: &str) -> Result<Output> {
    OUTPUT_NAMES.iter().find(|(n, _)| *n == name).map(|(_, o)| *o).with_context(|| format!("unknown output {name}"))
}

fn fader(v: &Value) -> Result<Fader> {
    let i = v["fader"].as_u64().context("missing fader")? as usize;
    FADERS.get(i).copied().with_context(|| format!("fader {i} out of range"))
}

fn int(v: &Value, key: &str) -> Result<i64> {
    v[key].as_i64().with_context(|| format!("missing number {key}"))
}

/// Checks a mic setting against its allowed range before it's stored.
fn in_range(name: &str, value: i64, lo: i64, hi: i64) -> Result<String> {
    if !(lo..=hi).contains(&value) {
        bail!("{name} must be between {lo} and {hi}");
    }
    Ok(value.to_string())
}

fn rgb(c: Rgb) -> String {
    format!("#{}", c.hex().to_lowercase())
}

/// Applies one request from the window. Returns the commands to send to the device.
pub fn apply(engine: &mut Engine, v: &Value) -> Result<Vec<Command>> {
    let cmd = v["cmd"].as_str().context("missing cmd")?;
    let state = &mut engine.state;
    Ok(match cmd {
        "volume" => {
            let c = channel(v["channel"].as_str().unwrap_or_default())?;
            let value = int(v, "value")?.clamp(0, 255) as u8;
            state.set_volume(c, value)
        }
        "submix" => state.set_submix_enabled(v["on"].as_bool().unwrap_or(false)),
        "subVolume" => {
            let i = input(v["channel"].as_str().unwrap_or_default())?;
            state.set_sub_volume(i, int(v, "value")?.clamp(0, 255) as u8)
        }
        "subLink" => {
            let i = input(v["channel"].as_str().unwrap_or_default())?;
            state.set_sub_linked(i, v["on"].as_bool().unwrap_or(false));
            Vec::new()
        }
        "outputMix" => {
            let o = output(v["output"].as_str().unwrap_or_default())?;
            let mix = match v["mix"].as_str() {
                Some("A") => Mix::A,
                Some("B") => Mix::B,
                other => bail!("unknown mix {other:?}"),
            };
            state.set_output_mix(o, mix)
        }
        "mute" => {
            let f = fader(v)?;
            engine.fader_mute(f, v["hold"].as_bool().unwrap_or(false))
        }
        "cough" => engine.cough(if v["down"].as_bool().unwrap_or(false) {
            CoughEvent::Press
        } else {
            CoughEvent::Release { hold_handled: false }
        }),
        "bleep" => {
            engine.set_bleep(v["down"].as_bool().unwrap_or(false));
            vec![engine.state.button_leds()]
        }
        "assign" => {
            let f = fader(v)?;
            let c = channel(v["channel"].as_str().unwrap_or_default())?;
            let current = state.fader_channel(f);
            // If the channel is already on another fader, the two faders swap channels.
            if let Some(other) = FADERS.into_iter().find(|&o| o != f && state.fader_channel(o) == c) {
                state.profile_mut().set_fader_channel(other, current);
            }
            state.profile_mut().set_fader_channel(f, c);
            let mut out = state.fader_assignments();
            out.extend(state.channel_mutes());
            out.extend(state.routing());
            out.push(state.button_leds());
            out
        }
        "route" => {
            let i = input(v["input"].as_str().unwrap_or_default())?;
            let o = output(v["output"].as_str().unwrap_or_default())?;
            if (i, o) == (Input::Chat, Output::Chat) {
                bail!("chat can't be routed back to voice chat");
            }
            let on = v["on"].as_bool().unwrap_or(false);
            state.profile_mut().set_route(i, o, if on { 8192 } else { 0 });
            state.routing_rows(i)
        }
        "micType" => {
            let t = int(v, "value")?.clamp(0, 2);
            state.mic_mut().set(SETUP, "MIC_TYPE", t.to_string());
            state.mic_commands()
        }
        "micGain" => {
            let g = int(v, "value")?.clamp(0, 72) as u16;
            let t = state.mic().mic_type();
            state.mic_mut().set_gain(t, g);
            state.mic_commands()
        }
        "eq" => {
            let band = int(v, "band")?;
            let Some(&(stem, _, lo, hi)) = usize::try_from(band).ok().and_then(|b| MINI_EQ.get(b)) else {
                bail!("eq band {band} out of range");
            };
            if let Some(f) = v["freq"].as_f64() {
                let f = f as f32;
                if !(lo..=hi).contains(&f) {
                    bail!("{stem} band frequency must be between {lo} and {hi} Hz");
                }
                state.mic_mut().set(DSP, &format!("MIC_MINI_EQ_{stem}_F"), f.to_string());
            }
            // Gains clamp (the EQ drag can overshoot); a frequency out of range is an error.
            if let Some(g) = v["gain"].as_i64() {
                state.mic_mut().set(DSP, &format!("MIC_MINI_EQ_{stem}_GAIN"), g.clamp(-9, 9).to_string());
            }
            state.mic_commands()
        }
        "gate" => {
            // Each value is checked and stored in turn (an out-of-range value stops there).
            for (key, attr, lo, hi) in [
                ("threshold", "MIC_GATE_THRESOLD", -59, 0),
                ("atten", "MIC_GATE_ATTEN", 0, 100),
                ("attack", "MIC_GATE_ATTACK", 0, 45),
                ("release", "MIC_GATE_RELEASE", 0, 45),
            ] {
                if let Some(x) = v[key].as_i64() {
                    let value = in_range(key, x, lo, hi)?;
                    state.mic_mut().set(DSP, attr, value);
                }
            }
            state.mic_commands()
        }
        "comp" => {
            for (key, attr, lo, hi) in [
                ("threshold", "MIC_COMP_THRESHOLD", -40, 0),
                ("ratio", "MIC_COMP_RATIO", 0, 14),
                ("attack", "MIC_COMP_ATTACK", 0, 19),
                ("release", "MIC_COMP_RELEASE", 0, 19),
                ("makeup", "MIC_COMP_MAKEUPGAIN", -6, 24),
            ] {
                if let Some(x) = v[key].as_i64() {
                    let value = in_range(key, x, lo, hi)?;
                    state.mic_mut().set(DSP, attr, value);
                }
            }
            state.mic_commands()
        }
        "deess" => {
            state.mic_mut().set(DSP, "MIC_DEESS_AMOUNT", int(v, "value")?.clamp(0, 100).to_string());
            state.mic_commands()
        }
        "bleepLevel" => {
            state.set_bleep_level(int(v, "value")?.clamp(-36, 0) as i8);
            state.mic_commands()
        }
        "colour" => {
            let colour = Rgb::parse_rgb(v["rgb"].as_str().unwrap_or_default())?;
            let index = int(v, "index")?.clamp(0, 1) as u8;
            let target = v["target"].as_str().unwrap_or_default();
            let p = state.profile_mut();
            match target {
                "fader0" | "fader1" | "fader2" | "fader3" => {
                    let f = FADERS[target[5..].parse::<usize>()?];
                    let leds = p.fader_leds(f);
                    p.set_colour(&leds, index, colour);
                }
                "mute" => {
                    for f in FADERS {
                        let leds = p.mute_leds(f);
                        p.set_colour(&leds, index, colour);
                    }
                }
                "cough" => p.set_colour(&Leds::new("muteChat"), 0, colour),
                "bleep" => p.set_colour(&Leds::new("swear"), 0, colour),
                "logo" => p.set_colour(&Leds::new("logoX"), 0, colour),
                _ => bail!("unknown colour target {target}"),
            }
            vec![state.colour_map(), state.button_leds()]
        }
        "faderStyle" => {
            let f = fader(v)?;
            let name = v["style"].as_str().unwrap_or_default();
            let style =
                STYLES.iter().find(|(n, _)| *n == name).map(|(_, s)| *s).with_context(|| format!("unknown style {name}"))?;
            state.profile_mut().set_fader_display(f, style);
            state.fader_displays()
        }
        "animation" => {
            let mode = *ANIMATIONS.get(int(v, "value")? as usize).context("animation out of range")?;
            state.profile_mut().set_animation(mode);
            vec![state.animation(), state.colour_map()]
        }
        other => bail!("unknown command {other}"),
    })
}

/// The mic settings the auto-tune works with, read from the loaded mic profile.
pub fn mic_settings(engine: &Engine) -> crate::autotune::MicSettings {
    let mic = engine.state.mic();
    let d = |k: &str| mic.dsp(k, 0);
    crate::autotune::MicSettings {
        gain: i32::from(mic.gain(mic.mic_type())),
        eq: std::array::from_fn(|b| {
            let (f, g) = mic.mini_eq(b);
            (f, i32::from(g))
        }),
        gate: [d("MIC_GATE_THRESOLD"), d("MIC_GATE_ATTEN"), d("MIC_GATE_ATTACK"), d("MIC_GATE_RELEASE")],
        comp: [
            d("MIC_COMP_THRESHOLD"),
            d("MIC_COMP_RATIO"),
            d("MIC_COMP_ATTACK"),
            d("MIC_COMP_RELEASE"),
            d("MIC_COMP_MAKEUPGAIN"),
        ],
        deess: d("MIC_DEESS_AMOUNT"),
    }
}

/// Everything the window shows, as JSON.
pub fn snapshot(engine: &Engine) -> Value {
    let state = &engine.state;
    let p = state.profile();
    let colours = |leds: &Leds| (0..2).map(|i| rgb(p.colour(leds, i))).collect::<Vec<_>>();
    let volumes: serde_json::Map<String, Value> = CHANNELS.iter().map(|(n, c)| (n.to_string(), json!(p.volume(*c)))).collect();
    let faders: Vec<Value> = FADERS
        .iter()
        .map(|&f| {
            let (x, all, function) = state.mute_state(f);
            let display = p.fader_display(f);
            let style = STYLES.iter().find(|(_, d)| *d == display).map_or("two", |(n, _)| n);
            json!({
                "channel": channel_name(state.fader_channel(f)),
                "muted": x || all, "mutedToAll": all || (x && function == MuteFunction::All),
                "blink": all, "function": function.name(),
                "colours": colours(&p.fader_leds(f)), "style": style,
            })
        })
        .collect();
    let mut routes = serde_json::Map::new();
    let mut held = serde_json::Map::new();
    let live = state.routing();
    for i in INPUTS {
        let row =
            live.iter().find(|c| c.opcode as usize == crate::state::input_row(i)).map(|c| c.body.clone()).unwrap_or_default();
        let cell = |col: usize| row.get(col * 2..col * 2 + 2).map(|b| u16::from_le_bytes([b[0], b[1]])).unwrap_or(0);
        let mut r = serde_json::Map::new();
        let mut h = serde_json::Map::new();
        for (n, o) in OUTPUT_NAMES {
            let profile_on = p.route(i, o) > 0;
            r.insert(n.into(), json!(profile_on));
            h.insert(n.into(), json!(profile_on && cell(crate::state::output_col(o)) == 0));
        }
        routes.insert(i.name().into(), Value::Object(r));
        held.insert(i.name().into(), Value::Object(h));
    }
    let mic = state.mic();
    let d = |k: &str| mic.dsp(k, 0);
    let (cough_on, cough_blink, _) = state.cough_state();
    json!({
        "volumes": volumes,
        "faders": faders,
        "positions": engine.fader_positions(),
        "cough": { "on": cough_on, "blink": cough_blink, "toggle": p.cough_toggle() },
        "bleep": p.lit(&Leds::new("swear")),
        "micMuted": state.channel_muted(Channel::Mic),
        "routes": routes,
        "held": held,
        "mic": {
            "type": mic.mic_type(),
            "gain": [mic.gain(0), mic.gain(1), mic.gain(2)],
            "eq": (0..6).map(|b| { let (f, g) = mic.mini_eq(b); json!([f, g]) }).collect::<Vec<_>>(),
            "gate": { "threshold": d("MIC_GATE_THRESOLD"), "atten": d("MIC_GATE_ATTEN"), "attack": d("MIC_GATE_ATTACK"), "release": d("MIC_GATE_RELEASE") },
            "comp": { "threshold": d("MIC_COMP_THRESHOLD"), "ratio": d("MIC_COMP_RATIO"), "attack": d("MIC_COMP_ATTACK"), "release": d("MIC_COMP_RELEASE"), "makeup": d("MIC_COMP_MAKEUPGAIN") },
            "deess": d("MIC_DEESS_AMOUNT"),
            "bleep": state.bleep_level(),
        },
        "colours": {
            "mute": colours(&p.mute_leds(Fader::A)),
            "cough": rgb(p.colour(&Leds::new("muteChat"), 0)),
            "bleep": rgb(p.colour(&Leds::new("swear"), 0)),
            "logo": rgb(p.colour(&Leds::new("logoX"), 0)),
        },
        "submix": {
            "on": state.submix_enabled(),
            "volumes": INPUTS.iter().map(|&i| (i.name().to_string(), json!(state.sub_volume(i)))).collect::<serde_json::Map<_, _>>(),
            "linked": INPUTS.iter().map(|&i| (i.name().to_string(), json!(state.sub_linked(i)))).collect::<serde_json::Map<_, _>>(),
            "outputs": OUTPUT_NAMES.iter().map(|&(n, o)| (n.to_string(), json!(if state.output_mix(o) == Mix::B { "B" } else { "A" }))).collect::<serde_json::Map<_, _>>(),
        },
        "animation": ANIMATIONS.iter().position(|m| *m == p.animation()).unwrap_or(5),
        "unsaved": !state.changed_edits().is_empty() || !state.changed_mic_edits().is_empty(),
    })
}
