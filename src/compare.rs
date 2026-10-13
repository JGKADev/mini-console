//! Offline check: compares a generated state push against a captured startup of the official app
//! (a JSONL capture, one JSON object per USB transfer).
//!
//! Commands are compared by the device state they leave behind, not by order: for each
//! (category, opcode) the last body sent wins. Key/value lists (0x801, 0x80b) are merged per key,
//! because the official app sends some parameters again on their own.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};

use crate::state::Command;

/// Key of one piece of device state: (category, opcode, parameter key for key/value lists).
type Slot = (u16, u16, Option<u32>);

fn is_key_value(category: u16) -> bool {
    matches!(category, 0x801 | 0x80b)
}

/// Writes that set device state. Reads (0x800, 0x80c, 0x80f), DICE categories and scribbles
/// (0x802: the Mini has no scribble screens, so we never send them) are ignored.
fn is_state_write(category: u16) -> bool {
    category >= 0x801 && !matches!(category, 0x802 | 0x80c | 0x80f)
}

fn final_state<'a>(commands: impl IntoIterator<Item = &'a Command>) -> BTreeMap<Slot, Vec<u8>> {
    let mut state = BTreeMap::new();
    for c in commands {
        if !is_state_write(c.category) {
            continue;
        }
        if is_key_value(c.category) {
            for pair in c.body.as_chunks::<8>().0 {
                let key = u32::from_le_bytes(pair[..4].try_into().unwrap());
                state.insert((c.category, c.opcode, Some(key)), pair[4..].to_vec());
            }
        } else {
            state.insert((c.category, c.opcode, None), c.body.clone());
        }
    }
    state
}

/// Reads the host-to-device DCP packets (bRequest 2) from a capture file.
pub fn load_capture(path: &Path) -> Result<Vec<Command>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut out = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let v: serde_json::Value = serde_json::from_str(line)?;
        if v["kind"] != "out" || v["bReq"] != 2 {
            continue;
        }
        let data = hex(v["data"].as_str().unwrap_or_default())?;
        if data.len() < 16 {
            continue;
        }
        let word = u32::from_le_bytes(data[..4].try_into()?);
        let len = u16::from_le_bytes(data[4..6].try_into()?) as usize;
        out.push(Command {
            category: (word >> 12) as u16,
            opcode: (word & 0xfff) as u16,
            body: data[16..16 + len.min(data.len() - 16)].to_vec(),
        });
    }
    Ok(out)
}

fn hex(s: &str) -> Result<Vec<u8>> {
    anyhow::ensure!(s.is_ascii() && s.len().is_multiple_of(2), "bad hex data {s:?}");
    (0..s.len()).step_by(2).map(|i| Ok(u8::from_str_radix(&s[i..i + 2], 16)?)).collect()
}

/// Prints every difference and returns how many slots differ.
pub fn report(generated: &[Command], captured: &[Command]) -> usize {
    let ours = final_state(generated);
    let theirs = final_state(captured);
    let mut slots: Vec<&Slot> = ours.keys().chain(theirs.keys()).collect();
    slots.sort();
    slots.dedup();

    let mut diffs = 0;
    for &slot in &slots {
        let (a, b) = (ours.get(slot), theirs.get(slot));
        if a == b {
            continue;
        }
        diffs += 1;
        let (cat, op, key) = *slot;
        let name = match key {
            Some(k) => format!("{cat:#x}/{op} key {k:#x}"),
            None => format!("{cat:#x}/{op:#x}"),
        };
        match (a, b) {
            (Some(a), Some(b)) if a.len() == b.len() && a.len() > 32 => {
                println!("DIFF {name} ({} bytes):", a.len());
                for (i, (x, y)) in a.chunks(4).zip(b.chunks(4)).enumerate() {
                    if x != y {
                        println!(
                            "    word {i:3} (byte {:3}): ours {} official {}",
                            i * 4,
                            crate::dcp::hex(x),
                            crate::dcp::hex(y)
                        );
                    }
                }
            }
            _ => println!(
                "DIFF {name}: ours {} official {}",
                a.map_or("(not sent)".into(), |v| crate::dcp::hex(v)),
                b.map_or("(not sent)".into(), |v| crate::dcp::hex(v))
            ),
        }
    }
    println!("{} state slots compared, {diffs} differ", slots.len());
    diffs
}
