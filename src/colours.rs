//! Colour map layout for opcode 0x803: 520 bytes = 130 slots of 4 bytes (B, G, R, 0).
//!
//! Slot positions match the official app's colour layout: each profile element's colours sit in the
//! 520-byte map at fixed bases: scribbles 0/2/4/6, the logo at 8 and 10, mute buttons 12 + 2n,
//! fader strips 20 + 14n, effect presets 77 + 2n, encoders 89 + 3n (colour 1 in the first slot, then
//! colour 0, then colour 2), sample banks 102 + 2n, sample clear 108, sample pads 110 + 2n (top
//! left, top right, bottom right, bottom left), megaphone/robot/hardtune/fx 118 + 2n, then the bleep
//! and cough buttons at 126 and 128.

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Target {
    Fader1Mute,
    Fader2Mute,
    Fader3Mute,
    Fader4Mute,
    Bleep,
    MicrophoneMute,
    EffectSelect1,
    EffectSelect2,
    EffectSelect3,
    EffectSelect4,
    EffectSelect5,
    EffectSelect6,
    EffectFx,
    EffectMegaphone,
    EffectRobot,
    EffectHardTune,
    SamplerSelectA,
    SamplerSelectB,
    SamplerSelectC,
    SamplerTopLeft,
    SamplerTopRight,
    SamplerBottomLeft,
    SamplerBottomRight,
    SamplerClear,
    FadeMeter1,
    FadeMeter2,
    FadeMeter3,
    FadeMeter4,
    Scribble1,
    Scribble2,
    Scribble3,
    Scribble4,
    PitchEncoder,
    GenderEncoder,
    ReverbEncoder,
    EchoEncoder,
    LogoX,
    InternalLight,
}

use Target::*;

pub const ALL: [Target; 38] = [
    Fader1Mute,
    Fader2Mute,
    Fader3Mute,
    Fader4Mute,
    Bleep,
    MicrophoneMute,
    EffectSelect1,
    EffectSelect2,
    EffectSelect3,
    EffectSelect4,
    EffectSelect5,
    EffectSelect6,
    EffectFx,
    EffectMegaphone,
    EffectRobot,
    EffectHardTune,
    SamplerSelectA,
    SamplerSelectB,
    SamplerSelectC,
    SamplerTopLeft,
    SamplerTopRight,
    SamplerBottomLeft,
    SamplerBottomRight,
    SamplerClear,
    FadeMeter1,
    FadeMeter2,
    FadeMeter3,
    FadeMeter4,
    Scribble1,
    Scribble2,
    Scribble3,
    Scribble4,
    PitchEncoder,
    GenderEncoder,
    ReverbEncoder,
    EchoEncoder,
    LogoX,
    InternalLight,
];

impl Target {
    pub fn count(self) -> u8 {
        match self {
            Scribble1 | Scribble2 | Scribble3 | Scribble4 => 1,
            PitchEncoder | GenderEncoder | ReverbEncoder | EchoEncoder => 3,
            _ => 2,
        }
    }

    /// First slot of this target in the map.
    fn start(self) -> usize {
        match self {
            Scribble1 => 0,
            Scribble2 => 2,
            Scribble3 => 4,
            Scribble4 => 6,
            InternalLight => 8,
            LogoX => 10,
            Fader1Mute => 12,
            Fader2Mute => 14,
            Fader3Mute => 16,
            Fader4Mute => 18,
            FadeMeter1 => 20,
            FadeMeter2 => 34,
            FadeMeter3 => 48,
            FadeMeter4 => 62,
            EffectSelect1 => 77,
            EffectSelect2 => 79,
            EffectSelect3 => 81,
            EffectSelect4 => 83,
            EffectSelect5 => 85,
            EffectSelect6 => 87,
            PitchEncoder => 89,
            GenderEncoder => 92,
            ReverbEncoder => 95,
            EchoEncoder => 98,
            SamplerSelectA => 102,
            SamplerSelectB => 104,
            SamplerSelectC => 106,
            SamplerClear => 108,
            SamplerTopLeft => 110,
            SamplerTopRight => 112,
            SamplerBottomRight => 114,
            SamplerBottomLeft => 116,
            EffectMegaphone => 118,
            EffectRobot => 120,
            EffectHardTune => 122,
            EffectFx => 124,
            Bleep => 126,
            MicrophoneMute => 128,
        }
    }

    /// Byte offset of colour `index` inside the 520-byte map.
    pub fn position(self, index: u8) -> usize {
        let start = self.start() * 4;
        match (self, index) {
            (PitchEncoder | GenderEncoder | ReverbEncoder | EchoEncoder, 0) => start + 4,
            (PitchEncoder | GenderEncoder | ReverbEncoder | EchoEncoder, 1) => start,
            _ => start + index as usize * 4,
        }
    }

    /// The profile element whose colours this target shows.
    pub fn element(self) -> &'static str {
        match self {
            Fader1Mute => "mute1",
            Fader2Mute => "mute2",
            Fader3Mute => "mute3",
            Fader4Mute => "mute4",
            Bleep => "swear",
            MicrophoneMute => "muteChat",
            EffectSelect1 => "effects1",
            EffectSelect2 => "effects2",
            EffectSelect3 => "effects3",
            EffectSelect4 => "effects4",
            EffectSelect5 => "effects5",
            EffectSelect6 => "effects6",
            EffectFx => "fxClear",
            EffectMegaphone => "megaphoneEffect",
            EffectRobot => "robotEffect",
            EffectHardTune => "hardtuneEffect",
            SamplerSelectA => "sampleBankA",
            SamplerSelectB => "sampleBankB",
            SamplerSelectC => "sampleBankC",
            SamplerTopLeft => "sampleTopLeft",
            SamplerTopRight => "sampleTopRight",
            SamplerBottomLeft => "sampleBottomLeft",
            SamplerBottomRight => "sampleBottomRight",
            SamplerClear => "sampleClear",
            FadeMeter1 => "FaderMeter0",
            FadeMeter2 => "FaderMeter1",
            FadeMeter3 => "FaderMeter2",
            FadeMeter4 => "FaderMeter3",
            Scribble1 => "scribble1",
            Scribble2 => "scribble2",
            Scribble3 => "scribble3",
            Scribble4 => "scribble4",
            PitchEncoder => "pitchEncoder",
            GenderEncoder => "genderEncoder",
            ReverbEncoder => "reverbEncoder",
            EchoEncoder => "echoEncoder",
            LogoX | InternalLight => "logoX",
        }
    }

    /// Sample pads (their light depends on whether the pad holds a sample).
    pub fn is_sample_pad(self) -> bool {
        matches!(self, SamplerTopLeft | SamplerTopRight | SamplerBottomLeft | SamplerBottomRight)
    }

    /// Button slot in the 24-byte 0x808 LED payload, for targets that are buttons.
    pub fn button_index(self) -> Option<usize> {
        Some(match self {
            EffectSelect1 => 0,
            EffectSelect5 => 1,
            SamplerSelectA => 2,
            SamplerTopLeft => 3,
            Fader1Mute => 4,
            EffectSelect2 => 5,
            EffectSelect6 => 6,
            SamplerSelectB => 7,
            SamplerTopRight => 8,
            Fader2Mute => 9,
            EffectSelect3 => 10,
            EffectRobot => 11,
            SamplerSelectC => 12,
            SamplerBottomRight => 13,
            Fader3Mute => 14,
            EffectSelect4 => 15,
            EffectHardTune => 16,
            SamplerBottomLeft => 17,
            SamplerClear => 18,
            Fader4Mute => 19,
            EffectMegaphone => 20,
            EffectFx => 21,
            Bleep => 22,
            MicrophoneMute => 23,
            _ => return None,
        })
    }
}
