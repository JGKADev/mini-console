//! Behaviour recorder: prints everything the app derives from a profile + mic profile, then runs a fixed
//! script of window commands and hardware readings and prints what each one sends and changes.
//! Two builds that print the same text behave the same. Used to check refactors (`golden <profile> <mic>`).

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{Value, json};

use crate::engine::Engine;
use crate::state::{Command, State};

fn hex(cmds: &[Command]) -> String {
    let mut s = String::new();
    for c in cmds {
        let _ = writeln!(s, "  {:03x}/{:02x} {}", c.category, c.opcode, crate::dcp::hex(&c.body));
    }
    s
}

fn edits(e: &Engine) -> String {
    let mut s = String::new();
    for (el, at, v) in e.state.changed_edits() {
        let _ = writeln!(s, "  edit {el} {at}={v}");
    }
    for (el, at, v) in e.state.changed_mic_edits() {
        let _ = writeln!(s, "  mic {el} {at}={v}");
    }
    s
}

fn script() -> Vec<Value> {
    let mut v = Vec::new();
    for ch in ["mic", "lineIn", "console", "system", "game", "chat", "sample", "music", "headphones", "micMonitor", "lineOut"] {
        v.push(json!({"cmd": "volume", "channel": ch, "value": 37}));
        v.push(json!({"cmd": "volume", "channel": ch, "value": 255}));
    }
    for f in 0..4 {
        for hold in [false, false, true, false] {
            v.push(json!({"cmd": "mute", "fader": f, "hold": hold}));
        }
    }
    for down in [true, false, true, false] {
        v.push(json!({"cmd": "cough", "down": down}));
        v.push(json!({"cmd": "bleep", "down": down}));
    }
    for (f, ch) in [(0, "music"), (1, "mic"), (2, "headphones"), (3, "lineOut"), (0, "sample"), (3, "system")] {
        v.push(json!({"cmd": "assign", "fader": f, "channel": ch}));
    }
    for i in ["mic", "chat", "music", "game", "console", "lineIn", "system", "sample"] {
        for o in ["hp", "stream", "chat", "sampler", "lineOut"] {
            v.push(json!({"cmd": "route", "input": i, "output": o, "on": false}));
            v.push(json!({"cmd": "route", "input": i, "output": o, "on": true}));
        }
    }
    for t in [1, 2, 0] {
        v.push(json!({"cmd": "micType", "value": t}));
        v.push(json!({"cmd": "micGain", "value": 20 + t * 10}));
    }
    for b in 0..6 {
        v.push(json!({"cmd": "eq", "band": b, "freq": 100.0 + 700.0 * f64::from(b), "gain": b - 3}));
    }
    for (b, f) in [60.0, 200.0, 650.0, 1600.0, 3900.0, 9500.0].into_iter().enumerate() {
        v.push(json!({"cmd": "eq", "band": b, "freq": f, "gain": 9 - 3 * b as i64}));
        v.push(json!({"cmd": "eq", "band": b, "gain": -12}));
    }
    for (b, f) in [30.0, 300.0, 800.0, 2500.0, 5000.0, 18000.0].into_iter().enumerate() {
        v.push(json!({"cmd": "eq", "band": b, "freq": f}));
    }
    v.push(json!({"cmd": "gate", "threshold": -40, "atten": 45, "attack": 3, "release": 9}));
    v.push(json!({"cmd": "gate", "atten": 100}));
    v.push(json!({"cmd": "gate", "atten": 0}));
    for r in [0, 5, 14] {
        v.push(json!({"cmd": "comp", "threshold": -20, "ratio": r, "attack": 4, "release": 6, "makeup": 3}));
    }
    v.push(json!({"cmd": "deess", "value": 33}));
    v.push(json!({"cmd": "bleepLevel", "value": -20}));
    for (t, i) in [("fader0", 0), ("fader2", 1), ("mute", 0), ("mute", 1), ("cough", 0), ("bleep", 0), ("logo", 0)] {
        v.push(json!({"cmd": "colour", "target": t, "index": i, "rgb": "#12ab34"}));
    }
    for (f, s) in [(0, "two"), (1, "grad"), (2, "meter"), (3, "gradmeter")] {
        v.push(json!({"cmd": "faderStyle", "fader": f, "style": s}));
    }
    for a in 0..6 {
        v.push(json!({"cmd": "animation", "value": a}));
    }
    v.push(json!({"cmd": "submix", "on": true}));
    for ch in ["mic", "music", "sample"] {
        v.push(json!({"cmd": "subVolume", "channel": ch, "value": 90}));
        v.push(json!({"cmd": "subLink", "channel": ch, "on": false}));
        v.push(json!({"cmd": "subVolume", "channel": ch, "value": 60}));
        v.push(json!({"cmd": "subLink", "channel": ch, "on": true}));
        v.push(json!({"cmd": "volume", "channel": ch, "value": 120}));
    }
    for o in ["hp", "stream", "chat", "sampler", "lineOut"] {
        v.push(json!({"cmd": "outputMix", "output": o, "mix": "B"}));
    }
    v.push(json!({"cmd": "outputMix", "output": "chat", "mix": "A"}));
    v.push(json!({"cmd": "submix", "on": false}));
    v
}

/// Hardware readings: (milliseconds after start, button mask, fader bytes).
const READINGS: [(u64, u32, [u8; 4]); 14] = [
    (0, 0, [0, 0, 0, 0]),
    (10, 0, [100, 0, 0, 0]),
    (20, 1 << 4, [100, 0, 0, 0]),
    (60, 0, [100, 0, 0, 0]),
    (80, 1 << 9, [100, 50, 0, 0]),
    (700, 1 << 9, [100, 50, 0, 0]),
    (720, 0, [100, 50, 0, 0]),
    (800, 1 << 22, [100, 50, 7, 0]),
    (900, 0, [100, 50, 7, 255]),
    (1000, 1 << 23, [0, 0, 0, 0]),
    (1100, 0, [0, 0, 0, 0]),
    (1200, 1 << 23, [0, 0, 0, 0]),
    (1900, 1 << 23, [0, 0, 0, 0]),
    (1950, 0, [1, 2, 3, 4]),
];

pub fn run(profile: &str, mic: &str) -> Result<()> {
    let fresh = || -> Result<Engine> { Ok(Engine::new(State::load(profile.as_ref(), mic.as_ref())?)) };
    let e = fresh()?;
    println!("== startup\n{}", hex(&e.state.startup()));
    println!("== snapshot\n{}", crate::control::snapshot(&e));
    println!("== edits");
    for (el, at, v) in e.state.edits() {
        println!("  {el} {at}={v}");
    }
    for (el, at, v) in e.state.mic_edits() {
        println!("  mic {el} {at}={v}");
    }
    println!("== mic settings {:?}", crate::control::mic_settings(&e));

    // Each command on a fresh engine, then the whole script on one engine.
    for (n, c) in script().iter().enumerate() {
        let mut e = fresh()?;
        let out = crate::control::apply(&mut e, c);
        println!("== single {n} {c}");
        match out {
            Ok(cmds) => print!("{}{}", hex(&cmds), edits(&e)),
            Err(err) => println!("  error {err:#}"),
        }
    }
    let mut e = fresh()?;
    for (n, c) in script().iter().enumerate() {
        let out = crate::control::apply(&mut e, c);
        println!("== chain {n} {c}");
        match out {
            Ok(cmds) => print!("{}", hex(&cmds)),
            Err(err) => println!("  error {err:#}"),
        }
    }
    println!("== chain end\n{}== chain snapshot\n{}", edits(&e), crate::control::snapshot(&e));

    let mut e = fresh()?;
    let t0 = Instant::now();
    for (ms, mask, faders) in READINGS {
        let now = t0 + Duration::from_millis(ms);
        let mut r = mask.to_le_bytes().to_vec();
        r.extend_from_slice(&[0; 4]);
        r.extend_from_slice(&faders);
        let mut out = e.on_input(&r, now);
        out.extend(e.tick(now));
        println!("== hw {ms} {mask:#x} {faders:?}\n{}  events {:?}", hex(&out), std::mem::take(&mut e.events));
    }
    println!("== hw end\n{}", edits(&e));
    Ok(())
}
