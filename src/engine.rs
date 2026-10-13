//! Hardware input handling: turns 0x800 state readings (buttons and faders) into profile changes
//! and the DCP commands that follow from them. Pure: the caller sends the commands.
//!
//! Button behaviour matches the official app: a tap mutes with the button's function, holding for
//! half a second mutes to everything (and blinks), the cough button mutes while held or toggles.

use std::time::{Duration, Instant};

use crate::profile::{Channel, FADERS, Fader, Input, Leds, MuteFunction};
use crate::state::{Command, State};

/// Holding a mute button this long mutes to all.
pub const HOLD: Duration = Duration::from_millis(500);

/// Button bit numbers in the 0x800 reply, Mini buttons only.
const FADER_MUTE_BITS: [u32; 4] = [4, 9, 14, 19];
const BLEEP_BIT: u32 = 22;
const COUGH_BIT: u32 = 23;
const MINI_BUTTONS: [u32; 6] = [4, 9, 14, 19, BLEEP_BIT, COUGH_BIT];

pub struct Engine {
    pub state: State,
    buttons: u32,
    faders_seen: [u8; 4],
    pressed_at: [Option<Instant>; 32],
    hold_handled: [bool; 32],
    /// Human-readable lines describing what happened, drained by the caller.
    pub events: Vec<String>,
}

impl Engine {
    pub fn new(state: State) -> Self {
        Self { state, buttons: 0, faders_seen: [0; 4], pressed_at: [None; 32], hold_handled: [false; 32], events: Vec::new() }
    }

    /// Handles one 12-byte 0x800 reply: u32 button mask, 4 encoder bytes, 4 fader bytes.
    pub fn on_input(&mut self, reply: &[u8], now: Instant) -> Vec<Command> {
        let mut out = Vec::new();
        if reply.len() < 12 {
            self.events.push(format!("ignored short hardware state ({} bytes)", reply.len()));
            return out;
        }

        // Faders: the Mini applies the volume itself, so we only track it. A reading equal to the
        // last one is ignored (the Mini reports 0 for faders not yet moved).
        for (i, &fader) in FADERS.iter().enumerate() {
            let value = reply[8 + i];
            if value != self.faders_seen[i] {
                self.faders_seen[i] = value;
                let channel = self.state.fader_channel(fader);
                self.state.profile_mut().set_volume(channel, value);
                // A linked Mix B volume follows the fader (the Mini only moves the main volume).
                out.extend(self.state.follow_linked(channel));
                self.events.push(format!("fader {} ({}) → {}%", i + 1, name(channel), percent(value)));
            }
        }

        // The top two bits of the button word aren't buttons.
        let mask = u32::from_le_bytes(reply[0..4].try_into().unwrap()) & 0x3fff_ffff;
        for bit in MINI_BUTTONS {
            let was = self.buttons & (1 << bit) != 0;
            let is = mask & (1 << bit) != 0;
            if is && !was {
                self.pressed_at[bit as usize] = Some(now);
                self.hold_handled[bit as usize] = false;
                out.extend(self.on_down(bit));
            } else if was && !is {
                let held = self.hold_handled[bit as usize];
                self.pressed_at[bit as usize] = None;
                out.extend(self.on_up(bit, held));
            }
        }
        self.buttons = mask;
        out
    }

    /// Last fader positions reported by the hardware (0 = not picked up yet).
    pub fn fader_positions(&self) -> [u8; 4] {
        self.faders_seen
    }

    /// Call regularly: fires hold actions for buttons held down for `HOLD`.
    pub fn tick(&mut self, now: Instant) -> Vec<Command> {
        let mut out = Vec::new();
        for bit in MINI_BUTTONS {
            let b = bit as usize;
            if let Some(at) = self.pressed_at[b]
                && !self.hold_handled[b]
                && now.duration_since(at) >= HOLD
            {
                self.hold_handled[b] = true;
                out.extend(self.on_hold(bit));
            }
        }
        out
    }

    fn on_down(&mut self, bit: u32) -> Vec<Command> {
        match bit {
            COUGH_BIT => self.cough(CoughEvent::Press),
            BLEEP_BIT => {
                self.set_bleep(true);
                self.events.push("bleep pressed".into());
                vec![self.state.button_leds()]
            }
            _ => Vec::new(), // fader mutes act on release (or on hold)
        }
    }

    fn on_hold(&mut self, bit: u32) -> Vec<Command> {
        if let Some(f) = FADER_MUTE_BITS.iter().position(|&b| b == bit) {
            return self.fader_mute(FADERS[f], true);
        }
        if bit == COUGH_BIT {
            return self.cough(CoughEvent::Hold);
        }
        Vec::new()
    }

    fn on_up(&mut self, bit: u32, hold_handled: bool) -> Vec<Command> {
        if let Some(f) = FADER_MUTE_BITS.iter().position(|&b| b == bit) {
            return if hold_handled { Vec::new() } else { self.fader_mute(FADERS[f], false) };
        }
        match bit {
            COUGH_BIT => self.cough(CoughEvent::Release { hold_handled }),
            BLEEP_BIT => {
                self.set_bleep(false);
                self.events.push("bleep released".into());
                vec![self.state.button_leds()]
            }
            _ => Vec::new(),
        }
    }

    pub fn set_bleep(&mut self, on: bool) {
        self.state.profile_mut().set_lit(&Leds::new("swear"), on);
    }

    fn set_mute(&mut self, fader: Fader, on: bool, blink: bool) {
        let p = self.state.profile_mut();
        let leds = p.mute_leds(fader);
        p.set_lit(&leds, on);
        p.set_blinking(&leds, blink);
    }

    /// A fader's mute button was pressed (released before the hold time) or held.
    pub fn fader_mute(&mut self, fader: Fader, held: bool) -> Vec<Command> {
        let (muted_to_x, muted_to_all, function) = self.state.mute_state(fader);
        let channel = self.state.fader_channel(fader);
        if held {
            if muted_to_all {
                return Vec::new(); // holding while already muted to all does nothing
            }
            self.set_mute(fader, true, true);
            self.events.push(format!("{} muted to all (held)", name(channel)));
        } else if muted_to_x || muted_to_all {
            self.set_mute(fader, false, false);
            self.events.push(format!("{} unmuted", name(channel)));
        } else {
            self.set_mute(fader, true, false);
            match function {
                MuteFunction::All => self.events.push(format!("{} muted to all", name(channel))),
                f => self.events.push(format!("{} muted ({})", name(channel), f.name())),
            }
        }
        self.channel_changed(channel)
    }

    pub fn cough(&mut self, event: CoughEvent) -> Vec<Command> {
        let toggle = self.state.profile().cough_toggle();
        let (on, blink, _) = self.state.cough_state();
        let set = |engine: &mut Self, on: bool, blink: bool| {
            let p = engine.state.profile_mut();
            p.set_cough_on(on);
            p.set_cough_blink(blink);
        };
        match event {
            CoughEvent::Press if !toggle => {
                set(self, true, false);
                self.events.push("cough held: mic muted".into());
            }
            CoughEvent::Hold if toggle => {
                set(self, true, true);
                self.events.push("cough held: mic muted to all".into());
            }
            CoughEvent::Release { hold_handled } if toggle => {
                if hold_handled {
                    return Vec::new();
                }
                if on || blink {
                    set(self, false, false);
                    self.events.push("cough: mic unmuted".into());
                } else {
                    set(self, true, false);
                    self.events.push("cough: mic muted".into());
                }
            }
            CoughEvent::Release { .. } => {
                set(self, false, false);
                self.events.push("cough released: mic live".into());
            }
            _ => return Vec::new(),
        }
        self.channel_changed(Channel::Mic)
    }

    /// Commands after a mute change on `channel`: its 0x809 state, its routing rows, the LEDs.
    pub fn channel_changed(&self, channel: Channel) -> Vec<Command> {
        let mut out: Vec<Command> =
            self.state.channel_mutes().into_iter().filter(|c| c.opcode == u16::from(channel.source_id())).collect();
        if let Some(input) = channel.input() {
            out.extend(self.state.routing_rows(input));
        }
        // Chat's mute can hold mic -> Chat Mic off ("Voice Chat Mute All Also Mutes Mic To Chat Mic").
        if channel == Channel::Chat {
            out.extend(self.state.routing_rows(Input::Mic));
        }
        out.push(self.state.button_leds());
        out
    }
}

pub enum CoughEvent {
    Press,
    Hold,
    Release { hold_handled: bool },
}

fn percent(v: u8) -> u32 {
    (u32::from(v) * 100 + 127) / 255
}

fn name(c: Channel) -> &'static str {
    match c {
        Channel::Mic => "Mic",
        Channel::Chat => "Chat",
        Channel::Music => "Music",
        Channel::Game => "Game",
        Channel::Console => "Console",
        Channel::LineIn => "Line In",
        Channel::System => "System",
        Channel::Sample => "Sample",
        Channel::Headphones => "Headphones",
        Channel::MicMonitor => "Mic Monitor",
        Channel::LineOut => "Line Out",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{Mix, Output};

    /// The test profile (fader A = mic, B = music, every mute function All, cough in hold mode).
    fn engine() -> Engine {
        let (profile, mic) = crate::profile::test_files();
        Engine::new(State::load(&profile, &mic).unwrap())
    }

    fn reading(buttons: u32, faders: [u8; 4]) -> Vec<u8> {
        let mut r = buttons.to_le_bytes().to_vec();
        r.extend_from_slice(&[0; 4]);
        r.extend_from_slice(&faders);
        r
    }

    fn find(cmds: &[Command], category: u16, opcode: u16) -> Option<&Command> {
        cmds.iter().find(|c| c.category == category && c.opcode == opcode)
    }

    #[test]
    fn submix_link_follows_the_fader_and_mix_b_drives_it_back() {
        let mut e = engine();
        let mic = Input::Mic;
        let cmds = e.state.set_submix_enabled(true);
        assert_eq!(find(&cmds, 0x806, 9).unwrap().body, [255], "mic monitor runs at full with submix on");
        assert!(find(&cmds, 0x806, 0x10).is_some(), "Mix B volumes are pushed");

        // Link at half: Mix B = 100 while the main volume is 200.
        e.state.set_volume(Channel::Mic, 200);
        e.state.profile_mut().set_sub_volume(mic, 100);
        e.state.set_sub_linked(mic, true);

        // Moving fader A (mic) to 100 brings Mix B to 50.
        let cmds = e.on_input(&reading(0, [100, 0, 0, 0]), Instant::now());
        assert_eq!(find(&cmds, 0x806, 0x10).unwrap().body, [50]);
        // Setting Mix B to 120 brings the main volume to 240.
        let cmds = e.state.set_sub_volume(mic, 120);
        assert_eq!(find(&cmds, 0x806, 0).unwrap().body, [240]);
        assert_eq!(find(&cmds, 0x806, 0x10).unwrap().body, [120]);

        // Unlinked, the two move on their own.
        e.state.set_sub_linked(mic, false);
        assert!(find(&e.state.set_volume(Channel::Mic, 10), 0x806, 0x10).is_none());

        // Stream on Mix B: 0x817 moves its id (2) into the second array.
        let cmds = e.state.set_output_mix(Output::Stream, Mix::B);
        assert_eq!(find(&cmds, 0x817, 0).unwrap().body[6], 2);
        assert!(e.state.changed_edits().iter().any(|(el, at, v)| el == "mixRoutingTree" && at == "stream" && v == "2"));

        // Off again: the stored mic monitor level comes back and everything listens to Mix A.
        let cmds = e.state.set_submix_enabled(false);
        assert_eq!(find(&cmds, 0x817, 0).unwrap().body[4..], [0x0e; 4]);
        assert_eq!(find(&cmds, 0x818, 0).unwrap().body, [0]);
    }

    #[test]
    fn mute_button_press_mutes_then_unmutes() {
        let mut e = engine();
        let t = Instant::now();
        let channel = e.state.fader_channel(Fader::A);
        let id = u16::from(channel.source_id());
        let (_, _, function) = e.state.mute_state(Fader::A);
        assert!(e.on_input(&reading(1 << 4, [0; 4]), t).is_empty(), "nothing happens on press");
        let cmds = e.on_input(&reading(0, [0; 4]), t + Duration::from_millis(100));
        let expected = u8::from(function == MuteFunction::All);
        assert_eq!(find(&cmds, 0x809, id).unwrap().body, vec![expected]);
        assert_eq!(find(&cmds, 0x808, 0).unwrap().body[4], 1, "mute 1 LED on");

        e.on_input(&reading(1 << 4, [0; 4]), t + Duration::from_millis(200));
        let cmds = e.on_input(&reading(0, [0; 4]), t + Duration::from_millis(300));
        assert_eq!(find(&cmds, 0x809, id).unwrap().body, vec![0]);
    }

    #[test]
    fn holding_mute_mutes_to_all_and_blinks() {
        let mut e = engine();
        let t = Instant::now();
        let id = u16::from(e.state.fader_channel(Fader::B).source_id());
        e.on_input(&reading(1 << 9, [0; 4]), t);
        assert!(e.tick(t + Duration::from_millis(400)).is_empty());
        let cmds = e.tick(t + HOLD);
        assert_eq!(find(&cmds, 0x809, id).unwrap().body, vec![1]);
        assert_eq!(find(&cmds, 0x808, 0).unwrap().body[9], 3, "mute 2 LED flashing");
        assert!(e.on_input(&reading(0, [0; 4]), t + Duration::from_secs(1)).is_empty(), "release after hold does nothing");
    }

    #[test]
    fn mic_mute_drops_mic_routes_except_chat_and_sampler() {
        let mut e = engine();
        let t = Instant::now();
        assert_eq!(e.state.fader_channel(Fader::A), Channel::Mic);
        e.on_input(&reading(1 << 4, [0; 4]), t);
        let cmds = e.tick(t + HOLD); // mute to all
        let row = find(&cmds, 0x804, 2).expect("mic left routing row");
        let cell = |col: usize| u16::from_le_bytes([row.body[col * 2], row.body[col * 2 + 1]]);
        assert_eq!(cell(0), 0, "headphones dropped");
        assert_eq!(cell(2), 0, "stream dropped");
        assert_eq!(cell(8), 0, "line out dropped");
    }

    /// Mic -> Chat Mic cell (left) from a set of commands, if the mic row is among them.
    fn mic_to_chat(cmds: &[Command]) -> Option<u16> {
        find(cmds, 0x804, 2).map(|row| u16::from_le_bytes([row.body[8], row.body[9]]))
    }

    #[test]
    fn chat_muted_to_all_mutes_mic_to_chat() {
        let mut e = engine();
        let t = Instant::now();
        e.state.profile_mut().set_fader_channel(Fader::B, Channel::Chat);
        e.state.profile_mut().set_route(Input::Mic, Output::Chat, 8192);
        e.on_input(&reading(1 << 9, [0; 4]), t);
        let cmds = e.tick(t + HOLD); // chat muted to all
        assert_eq!(mic_to_chat(&cmds), Some(0), "mic -> chat held off");
        assert!(e.on_input(&reading(0, [0; 4]), t + Duration::from_secs(1)).is_empty(), "release after hold");
        e.on_input(&reading(1 << 9, [0; 4]), t + Duration::from_millis(1100));
        let cmds = e.on_input(&reading(0, [0; 4]), t + Duration::from_millis(1200)); // tap: unmute
        assert_eq!(mic_to_chat(&cmds), Some(0x2000), "mic -> chat back");

        e.state.chat_mute_mutes_mic = false;
        e.on_input(&reading(1 << 9, [0; 4]), t + Duration::from_secs(2));
        let cmds = e.tick(t + Duration::from_secs(2) + HOLD);
        assert_eq!(mic_to_chat(&cmds), Some(0x2000), "setting off: mic -> chat stays");
    }

    #[test]
    fn line_out_uses_source_id_9() {
        let mut e = engine();
        e.state.profile_mut().set_fader_channel(Fader::D, Channel::LineOut);
        let cmds = e.state.startup();
        assert_eq!(find(&cmds, 0x805, 3).unwrap().body, vec![9, 0, 0, 0]);
        assert_eq!(find(&cmds, 0x806, 10).unwrap().category, 0x806, "volume keeps channel id 10");
    }

    #[test]
    fn cough_hold_mode_mutes_while_held() {
        let mut e = engine();
        let t = Instant::now();
        let cmds = e.on_input(&reading(1 << 23, [0; 4]), t);
        assert_eq!(find(&cmds, 0x809, 0).unwrap().body, vec![1]);
        let cmds = e.on_input(&reading(0, [0; 4]), t + Duration::from_millis(50));
        assert_eq!(find(&cmds, 0x809, 0).unwrap().body, vec![0]);
    }

    #[test]
    fn fader_move_updates_volume_without_commands() {
        let mut e = engine();
        let cmds = e.on_input(&reading(0, [0, 128, 0, 0]), Instant::now());
        assert!(cmds.is_empty());
        let channel = e.state.fader_channel(Fader::B);
        assert_eq!(e.state.profile().volume(channel), 128);
    }

    #[test]
    fn bleep_lights_while_held() {
        let mut e = engine();
        let t = Instant::now();
        let cmds = e.on_input(&reading(1 << 22, [0; 4]), t);
        assert_eq!(find(&cmds, 0x808, 0).unwrap().body[22], 1);
        let cmds = e.on_input(&reading(0, [0; 4]), t);
        assert_ne!(find(&cmds, 0x808, 0).unwrap().body[22], 1);
    }
}
