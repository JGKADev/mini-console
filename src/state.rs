//! Builds the full device state push (what the official app sends on connect) from a `.goxlr` profile and a
//! `.goxlrMicProfile`. Pure: produces DCP commands and sends nothing.
//!
//! Command order and value encodings follow a USB capture of the official app's startup and its
//! DSP parameter tables. Most effect values are sent exactly as stored in the profile, whose
//! attributes carry the DSP parameter names.

use std::collections::HashSet;
use std::path::Path;

use anyhow::Result;

use crate::colours::{self, Target};
use crate::profile::{
    Channel, DSP, FADERS, FULL_EQ, Fader, HardTuneSource, INPUTS, Input, Leds, MINI_EQ, MicProfile, Mix, MuteFunction, OUTPUTS,
    OffStyle, Output, Profile, SETUP,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    pub category: u16,
    pub opcode: u16,
    pub body: Vec<u8>,
}

fn cmd(category: u16, opcode: u16, body: impl Into<Vec<u8>>) -> Command {
    Command { category, opcode, body: body.into() }
}

pub type Edit = crate::profile_file::Edit;

pub struct State {
    profile: Profile,
    mic: MicProfile,
    /// `edits()` as loaded, so a save only writes values that changed since.
    loaded: HashSet<Edit>,
    loaded_mic: HashSet<Edit>,
    /// Official app setting "Voice Chat Mute All Also Mutes Mic To Chat Mic" (`voiceChatMuteMutesMic`,
    /// default on). Not stored in the profile.
    pub chat_mute_mutes_mic: bool,
}

/// Official app default when a mic profile has no BLEEP_LEVEL.
const DEFAULT_BLEEP_LEVEL: i8 = -9;

/// Routing matrix row (left channel) for an input.
pub fn input_row(i: Input) -> usize {
    match i {
        Input::Mic => 2,
        Input::LineIn => 4,
        Input::Console => 6,
        Input::System => 8,
        Input::Game => 10,
        Input::Chat => 12,
        Input::Music => 14,
        Input::Sample => 16,
    }
}

/// Routing matrix column (left channel) for an output.
pub fn output_col(o: Output) -> usize {
    match o {
        Output::Headphones => 0,
        Output::Stream => 2,
        Output::Chat => 4,
        Output::Sampler => 6,
        Output::LineOut => 8,
    }
}

/// Gate attenuation is stored as a percent and sent as dB from this table (the official app's own
/// table, indexed by percent × 0.24).
const GATE_ATTENUATION: [i8; 26] = [
    -6, -7, -8, -9, -10, -11, -12, -13, -14, -15, -16, -17, -18, -19, -20, -21, -22, -23, -24, -25, -26, -27, -28, -30, -32, -61,
];

/// Compressor ratio for each stored ratio index (the official app's own table).
const COMPRESSOR_RATIOS: [f32; 15] = [1.0, 1.1, 1.2, 1.4, 1.6, 1.8, 2.0, 2.5, 3.2, 4.0, 5.6, 8.0, 16.0, 32.0, 64.0];

/// 0x801 keys in the order the official app sends them at startup (all 115).
const EFFECT_KEYS: [u32; 115] = [
    // full-size EQ bandwidths: 125, 16k, 1k, 250, 2k, 31.5, 4k, 500, 63, 8k
    0x115, 0x131, 0x11f, 0x12b, 0x12e, 0x128, 0x122, 0x118, 0xfa, 0x10b, 0x14f, 0x158, // gate amount, mic input mute
    0x13d, 0x13c, 0x13e, 0x13f, 0x140, // compressor threshold, ratio, attack, release, makeup
    0x11, 0x16, 0x17, 0x14, 0x15, // gate threshold, attack, release, enable, attenuation
    // full-size EQ gains then frequencies: 31.5, 63, 125, 250, 500, 1k, 2k, 4k, 8k, 16k
    0x127, 0xf9, 0x114, 0x12a, 0x117, 0x11e, 0x12d, 0x121, 0x10a, 0x130, 0x126, 0xf8, 0x113, 0x129, 0x116, 0x11d, 0x12c, 0x120,
    0x109, 0x12f, 0x0b, 0x14b, 0x73, 0x10, // de-esser, comp select, bleep level, gate mode
    // reverb: out level, type, decay, predelay, diffuse, lo/hi colour, hi factor, mod speed/depth, early, tail
    0x76, 0x2e, 0x2f, 0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x39,
    // megaphone: style, dist amount, HP, LP, pre/post gain, dist type, presence gain/fc/bw, beatbox,
    // filter control, filter, drive comp mid/max
    0x3a, 0x3c, 0x3d, 0x3e, 0x3f, 0x40, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49,
    // robot: pulse width, waveform, gate threshold, dry mix, low/mid/high freq+gain+bw, style
    0x146, 0x147, 0x157, 0x14d, 0x133, 0x134, 0x135, 0x139, 0x13a, 0x13b, 0x136, 0x137, 0x138, 0x00,
    // hardtune: key source, amount, window, rate, scale, pitch amount
    0x59, 0x5a, 0x5b, 0x5c, 0x5e, 0x5f,
    // enables: encoder 1, encoder 3, hardtune, megaphone, encoder 4, encoder 2, robot
    0xd5, 0x150, 0xd8, 0xd7, 0x151, 0xd6, 0x14e,
    // echo: out level, source, div L/R, feedback L/R, xfb L>R, R>L, feedback control, filter style
    0x75, 0x1e, 0x20, 0x21, 0x24, 0x25, 0x26, 0x27, 0x28, 0x2a,
    0x1f, // echo tempo (replaced by delay L/R 0x22, 0x23 for the classic slap style)
    0x5d, 0x159, 0x167, // pitch amount, threshold, character
    0x60,  // gender amount
];

/// Effect keys whose value is the same-named attribute of the active preset, as stored.
const PRESET_KEYS: [(u32, &str, &str); 53] = [
    (0x2e, "reverbEncoder", "REVERB_TYPE"),
    (0x2f, "reverbEncoder", "REVERB_DECAY"),
    (0x30, "reverbEncoder", "REVERB_PREDELAY"),
    (0x31, "reverbEncoder", "REVERB_DIFFUSE"),
    (0x32, "reverbEncoder", "REVERB_LOCOLOR"),
    (0x33, "reverbEncoder", "REVERB_HICOLOR"),
    (0x34, "reverbEncoder", "REVERB_HIFACTOR"),
    (0x35, "reverbEncoder", "REVERB_MODSPEED"),
    (0x36, "reverbEncoder", "REVERB_MODDEPTH"),
    (0x37, "reverbEncoder", "REVERB_EARLYLEVEL"),
    (0x3a, "megaphoneEffect", "MEGAPHONE_STYLE"),
    (0x3c, "megaphoneEffect", "TRANS_DIST_AMT"),
    (0x3d, "megaphoneEffect", "TRANS_HP"),
    (0x3e, "megaphoneEffect", "TRANS_LP"),
    (0x3f, "megaphoneEffect", "TRANS_PREGAIN"),
    (0x40, "megaphoneEffect", "TRANS_POSTGAIN"),
    (0x41, "megaphoneEffect", "TRANS_DIST_TYPE"),
    (0x42, "megaphoneEffect", "TRANS_PRESENCE_GAIN"),
    (0x43, "megaphoneEffect", "TRANS_PRESENCE_FC"),
    (0x44, "megaphoneEffect", "TRANS_PRESENCE_BW"),
    (0x45, "megaphoneEffect", "TRANS_BEATBOX_ENABLE"),
    (0x46, "megaphoneEffect", "TRANS_FILTER_CONTROL"),
    (0x47, "megaphoneEffect", "TRANS_FILTER"),
    (0x48, "megaphoneEffect", "TRANS_DRIVE_POT_GAIN_COMP_MID"),
    (0x49, "megaphoneEffect", "TRANS_DRIVE_POT_GAIN_COMP_MAX"),
    (0x146, "robotEffect", "ROBOT_SYNTHOSC_PULSEWIDTH"),
    (0x147, "robotEffect", "ROBOT_SYNTHOSC_WAVEFORM"),
    (0x157, "robotEffect", "ROBOT_VOCODER_GATE_THRESHOLD"),
    (0x14d, "robotEffect", "ROBOT_DRY_MIX"),
    (0x133, "robotEffect", "ROBOT_VOCODER_LOW_FREQ"),
    (0x134, "robotEffect", "ROBOT_VOCODER_LOW_GAIN"),
    (0x135, "robotEffect", "ROBOT_VOCODER_LOW_BW"),
    (0x139, "robotEffect", "ROBOT_VOCODER_MID_FREQ"),
    (0x13a, "robotEffect", "ROBOT_VOCODER_MID_GAIN"),
    (0x13b, "robotEffect", "ROBOT_VOCODER_MID_BW"),
    (0x136, "robotEffect", "ROBOT_VOCODER_HIGH_FREQ"),
    (0x137, "robotEffect", "ROBOT_VOCODER_HIGH_GAIN"),
    (0x138, "robotEffect", "ROBOT_VOCODER_HIGH_BW"),
    (0x00, "robotEffect", "ROBOT_STYLE"),
    (0x5a, "hardtuneEffect", "HARDTUNE_AMOUNT"),
    (0x5b, "hardtuneEffect", "HARDTUNE_WINDOW"),
    (0x5c, "hardtuneEffect", "HARDTUNE_RATE"),
    (0x5e, "hardtuneEffect", "HARDTUNE_SCALE"),
    (0x5f, "hardtuneEffect", "HARDTUNE_PITCH_AMT"),
    (0x1e, "echoEncoder", "DELAY_SOURCE"),
    (0x20, "echoEncoder", "DELAY_DIV_L"),
    (0x21, "echoEncoder", "DELAY_DIV_R"),
    (0x24, "echoEncoder", "DELAY_FB_L"),
    (0x25, "echoEncoder", "DELAY_FB_R"),
    (0x26, "echoEncoder", "DELAY_XFB_L_R"),
    (0x27, "echoEncoder", "DELAY_XFB_R_L"),
    (0x28, "echoEncoder", "DELAY_FB_CONTROL"),
    (0x2a, "echoEncoder", "DELAY_FILTER_STYLE"),
];

/// Effect keys whose value is the same-named mic profile attribute.
const MIC_KEYS: [(u32, &str); 9] = [
    (0x13d, "MIC_COMP_THRESHOLD"),
    (0x13c, "MIC_COMP_RATIO"),
    (0x13e, "MIC_COMP_ATTACK"),
    (0x13f, "MIC_COMP_RELEASE"),
    (0x140, "MIC_COMP_MAKEUPGAIN"),
    (0x11, "MIC_GATE_THRESOLD"),
    (0x16, "MIC_GATE_ATTACK"),
    (0x17, "MIC_GATE_RELEASE"),
    (0x0b, "MIC_DEESS_AMOUNT"),
];

/// Full-size EQ keys: (gain key, frequency key) per band in `FULL_EQ` order.
const FULL_EQ_KEYS: [(u32, u32); 10] = [
    (0x127, 0x126),
    (0xf9, 0xf8),
    (0x114, 0x113),
    (0x12a, 0x129),
    (0x117, 0x116),
    (0x11e, 0x11d),
    (0x12d, 0x12c),
    (0x121, 0x120),
    (0x10a, 0x109),
    (0x130, 0x12f),
];

/// Effect amount (reverb, echo) from its knob position 0-24: −36 dB (off) up to 0 dB.
fn knob_amount(knob: i32) -> i32 {
    36 * knob / 24 - 36
}

/// Full-size EQ frequency index: f = 20 · 2^(index / 24).
fn eq_index(freq: f64) -> i32 {
    (24.0 * (freq / 20.0).log2()).round() as i32
}

impl State {
    pub fn load(profile: &Path, mic_profile: &Path) -> Result<Self> {
        let mut state = Self {
            profile: Profile::load(profile)?,
            mic: MicProfile::load(mic_profile)?,
            loaded: HashSet::new(),
            loaded_mic: HashSet::new(),
            chat_mute_mutes_mic: true,
        };
        if state.submix_enabled() {
            state.sync_linked();
        }
        state.mark_saved();
        Ok(state)
    }

    pub fn profile(&self) -> &Profile {
        &self.profile
    }

    pub fn profile_mut(&mut self) -> &mut Profile {
        &mut self.profile
    }

    pub fn mic(&self) -> &MicProfile {
        &self.mic
    }

    pub fn mic_mut(&mut self) -> &mut MicProfile {
        &mut self.mic
    }

    /// The profile.xml attributes this app changes at runtime, with their current values.
    pub fn edits(&self) -> Vec<Edit> {
        use Channel::*;
        let p = &self.profile;
        let e = |el: &str, at: String, v: String| (el.to_string(), at, v);
        let mut out: Vec<Edit> = [
            (Mic, "mic"),
            (Chat, "chat"),
            (Music, "music"),
            (Game, "game"),
            (Console, "console"),
            (LineIn, "lineIn"),
            (System, "system"),
            (Sample, "sample"),
            (Headphones, "headphone"),
            (MicMonitor, "mic2headphoneSub"),
            (LineOut, "lineOut"),
        ]
        .into_iter()
        .map(|(c, name)| e("mixerTree", format!("{name}Level"), p.volume(c).to_string()))
        .collect();
        for (i, f) in FADERS.into_iter().enumerate() {
            let (on, blink, _) = self.mute_state(f);
            let el = format!("mute{}", i + 1);
            out.push(e(&el, format!("{el}state"), u8::from(on).to_string()));
            out.push(e(&el, format!("{el}blink"), u8::from(blink).to_string()));
        }
        let (on, blink, _) = self.cough_state();
        out.push(e("muteChat", "coughButtonIsOn".into(), u8::from(on).to_string()));
        out.push(e("muteChat", "blink".into(), u8::from(blink).to_string()));

        for input in INPUTS {
            for (output, oname) in [
                (Output::Headphones, "HP"),
                (Output::Stream, "Stream"),
                (Output::Chat, "Chat"),
                (Output::Sampler, "Sampler"),
                (Output::LineOut, "LineOut"),
            ] {
                out.push(e("mixerTree", format!("{}To{oname}", input.name()), p.route(input, output).to_string()));
            }
        }

        let argb = |leds: &Leds, i: u8| format!("FF{}", p.colour(leds, i).hex());
        for (i, f) in FADERS.into_iter().enumerate() {
            let el = format!("FaderMeter{i}");
            let leds = p.fader_leds(f);
            out.push(e(&el, format!("{el}listIndex"), p.fader_channel(f).list_index().to_string()));
            out.push(e(&el, format!("{el}colour0"), argb(&leds, 0)));
            out.push(e(&el, format!("{el}colour1"), argb(&leds, 1)));
            out.push(e(&el, format!("{el}Display"), p.fader_display(f).label().to_string()));
            let mute = format!("mute{}", i + 1);
            let leds = p.mute_leds(f);
            out.push(e(&mute, format!("{mute}colour0"), argb(&leds, 0)));
            out.push(e(&mute, format!("{mute}colour1"), argb(&leds, 1)));
        }
        out.push(e("muteChat", "muteChatcolour0".into(), argb(&Leds::new("muteChat"), 0)));
        out.push(e("swear", "swearcolour0".into(), argb(&Leds::new("swear"), 0)));
        out.push(e("logoX", "logoXcolour0".into(), argb(&Leds::new("logoX"), 0)));
        out.push(e("animationTree", "animationMode".into(), (p.animation() as u8).to_string()));

        out.push(e("submixerTree", "submixMode".into(), u8::from(p.submix_on()).to_string()));
        for input in INPUTS {
            let name = input.name();
            out.push(e("submixerTree", format!("{name}Level"), p.sub_volume(input).to_string()));
            out.push(e("linkingTree", format!("{name}Linked"), u8::from(p.sub_linked(input)).to_string()));
            // Debug formatting keeps the ".0" the official files use ("1.0").
            out.push(e("linkingTree", format!("{name}Ratio"), format!("{:?}", p.sub_ratio(input))));
        }
        for (output, name) in [
            (Output::Headphones, "headphone"),
            (Output::LineOut, "lineout"),
            (Output::Chat, "chat"),
            (Output::Sampler, "sampler"),
            (Output::Stream, "stream"),
        ] {
            out.push(e("mixRoutingTree", name.into(), (p.output_mix(output) as u8 + 1).to_string()));
        }
        out
    }

    /// The mic profile attributes this app can change, with their current values.
    pub fn mic_edits(&self) -> Vec<Edit> {
        let e = |el: &str, at: &str, v: String| (el.to_string(), at.to_string(), v);
        let m = &self.mic;
        let mut out: Vec<Edit> = [
            "MIC_GATE_THRESOLD",
            "MIC_GATE_ATTEN",
            "MIC_GATE_ATTACK",
            "MIC_GATE_RELEASE",
            "MIC_COMP_THRESHOLD",
            "MIC_COMP_RATIO",
            "MIC_COMP_ATTACK",
            "MIC_COMP_RELEASE",
            "MIC_COMP_MAKEUPGAIN",
            "MIC_DEESS_AMOUNT",
        ]
        .into_iter()
        .map(|k| e(DSP, k, m.dsp(k, 0).to_string()))
        .collect();
        out.push(e(DSP, "BLEEP_LEVEL", self.bleep_level().to_string()));
        for (b, (stem, ..)) in MINI_EQ.iter().enumerate() {
            let (f, g) = m.mini_eq(b);
            out.push(e(DSP, &format!("MIC_MINI_EQ_{stem}_F"), f.to_string()));
            out.push(e(DSP, &format!("MIC_MINI_EQ_{stem}_GAIN"), g.to_string()));
        }
        out.push(e(SETUP, "MIC_TYPE", m.mic_type().to_string()));
        for (t, key) in ["DYNAMIC_MIC_GAIN", "CONDENSER_MIC_GAIN", "TRS_MIC_GAIN"].into_iter().enumerate() {
            out.push(e(SETUP, key, (u32::from(m.gain(t as u8)) << 16).to_string()));
        }
        out
    }

    pub fn changed_mic_edits(&self) -> Vec<Edit> {
        self.mic_edits().into_iter().filter(|e| !self.loaded_mic.contains(e)).collect()
    }

    /// The managed attributes whose values changed since the profile was loaded.
    pub fn changed_edits(&self) -> Vec<Edit> {
        self.edits().into_iter().filter(|e| !self.loaded.contains(e)).collect()
    }

    pub fn bleep_level(&self) -> i8 {
        self.mic.get(DSP, "BLEEP_LEVEL").map_or(DEFAULT_BLEEP_LEVEL, |v| v as i8)
    }

    pub fn set_bleep_level(&mut self, level: i8) {
        self.mic.set(DSP, "BLEEP_LEVEL", level.clamp(-36, 0).to_string());
    }

    /// Marks the current values as saved.
    pub fn mark_saved(&mut self) {
        self.loaded = self.edits().into_iter().collect();
        self.loaded_mic = self.mic_edits().into_iter().collect();
    }

    /// Marks only the mic profile values as saved (after saving them under a new name).
    pub fn mark_mic_saved(&mut self) {
        self.loaded_mic = self.mic_edits().into_iter().collect();
    }

    /// Mic commands to resend after any mic setting change: the full 0x80b list, the full 0x801
    /// list (it carries the gate, compressor and de-esser too) and the gain.
    pub fn mic_commands(&self) -> Vec<Command> {
        vec![self.mic_params(), self.effects(), self.mic_gain()]
    }

    pub fn fader_channel(&self, fader: Fader) -> Channel {
        self.profile.fader_channel(fader)
    }

    /// (muted_to_x, muted_to_all, function) for a fader's mute button.
    pub fn mute_state(&self, fader: Fader) -> (bool, bool, MuteFunction) {
        let leds = self.profile.mute_leds(fader);
        (self.profile.lit(&leds), self.profile.blinking(&leds), self.profile.mute_function(fader))
    }

    fn fader_muted_to_all(&self, fader: Fader) -> bool {
        let (x, all, function) = self.mute_state(fader);
        all || (x && function == MuteFunction::All)
    }

    fn fader_of(&self, channel: Channel) -> Option<Fader> {
        FADERS.into_iter().find(|&f| self.fader_channel(f) == channel)
    }

    /// (muted_to_x, muted_to_all, function) for the cough button (mic only).
    pub fn cough_state(&self) -> (bool, bool, MuteFunction) {
        (self.profile.cough_on(), self.profile.cough_blink(), self.profile.cough_function())
    }

    /// Muted to all, by its fader's mute button or (mic only) by the cough button.
    pub fn channel_muted(&self, channel: Channel) -> bool {
        let by_fader = self.fader_of(channel).is_some_and(|f| self.fader_muted_to_all(f));
        let by_cough = channel == Channel::Mic && {
            let (x, all, function) = self.cough_state();
            all || (x && function == MuteFunction::All)
        };
        by_fader || by_cough
    }

    /// Chat's mute holds mic -> Chat Mic off: chat muted to all, or muted with function All or To Phones
    /// (you can't hear chat, so chat doesn't hear you). The official app keeps a
    /// per-channel flag for this and applies it to mic -> Chat while muting.
    fn chat_mute_holds_mic(&self) -> bool {
        self.chat_mute_mutes_mic
            && self.fader_of(Channel::Chat).is_some_and(|f| {
                let (x, all, function) = self.mute_state(f);
                all || (x && matches!(function, MuteFunction::All | MuteFunction::ToPhones))
            })
    }

    /// Whether a mute currently holds `input -> output` off, although the profile routes it.
    /// The mic's HP/Stream/Chat/Line Out cells are ANDed with the cough and fader masks; other
    /// channels lose only the muted-to column, and muted to all leaves their routing to 0x809.
    fn route_held_off(&self, input: Input, output: Output) -> bool {
        if (input, output) == (Input::Mic, Output::Chat) && self.chat_mute_holds_mic() {
            return true;
        }
        let mut mutes: Vec<(bool, bool, MuteFunction)> =
            self.fader_of(input.channel()).map(|f| self.mute_state(f)).into_iter().collect();
        if input == Input::Mic {
            mutes.push(self.cough_state());
        }
        mutes.into_iter().any(|(x, all, function)| {
            if all || (x && function == MuteFunction::All) {
                // Mic muted to all: the official app drops HP, Stream and Line Out but keeps Chat and
                // Sampler (as captured from the official app). The engine drops Chat too, but the chat-channel pass that runs
                // after it puts mic -> Chat back unless a mute *to chat* holds it.
                // 0x809 silences the mic either way. Other channels are muted by 0x809 alone.
                input == Input::Mic && matches!(output, Output::Headphones | Output::Stream | Output::LineOut)
            } else if x {
                // Mute to X: only that output.
                matches!(
                    (function, output),
                    (MuteFunction::ToStream | MuteFunction::ToStreams, Output::Stream)
                        | (MuteFunction::ToVoiceChat, Output::Chat)
                        | (MuteFunction::ToPhones, Output::Headphones)
                        | (MuteFunction::ToLineOut, Output::LineOut)
                )
            } else {
                false
            }
        })
    }

    fn lit(&self, element: &str) -> bool {
        self.profile.lit(&Leds::new(element))
    }

    fn fx_enabled(&self) -> bool {
        self.lit("fxClear")
    }

    fn hardtune_enabled(&self) -> bool {
        self.lit("hardtuneEffect")
    }

    /// The whole startup state push, in the official app's order.
    pub fn startup(&self) -> Vec<Command> {
        let mut out = Vec::new();
        out.extend(self.fader_assignments());
        out.extend(self.channel_mutes());
        out.extend(self.volumes());
        out.push(self.effects());
        out.push(self.mic_params());
        // 0x802 scribble images are skipped: the Mini has no scribble screens. The official app
        // still renders and sends them (blank, then the profile's scribbleN.png).
        out.extend(self.encoder_modes());
        out.push(self.button_leds());
        out.extend(self.fader_displays());
        out.extend(self.encoder_values());
        out.push(self.animation());
        out.push(self.colour_map());
        out.extend(self.routing());
        out.push(self.monitored_mix());
        out.push(self.submix_outputs());
        out.push(self.mic_gain());
        out
    }

    pub fn fader_assignments(&self) -> Vec<Command> {
        FADERS
            .iter()
            .enumerate()
            .map(|(i, &f)| cmd(0x805, i as u16, u32::from(self.fader_channel(f).source_id()).to_le_bytes()))
            .collect()
    }

    /// 0x809: 1 = channel muted to all, indexed by source id. The official app sends ids in this order.
    pub fn channel_mutes(&self) -> Vec<Command> {
        use Channel::*;
        [Mic, Chat, Music, Game, Console, LineIn, System, Sample, Headphones, LineOut, MicMonitor]
            .into_iter()
            .map(|c| cmd(0x809, c.source_id().into(), [u8::from(self.channel_muted(c))]))
            .collect()
    }

    /// 0x806 bank 0 = main volumes (ids 0-10), bank 1 = submix (Mix B) volumes (ids 0x10-0x17).
    fn volumes(&self) -> Vec<Command> {
        use Channel::*;
        let mut out: Vec<Command> = [Mic, LineIn, Console, System, Game, Chat, Sample, Music, Headphones, MicMonitor, LineOut]
            .into_iter()
            .map(|c| {
                cmd(0x806, c.id().into(), [if c == MicMonitor { self.mic_monitor_volume() } else { self.profile.volume(c) }])
            })
            .collect();
        for i in [Input::Mic, Input::LineIn, Input::Console, Input::System, Input::Game, Input::Chat, Input::Sample, Input::Music]
        {
            out.push(self.sub_volume_command(i));
        }
        out
    }

    /// Gate attenuation in dB for the stored percent. Like the official app: index = percent × 0.24
    /// (so 0–99 % use the first 24 steps) and 100 % jumps to the last entry.
    fn gate_attenuation(&self) -> i8 {
        let percent = self.mic.dsp("MIC_GATE_ATTEN", 0);
        if percent > 99 {
            return GATE_ATTENUATION[25];
        }
        GATE_ATTENUATION[(percent.max(0) as f32 * 0.24) as usize]
    }

    fn effect_value(&self, key: u32) -> i32 {
        let p = &self.profile;
        if let Some((_, effect, attr)) = PRESET_KEYS.iter().find(|(k, ..)| *k == key) {
            return p.preset(effect).int(attr);
        }
        if let Some((_, attr)) = MIC_KEYS.iter().find(|(k, _)| *k == key) {
            return self.mic.dsp(attr, 0);
        }
        if let Some(b) = FULL_EQ_KEYS.iter().position(|(g, _)| *g == key) {
            return self.mic.dsp(&format!("MIC_EQ_{}_GAIN", FULL_EQ[b].0), 0);
        }
        if let Some(b) = FULL_EQ_KEYS.iter().position(|(_, f)| *f == key) {
            let (stem, default) = FULL_EQ[b];
            return eq_index(self.mic.get(DSP, &format!("MIC_EQ_{stem}_F")).unwrap_or(default));
        }
        let fx = self.fx_enabled();
        match key {
            0x115 | 0x131 | 0x11f | 0x12b | 0x12e | 0x128 | 0x122 | 0x118 | 0xfa | 0x10b => 16, // fixed
            0x14f => 0, // MIC_GATE_AMOUNT: sent as 0 although the mic profile has a macro amount
            0x158 => 0, // MIC_INPUT_MUTE: the official app mutes via 0x809 + routing, not here
            0x14 => 1,  // gate enable: always 1 (the UI uses it for mic testing)
            0x15 => self.gate_attenuation().into(),
            0x14b => self.mic.dsp("MIC_COMP_SELECT", 1),
            0x73 => self.bleep_level().into(),
            0x10 => self.mic.dsp("MIC_GATE_MODE", 2),
            0x76 => knob_amount(p.preset("reverbEncoder").int("REVERB_KNOB_POSITION")),
            0x39 => 0, // tail level: always 0 from the official app
            0x59 => 0, // hardtune key source: always 0, the source is set through routing column 12
            0xd5 | 0xd6 | 0x150 | 0x151 => fx.into(),
            0xd7 => (fx && self.lit("megaphoneEffect")).into(),
            0xd8 => (fx && self.hardtune_enabled()).into(),
            0x14e => (fx && self.lit("robotEffect")).into(),
            0x75 => knob_amount(p.preset("echoEncoder").int("DELAY_KNOB_POSITION")),
            0x1f => p.preset("echoEncoder").int("DELAY_TEMPO"),
            0x22 => p.preset("echoEncoder").int("DELAY_TIME_L"),
            0x23 => p.preset("echoEncoder").int("DELAY_TIME_R"),
            0x5d => p.preset("pitchEncoder").int("PITCH_KNOB_POSITION"),
            // The threshold follows the pitch style (narrow −36, wide −26), whatever the file says.
            0x159 => {
                if self.pitch_wide() {
                    -26
                } else {
                    -36
                }
            }
            0x167 => p.preset("pitchEncoder").get("PITCH_SHIFT_INST_RATIO").map_or(75, |v| v as i32),
            0x60 => {
                // Gender amount: the knob (−24…24) scaled to the style's range of 12, 25 or 50.
                let g = p.preset("genderEncoder");
                let range = match g.int("GENDER_STYLE") {
                    1 => 25.0,
                    2 => 50.0,
                    _ => 12.0,
                };
                (range * f64::from(g.int("GENDER_KNOB_POSITION")) / 24.0).round() as i32
            }
            _ => unreachable!("effect key {key:#x} has no value source"),
        }
    }

    fn pitch_wide(&self) -> bool {
        self.profile.preset("pitchEncoder").int("PITCH_STYLE") == 1
    }

    /// 0x801: every effect parameter as {u32 key, i32 value}.
    fn effects(&self) -> Command {
        // Echo style 4 (classic slap) sends its delay times instead of the tempo.
        let slap = self.profile.preset("echoEncoder").int("DELAY_STYLE") == 4;
        let mut body = Vec::with_capacity(EFFECT_KEYS.len() * 8 + 8);
        for key in EFFECT_KEYS {
            let keys: &[u32] = if key == 0x1f && slap { &[0x22, 0x23] } else { &[key] };
            for &k in keys {
                body.extend_from_slice(&k.to_le_bytes());
                body.extend_from_slice(&self.effect_value(k).to_le_bytes());
            }
        }
        cmd(0x801, 0, body)
    }

    /// 0x80b: Mini mic parameters as {u32 key, f32 value}.
    fn mic_params(&self) -> Command {
        let m = &self.mic;
        let ratio = COMPRESSOR_RATIOS.get(m.dsp("MIC_COMP_RATIO", 0).max(0) as usize).copied().unwrap_or(1.0);
        let f = |k: &str| m.dsp(k, 0) as f32;
        // Mini EQ bandwidth is not stored in the profile; the official app always sends 13.
        let bw = 13.0;
        let eq: Vec<(f32, i8)> = (0..6).map(|b| m.mini_eq(b)).collect();
        let pairs: [(u32, f32); 29] = [
            (0x00000, if m.mic_type() == 1 { 1.0 } else { 0.0 }), // phantom power on condenser
            (0x30200, f("MIC_GATE_THRESOLD")),
            (0x30400, f("MIC_GATE_ATTACK")),
            (0x30600, f("MIC_GATE_RELEASE")),
            (0x30900, self.gate_attenuation().into()),
            (0x40000, eq[0].0),
            (0x40001, eq[0].1.into()),
            (0x40002, bw),
            (0x40003, eq[1].0),
            (0x40004, eq[1].1.into()),
            (0x40005, bw),
            (0x40006, eq[2].0),
            (0x40007, eq[2].1.into()),
            (0x40008, bw),
            (0x50000, eq[3].0),
            (0x50001, eq[3].1.into()),
            (0x50002, bw),
            (0x50003, eq[4].0),
            (0x50004, eq[4].1.into()),
            (0x50005, bw),
            (0x50006, eq[5].0),
            (0x50007, eq[5].1.into()),
            (0x50008, bw),
            (0x60200, f("MIC_COMP_THRESHOLD")),
            (0x60300, ratio),
            (0x60400, f("MIC_COMP_ATTACK")),
            (0x60600, f("MIC_COMP_RELEASE")),
            (0x60700, f("MIC_COMP_MAKEUPGAIN")),
            // Bleep level is sent as the 16.16 fixed-point value converted to float.
            (0x70100, f32::from(self.bleep_level()) * 65536.0),
        ];
        let mut body = Vec::with_capacity(pairs.len() * 8);
        for (k, v) in pairs {
            body.extend_from_slice(&k.to_le_bytes());
            body.extend_from_slice(&v.to_le_bytes());
        }
        cmd(0x80b, 0, body)
    }

    /// 0x80b MIC_GAIN_{DYNAMIC,CONDENSER,TRS} for the selected mic type, as u16 in the high half.
    fn mic_gain(&self) -> Command {
        let t = self.mic.mic_type();
        let key: u32 = match t {
            1 => 2,
            2 => 3,
            _ => 1,
        };
        let mut body = key.to_le_bytes().to_vec();
        body.extend_from_slice(&(u32::from(self.mic.gain(t)) << 16).to_le_bytes());
        cmd(0x80b, 0, body)
    }

    /// 0x811 {mode, resolution}: pitch, gender, reverb, echo. HardTune switches pitch to mode 3
    /// with a resolution of 1 (narrow) or 2 (wide).
    fn encoder_modes(&self) -> Vec<Command> {
        let ht = self.hardtune_enabled();
        let pitch = if ht { [3, if self.pitch_wide() { 2 } else { 1 }] } else { [1, 4] };
        vec![cmd(0x811, 0, pitch), cmd(0x811, 1, [1, 4]), cmd(0x811, 2, [0, 2]), cmd(0x811, 3, [0, 2])]
    }

    /// 0x80a encoder knob positions: pitch, gender, reverb, echo. The pitch knob counts in half
    /// steps when narrow, whole steps when wide, and octaves (÷12) with HardTune on.
    fn encoder_values(&self) -> Vec<Command> {
        let p = &self.profile;
        let knob = |effect: &str, key: &str| p.preset(effect).int(key) as i8;
        let pitch = knob("pitchEncoder", "PITCH_KNOB_POSITION");
        let pitch = if self.hardtune_enabled() {
            pitch / 12
        } else if self.pitch_wide() {
            pitch
        } else {
            pitch.wrapping_mul(2)
        };
        let values = [
            pitch,
            knob("genderEncoder", "GENDER_KNOB_POSITION"),
            knob("reverbEncoder", "REVERB_KNOB_POSITION"),
            knob("echoEncoder", "DELAY_KNOB_POSITION"),
        ];
        values.iter().enumerate().map(|(i, &v)| cmd(0x80a, i as u16, [v as u8])).collect()
    }

    /// 0x808: one LED state byte per button slot.
    pub fn button_leds(&self) -> Command {
        let mut body = [ButtonState::DimmedColour1 as u8; 24];
        for target in colours::ALL {
            let Some(index) = target.button_index() else { continue };
            body[index] = self.button_state(target) as u8;
        }
        cmd(0x808, 0, body)
    }

    fn button_state(&self, target: Target) -> ButtonState {
        let p = &self.profile;
        let leds = Leds::new(target.element());
        if target == Target::MicrophoneMute {
            if p.cough_blink() {
                return ButtonState::Flashing;
            }
            if p.cough_on() {
                return ButtonState::Colour1;
            }
            return off_state(p.off_style(&leds));
        }
        if target.is_sample_pad() && p.sample_tracks(target.element()) == 0 {
            return ButtonState::DimmedColour2; // empty sample slot: unlit
        }
        if p.blinking(&leds) {
            return ButtonState::Flashing;
        }
        if p.lit(&leds) {
            return ButtonState::Colour1;
        }
        off_state(p.off_style(&leds))
    }

    /// 0x814 {gradient, meter} per fader.
    pub fn fader_displays(&self) -> Vec<Command> {
        FADERS
            .iter()
            .enumerate()
            .map(|(i, &f)| {
                let d = self.profile.fader_display(f);
                cmd(0x814, i as u16, [u8::from(d.gradient()), u8::from(d.meter())])
            })
            .collect()
    }

    /// 0x816 {enabled, mode, mod1, mod2, waterfall}.
    pub fn animation(&self) -> Command {
        let mode = self.profile.animation() as u8;
        let (mod1, mod2, waterfall) = self.profile.animation_params();
        cmd(0x816, 0, [u8::from(mode != 5), mode, mod1, mod2, waterfall])
    }

    /// 0x803: the 520-byte colour map, each colour as B, G, R, 0.
    pub fn colour_map(&self) -> Command {
        let p = &self.profile;
        let mut map = vec![0u8; 520];
        for target in colours::ALL {
            let leds = Leds::new(target.element());
            // Two-colour buttons (and the logo) with the DIMMED off style get colour 1 blanked: the
            // device dims colour 0 itself. Fader meters and encoders keep every colour.
            let dimmed = (target.button_index().is_some() || matches!(target, Target::LogoX | Target::InternalLight))
                && p.off_style(&leds) == OffStyle::Dimmed;
            for i in 0..target.count() {
                let pos = target.position(i);
                let c = p.colour(&leds, i);
                let bytes = if dimmed && i == 1 { [0; 4] } else { [c.b, c.g, c.r, 0] }; // the official app never sends alpha
                map[pos..pos + 4].copy_from_slice(&bytes);
            }
        }
        cmd(0x803, 0, map)
    }

    /// The two 0x804 rows (left, right) of one input.
    pub fn routing_rows(&self, input: Input) -> Vec<Command> {
        let row = input_row(input);
        self.routing().into_iter().filter(|c| c.opcode as usize == row || c.opcode as usize == row + 1).collect()
    }

    /// 0x804: 18 rows x 13 u16 levels. Stereo pairs, 0x2000 = on.
    pub fn routing(&self) -> Vec<Command> {
        let mut matrix = [[0u16; 13]; 18];
        let source = self.profile.hardtune_source();
        for input in INPUTS {
            let row = input_row(input);
            for output in OUTPUTS {
                let col = output_col(output);
                if self.profile.route(input, output) > 0 && !self.route_held_off(input, output) {
                    matrix[row][col] = 0x2000;
                    matrix[row + 1][col + 1] = 0x2000;
                }
            }
            // Column 12 feeds HardTune its pitch reference: the chosen source, or a quarter of each
            // of the four music-like sources for "all".
            let feed = match (source, input) {
                (None | Some(HardTuneSource::All), Input::Music | Input::Game | Input::LineIn | Input::System) => 0x0400,
                (Some(HardTuneSource::Music), Input::Music)
                | (Some(HardTuneSource::Game), Input::Game)
                | (Some(HardTuneSource::LineIn), Input::LineIn)
                | (Some(HardTuneSource::System), Input::System) => 0x1000,
                _ => 0,
            };
            matrix[row][12] = feed;
            matrix[row + 1][12] = feed;
        }
        matrix
            .iter()
            .enumerate()
            .map(|(row, cells)| cmd(0x804, row as u16, cells.iter().flat_map(|c| c.to_le_bytes()).collect::<Vec<_>>()))
            .collect()
    }

    /// Submix mode: every input also feeds a second mix (Mix B) with its own volume, and each output
    /// listens to Mix A or Mix B.
    pub fn submix_enabled(&self) -> bool {
        self.profile.submix_on()
    }

    pub fn sub_volume(&self, input: Input) -> u8 {
        self.profile.sub_volume(input)
    }

    pub fn sub_linked(&self, input: Input) -> bool {
        self.profile.sub_linked(input)
    }

    pub fn output_mix(&self, output: Output) -> Mix {
        self.profile.output_mix(output)
    }

    /// 0x806 bank 1: an input's Mix B volume.
    fn sub_volume_command(&self, input: Input) -> Command {
        cmd(0x806, 0x10 | u16::from(input.channel().id()), [self.sub_volume(input)])
    }

    /// Sets a channel's main volume. With submix on and the channel linked, Mix B follows at the
    /// linked ratio.
    pub fn set_volume(&mut self, channel: Channel, volume: u8) -> Vec<Command> {
        self.profile.set_volume(channel, volume);
        if channel == Channel::MicMonitor {
            // Held at full while submix is on; the stored level comes back when it's turned off.
            return vec![cmd(0x806, channel.id().into(), [self.mic_monitor_volume()])];
        }
        let mut out = vec![cmd(0x806, channel.id().into(), [volume])];
        out.extend(self.follow_linked(channel));
        out
    }

    /// After a main volume change (from the window or a hardware fader): the linked Mix B volume.
    pub fn follow_linked(&mut self, channel: Channel) -> Option<Command> {
        let input = channel.input()?;
        if !self.submix_enabled() || !self.sub_linked(input) {
            return None;
        }
        let ratio = self.profile.sub_ratio(input);
        let linked = (f64::from(self.profile.volume(channel)) * ratio).clamp(0.0, 255.0) as u8;
        if linked == self.sub_volume(input) {
            return None;
        }
        self.profile.set_sub_volume(input, linked);
        Some(self.sub_volume_command(input))
    }

    /// Sets an input's Mix B volume. When linked, the main volume follows so the ratio holds.
    pub fn set_sub_volume(&mut self, input: Input, volume: u8) -> Vec<Command> {
        let mut out = Vec::new();
        if self.sub_linked(input) {
            let ratio = self.profile.sub_ratio(input);
            let main = (f64::from(volume) / ratio).clamp(0.0, 255.0) as u8;
            let channel = input.channel();
            if self.profile.volume(channel) != main {
                self.profile.set_volume(channel, main);
                out.push(cmd(0x806, channel.id().into(), [main]));
            }
        }
        self.profile.set_sub_volume(input, volume);
        out.push(self.sub_volume_command(input));
        out
    }

    /// Links or unlinks Mix B to the main volume. Linking keeps the current ratio between them.
    pub fn set_sub_linked(&mut self, input: Input, linked: bool) {
        if linked {
            let main = self.profile.volume(input.channel()).max(1);
            let sub = self.sub_volume(input).max(1);
            self.profile.set_sub_ratio(input, f64::from(sub) / f64::from(main));
        }
        self.profile.set_sub_linked(input, linked);
    }

    /// Turns submix mode on or off, and returns everything that changes on the device.
    pub fn set_submix_enabled(&mut self, on: bool) -> Vec<Command> {
        self.profile.set_submix_on(on);
        self.sync_linked();
        self.submix_commands()
    }

    /// Brings every linked Mix B volume to main × ratio. Profiles that never had submix on store
    /// Mix B at 127 whatever the main volume is.
    fn sync_linked(&mut self) {
        for input in INPUTS {
            self.follow_linked(input.channel());
        }
    }

    /// Picks the mix an output listens to.
    pub fn set_output_mix(&mut self, output: Output, mix: Mix) -> Vec<Command> {
        self.profile.set_output_mix(output, mix);
        self.submix_commands()
    }

    /// The submix state on the device: monitored mix, output assignment, Mix B volumes and the mic
    /// monitor level. With submix on, the mic monitor runs at full, since the headphones' mix sets
    /// how loud the mic is there (not verified against the official app).
    fn submix_commands(&self) -> Vec<Command> {
        let mut out = vec![self.monitored_mix(), self.submix_outputs()];
        if self.submix_enabled() {
            out.extend(INPUTS.into_iter().map(|i| self.sub_volume_command(i)));
        }
        out.push(cmd(0x806, Channel::MicMonitor.id().into(), [self.mic_monitor_volume()]));
        out
    }

    fn mic_monitor_volume(&self) -> u8 {
        if self.submix_enabled() { 255 } else { self.profile.volume(Channel::MicMonitor) }
    }

    /// 0x818: which mix the headphones monitor (0 = A, 1 = B). Submix mode off forces A.
    fn monitored_mix(&self) -> Command {
        let mix = if self.submix_enabled() { self.output_mix(Output::Headphones) } else { Mix::A };
        cmd(0x818, 0, [mix as u8])
    }

    /// 0x817: Mix A outputs then Mix B outputs, slots {chat, sampler, stream, line out}, filler 0x0e.
    /// Captured with submix off: `0e 06 02 08 0e 0e 0e 0e`. The official app gives chat id 4 when
    /// its mixRoutingTree value changes; at startup no such event fired, so chat stays 0x0e with
    /// submix off and goes to its mix when submix is on.
    fn submix_outputs(&self) -> Command {
        let enabled = self.submix_enabled();
        let mut body = [0x0e_u8; 8];
        if enabled {
            body[4 * self.output_mix(Output::Chat) as usize] = 4;
        }
        for (slot, output, id) in [(1, Output::Sampler, 6u8), (2, Output::Stream, 2), (3, Output::LineOut, 8)] {
            let mix = if enabled { self.output_mix(output) } else { Mix::A };
            body[slot + 4 * mix as usize] = id;
        }
        cmd(0x817, 0, body)
    }
}

#[derive(Copy, Clone)]
enum ButtonState {
    Colour2 = 0x00,
    Colour1 = 0x01,
    DimmedColour1 = 0x02,
    Flashing = 0x03,
    DimmedColour2 = 0x04,
}

fn off_state(style: OffStyle) -> ButtonState {
    match style {
        OffStyle::Dimmed => ButtonState::DimmedColour1,
        OffStyle::Colour2 => ButtonState::Colour2,
        OffStyle::DimmedColour2 => ButtonState::DimmedColour2,
    }
}
