//! The GoXLR profile (`.goxlr`, a zip holding `profile.xml`) and mic profile (`.goxlrMicProfile`, plain
//! XML) as this app reads and changes them. Values live in the parsed XML itself (`xml::Doc`), read
//! and written by attribute name, so nothing in the files is lost or reformatted. Element and
//! attribute names are the official app's own. Saving edits the original text: see `profile_file`.

use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::xml::Doc;

/// A mixer channel. The order is the device's channel numbering (0x806 index).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Channel {
    Mic,
    LineIn,
    Console,
    System,
    Game,
    Chat,
    Sample,
    Music,
    Headphones,
    MicMonitor,
    LineOut,
}

/// The fader list in the official app's order: `FaderMeterNlistIndex` is a position in it.
const FADER_LIST: [Channel; 11] = [
    Channel::Mic,
    Channel::Chat,
    Channel::Music,
    Channel::Game,
    Channel::Console,
    Channel::LineIn,
    Channel::System,
    Channel::Sample,
    Channel::Headphones,
    Channel::LineOut,
    Channel::MicMonitor,
];

impl Channel {
    /// Device channel id: 0x806 volume index, and 0x805/0x809 except for ids 9/10 (`source_id`).
    pub fn id(self) -> u8 {
        self as u8
    }

    /// Fader assignment (0x805) and channel mute (0x809) use the source id, which swaps 9 and 10.
    pub fn source_id(self) -> u8 {
        match self {
            Channel::LineOut => 9,
            Channel::MicMonitor => 10,
            other => other.id(),
        }
    }

    /// `mixerTree` volume attribute.
    fn level_attr(self) -> &'static str {
        match self {
            Channel::Mic => "micLevel",
            Channel::LineIn => "lineInLevel",
            Channel::Console => "consoleLevel",
            Channel::System => "systemLevel",
            Channel::Game => "gameLevel",
            Channel::Chat => "chatLevel",
            Channel::Sample => "sampleLevel",
            Channel::Music => "musicLevel",
            Channel::Headphones => "headphoneLevel",
            Channel::MicMonitor => "mic2headphoneSubLevel",
            Channel::LineOut => "lineOutLevel",
        }
    }

    pub fn list_index(self) -> u8 {
        FADER_LIST.iter().position(|&c| c == self).unwrap_or(0) as u8
    }

    /// The routable input of this channel, if it has one.
    pub fn input(self) -> Option<Input> {
        INPUTS.into_iter().find(|i| i.channel() == self)
    }
}

/// A source that can be routed to the outputs.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Input {
    Mic,
    Chat,
    Music,
    Game,
    Console,
    LineIn,
    System,
    Sample,
}

pub const INPUTS: [Input; 8] =
    [Input::Mic, Input::Chat, Input::Music, Input::Game, Input::Console, Input::LineIn, Input::System, Input::Sample];

impl Input {
    /// The name used in profile attributes (`micToStream`, `micLevel`, `micLinked`).
    pub fn name(self) -> &'static str {
        match self {
            Input::Mic => "mic",
            Input::Chat => "chat",
            Input::Music => "music",
            Input::Game => "game",
            Input::Console => "console",
            Input::LineIn => "lineIn",
            Input::System => "system",
            Input::Sample => "sample",
        }
    }

    pub fn channel(self) -> Channel {
        match self {
            Input::Mic => Channel::Mic,
            Input::Chat => Channel::Chat,
            Input::Music => Channel::Music,
            Input::Game => Channel::Game,
            Input::Console => Channel::Console,
            Input::LineIn => Channel::LineIn,
            Input::System => Channel::System,
            Input::Sample => Channel::Sample,
        }
    }
}

/// An output the Mini can route to.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Output {
    Headphones,
    Stream,
    Chat,
    Sampler,
    LineOut,
}

pub const OUTPUTS: [Output; 5] = [Output::Headphones, Output::Stream, Output::Chat, Output::Sampler, Output::LineOut];

impl Output {
    /// Suffix of the routing attributes (`micToHP`).
    fn route_suffix(self) -> &'static str {
        match self {
            Output::Headphones => "HP",
            Output::Stream => "Stream",
            Output::Chat => "Chat",
            Output::Sampler => "Sampler",
            Output::LineOut => "LineOut",
        }
    }

    /// `mixRoutingTree` attribute.
    fn mix_attr(self) -> &'static str {
        match self {
            Output::Headphones => "headphone",
            Output::Stream => "stream",
            Output::Chat => "chat",
            Output::Sampler => "sampler",
            Output::LineOut => "lineout",
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Fader {
    A,
    B,
    C,
    D,
}

pub const FADERS: [Fader; 4] = [Fader::A, Fader::B, Fader::C, Fader::D];

impl Fader {
    pub fn index(self) -> usize {
        self as usize
    }
    fn meter(self) -> String {
        format!("FaderMeter{}", self.index())
    }
    fn mute(self) -> String {
        format!("mute{}", self.index() + 1)
    }
}

/// What a mute button silences. Profiles store the official app's labels ("Mute to Phones").
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MuteFunction {
    All,
    ToStream,
    ToVoiceChat,
    ToPhones,
    ToLineOut,
    ToStream2,
    ToStreams,
}

impl MuteFunction {
    const ORDER: [MuteFunction; 7] = [
        MuteFunction::All,
        MuteFunction::ToStream,
        MuteFunction::ToVoiceChat,
        MuteFunction::ToPhones,
        MuteFunction::ToLineOut,
        MuteFunction::ToStream2,
        MuteFunction::ToStreams,
    ];

    fn from_label(s: &str) -> Option<Self> {
        Some(match s {
            "All" | "Mute All" => MuteFunction::All,
            "Mute to Stream" | "Mute to Stream 1" => MuteFunction::ToStream,
            "Mute to Chat Mic" | "Mute to Voice Chat" => MuteFunction::ToVoiceChat,
            "Mute to Phones" => MuteFunction::ToPhones,
            "Mute to Line Out" => MuteFunction::ToLineOut,
            "Mute to Stream 2" => MuteFunction::ToStream2,
            "Mute to Streams 1 + 2" => MuteFunction::ToStreams,
            _ => return None,
        })
    }

    /// The cough button stores its function as a position in the official list.
    fn from_index(i: usize) -> Self {
        Self::ORDER.get(i).copied().unwrap_or(MuteFunction::All)
    }

    /// Name shown by the window.
    pub fn name(self) -> &'static str {
        match self {
            MuteFunction::All => "All",
            MuteFunction::ToStream => "ToStream",
            MuteFunction::ToVoiceChat => "ToVoiceChat",
            MuteFunction::ToPhones => "ToPhones",
            MuteFunction::ToLineOut => "ToLineOut",
            MuteFunction::ToStream2 => "ToStream2",
            MuteFunction::ToStreams => "ToStreams",
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Mix {
    A = 0,
    B = 1,
}

/// How a button looks when it's off.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum OffStyle {
    Dimmed,
    Colour2,
    DimmedColour2,
}

/// A fader strip's lighting style (`FaderMeterNDisplay`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FaderDisplay {
    TwoColour,
    Gradient,
    Meter,
    GradientMeter,
}

impl FaderDisplay {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "TWO COLOR" => FaderDisplay::TwoColour,
            "GRADIENT" => FaderDisplay::Gradient,
            "METER" => FaderDisplay::Meter,
            "GRADIENT_METER" => FaderDisplay::GradientMeter,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            FaderDisplay::TwoColour => "TWO COLOR",
            FaderDisplay::Gradient => "GRADIENT",
            FaderDisplay::Meter => "METER",
            FaderDisplay::GradientMeter => "GRADIENT_METER",
        }
    }

    pub fn gradient(self) -> bool {
        matches!(self, FaderDisplay::Gradient | FaderDisplay::GradientMeter)
    }

    pub fn meter(self) -> bool {
        matches!(self, FaderDisplay::Meter | FaderDisplay::GradientMeter)
    }
}

/// An LED colour. Profiles store it as AARRGGBB hex.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    /// Parses AARRGGBB (the alpha byte is ignored). "0" and "" are black, as the official files use them.
    pub fn parse_argb(s: &str) -> Self {
        let byte = |i: usize| s.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok()).unwrap_or(0);
        if s.len() != 8 {
            return Rgb::default();
        }
        Rgb { r: byte(2), g: byte(4), b: byte(6) }
    }

    /// Parses RRGGBB, as the window sends it.
    pub fn parse_rgb(s: &str) -> Result<Self> {
        let s = s.trim_start_matches('#');
        if s.len() != 6 || !s.is_ascii() {
            bail!("colour {s} is not RRGGBB");
        }
        let byte = |i: usize| u8::from_str_radix(&s[i..i + 2], 16).context("colour is not hex");
        Ok(Rgb { r: byte(0)?, g: byte(2)?, b: byte(4)? })
    }

    /// RRGGBB, upper case.
    pub fn hex(self) -> String {
        format!("{:02X}{:02X}{:02X}", self.r, self.g, self.b)
    }
}

/// The animation modes, in the order of `animationMode`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Animation {
    RetroRainbow,
    RainbowDark,
    RainbowBright,
    Simple,
    Ripple,
    None,
}

pub const ANIMATIONS: [Animation; 6] = [
    Animation::RetroRainbow,
    Animation::RainbowDark,
    Animation::RainbowBright,
    Animation::Simple,
    Animation::Ripple,
    Animation::None,
];

/// Where HardTune takes its pitch reference from.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HardTuneSource {
    All,
    Music,
    Game,
    LineIn,
    System,
}

/// Number parsing that accepts the official files' "1.00000000000000000000" style.
fn num(s: Option<&str>) -> Option<f64> {
    s.and_then(|v| v.trim().parse::<f64>().ok())
}

/// Profile values on the active preset's elements, read as numbers.
pub struct Preset<'a> {
    doc: &'a Doc,
    element: Option<usize>,
}

impl Preset<'_> {
    pub fn get(&self, key: &str) -> Option<f64> {
        num(self.element.and_then(|e| self.doc.attr(e, key)))
    }
    pub fn int(&self, key: &str) -> i32 {
        self.get(key).map_or(0, |v| v as i32)
    }
    pub fn text(&self, key: &str) -> Option<&str> {
        self.element.and_then(|e| self.doc.attr(e, key))
    }
}

pub struct Profile {
    doc: Doc,
}

impl Profile {
    pub fn load(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let mut zip = zip::ZipArchive::new(file).context("profile is not a zip")?;
        let mut xml = String::new();
        zip.by_name("profile.xml").context("profile has no profile.xml")?.read_to_string(&mut xml)?;
        Ok(Self { doc: Doc::parse(&xml).context("parsing profile.xml")? })
    }

    fn el(&self, name: &str) -> Option<usize> {
        self.doc.find(name)
    }

    pub fn attr(&self, element: &str, key: &str) -> Option<&str> {
        self.el(element).and_then(|e| self.doc.attr(e, key))
    }

    fn number(&self, element: &str, key: &str) -> Option<f64> {
        num(self.attr(element, key))
    }

    fn flag(&self, element: &str, key: &str) -> bool {
        self.attr(element, key) == Some("1")
    }

    /// Sets a value, creating the element if the profile doesn't have it: under the root, as
    /// `profile_file` adds it when saving (linkingTree inside submixerTree).
    pub fn set(&mut self, element: &str, key: &str, value: String) {
        let e = match self.el(element) {
            Some(e) => e,
            None => {
                let root = self.doc.elements.iter().position(|e| e.parent.is_none());
                let parent = if element == "linkingTree" {
                    Some(self.el("submixerTree").unwrap_or_else(|| self.doc.add("submixerTree", root)))
                } else {
                    root
                };
                self.doc.add(element, parent)
            }
        };
        self.doc.set_attr(e, key, value);
    }

    // ---- mixer ----

    /// A channel's volume. Profiles from before Line Out existed have no level for it (full), and
    /// any other missing level reads as half.
    pub fn volume(&self, c: Channel) -> u8 {
        let default = if c == Channel::LineOut { 255 } else { 128 };
        self.number("mixerTree", c.level_attr()).map_or(default, |v| v as u8)
    }

    pub fn set_volume(&mut self, c: Channel, v: u8) {
        self.set("mixerTree", c.level_attr(), v.to_string());
    }

    /// Routing level of `input` to `output` (8192 = on, 0 = off).
    pub fn route(&self, i: Input, o: Output) -> u16 {
        self.number("mixerTree", &format!("{}To{}", i.name(), o.route_suffix())).map_or(0, |v| v as u16)
    }

    pub fn set_route(&mut self, i: Input, o: Output, level: u16) {
        self.set("mixerTree", &format!("{}To{}", i.name(), o.route_suffix()), level.to_string());
    }

    // ---- faders ----

    pub fn fader_channel(&self, f: Fader) -> Channel {
        let i = self.number(&f.meter(), &format!("{}listIndex", f.meter())).map_or(0, |v| v as usize);
        FADER_LIST.get(i).copied().unwrap_or(Channel::Mic)
    }

    pub fn set_fader_channel(&mut self, f: Fader, c: Channel) {
        let el = f.meter();
        self.set(&el, &format!("{el}listIndex"), c.list_index().to_string());
    }

    /// A fader strip's lighting style; two colours when the profile doesn't say.
    pub fn fader_display(&self, f: Fader) -> FaderDisplay {
        let el = f.meter();
        self.attr(&el, &format!("{el}Display")).and_then(FaderDisplay::parse).unwrap_or(FaderDisplay::TwoColour)
    }

    pub fn set_fader_display(&mut self, f: Fader, d: FaderDisplay) {
        let el = f.meter();
        self.set(&el, &format!("{el}Display"), d.label().to_string());
    }

    /// The fader strip's lights (top and bottom colour).
    pub fn fader_leds(&self, f: Fader) -> Leds {
        Leds::new(&f.meter())
    }

    // ---- buttons and lights ----

    /// Colour `i` of an element's lights (`<el>colour<i>`), black when unset.
    pub fn colour(&self, leds: &Leds, i: u8) -> Rgb {
        self.attr(&leds.element, &format!("{}colour{i}", leds.element)).map(Rgb::parse_argb).unwrap_or_default()
    }

    pub fn set_colour(&mut self, leds: &Leds, i: u8, c: Rgb) {
        self.set(&leds.element, &format!("{}colour{i}", leds.element), format!("FF{}", c.hex()));
    }

    pub fn off_style(&self, leds: &Leds) -> OffStyle {
        match self.attr(&leds.element, &format!("{}offStyle", leds.element)) {
            Some("COLOUR2") => OffStyle::Colour2,
            Some("DIMMEDCOLOUR2") => OffStyle::DimmedColour2,
            _ => OffStyle::Dimmed,
        }
    }

    /// Lit (`<el>state` = 1).
    pub fn lit(&self, leds: &Leds) -> bool {
        self.flag(&leds.element, &format!("{}state", leds.element))
    }

    pub fn set_lit(&mut self, leds: &Leds, on: bool) {
        self.set(&leds.element, &format!("{}state", leds.element), u8::from(on).to_string());
    }

    /// Flashing (`<el>blink` = 1).
    pub fn blinking(&self, leds: &Leds) -> bool {
        self.flag(&leds.element, &format!("{}blink", leds.element))
    }

    pub fn set_blinking(&mut self, leds: &Leds, on: bool) {
        self.set(&leds.element, &format!("{}blink", leds.element), u8::from(on).to_string());
    }

    pub fn mute_leds(&self, f: Fader) -> Leds {
        Leds::new(&f.mute())
    }

    pub fn mute_function(&self, f: Fader) -> MuteFunction {
        let el = f.mute();
        self.attr(&el, &format!("{el}Function")).and_then(MuteFunction::from_label).unwrap_or(MuteFunction::All)
    }

    // ---- cough button (muteChat) ----

    pub fn cough_on(&self) -> bool {
        self.flag("muteChat", "coughButtonIsOn")
    }
    pub fn set_cough_on(&mut self, on: bool) {
        self.set("muteChat", "coughButtonIsOn", u8::from(on).to_string());
    }
    pub fn cough_blink(&self) -> bool {
        self.flag("muteChat", "blink")
    }
    pub fn set_cough_blink(&mut self, on: bool) {
        self.set("muteChat", "blink", u8::from(on).to_string());
    }
    pub fn cough_function(&self) -> MuteFunction {
        MuteFunction::from_index(self.number("muteChat", "coughButtonMuteSourceSelection").map_or(0, |v| v as usize))
    }
    pub fn cough_toggle(&self) -> bool {
        self.flag("muteChat", "coughButtonToggleSetting")
    }

    // ---- context ----

    /// Active effect preset, 1-6 (`selectedEffectBank` = "effectsN").
    pub fn effect_preset(&self) -> u8 {
        self.attr("selectedContext", "selectedEffectBank")
            .and_then(|s| s.strip_prefix("effects"))
            .and_then(|n| n.parse().ok())
            .filter(|n| (1..=6).contains(n))
            .unwrap_or(1)
    }

    /// The values of `<effect>presetN` for the active preset (`effect` like "reverbEncoder").
    pub fn preset(&self, effect: &str) -> Preset<'_> {
        Preset { doc: &self.doc, element: self.el(&format!("{effect}preset{}", self.effect_preset())) }
    }

    /// Tracks on a sample pad in the active bank: `track_N` entries below `sampleStackXstackSize`.
    pub fn sample_tracks(&self, pad: &str) -> usize {
        let bank = self.attr("selectedContext", "selectedSampleStack").unwrap_or("sampleStackA");
        let Some(stack) = self.el(pad).and_then(|p| self.doc.find_child(p, bank)) else { return 0 };
        let size = num(self.doc.attr(stack, &format!("{bank}stackSize"))).map_or(0, |v| v as usize);
        (0..size).filter(|i| self.doc.attr(stack, &format!("track_{i}")).is_some()).count()
    }

    pub fn hardtune_source(&self) -> Option<HardTuneSource> {
        Some(match self.preset("hardtuneEffect").text("HARDTUNE_SOURCE")? {
            "ALL" => HardTuneSource::All,
            "MUSIC" => HardTuneSource::Music,
            "GAME" => HardTuneSource::Game,
            "LINEIN" => HardTuneSource::LineIn,
            "SYSTEM" => HardTuneSource::System,
            _ => return None,
        })
    }

    // ---- animation ----

    pub fn animation(&self) -> Animation {
        self.number("animationTree", "animationMode").and_then(|v| ANIMATIONS.get(v as usize).copied()).unwrap_or(Animation::None)
    }

    pub fn set_animation(&mut self, a: Animation) {
        self.set("animationTree", "animationMode", (a as u8).to_string());
    }

    /// Animation parameters: (mod1, mod2, waterfall direction).
    pub fn animation_params(&self) -> (u8, u8, u8) {
        let n = |k| self.number("animationTree", k).map_or(0, |v| v as u8);
        let waterfall = n("mod3");
        (n("mod1"), n("mod2"), if waterfall < 3 { waterfall } else { 0 })
    }

    // ---- submix ----

    pub fn submix_on(&self) -> bool {
        self.flag("submixerTree", "submixMode")
    }
    pub fn set_submix_on(&mut self, on: bool) {
        self.set("submixerTree", "submixMode", u8::from(on).to_string());
    }
    pub fn sub_volume(&self, i: Input) -> u8 {
        self.number("submixerTree", &format!("{}Level", i.name())).map_or(0, |v| v as u8)
    }
    pub fn set_sub_volume(&mut self, i: Input, v: u8) {
        self.set("submixerTree", &format!("{}Level", i.name()), v.to_string());
    }
    /// Linked unless the profile says `<input>Linked="0"`.
    pub fn sub_linked(&self, i: Input) -> bool {
        self.attr("linkingTree", &format!("{}Linked", i.name())) != Some("0")
    }
    pub fn set_sub_linked(&mut self, i: Input, on: bool) {
        self.set("linkingTree", &format!("{}Linked", i.name()), u8::from(on).to_string());
    }
    /// Mix B volume ÷ main volume while linked (1.0 when unset; kept within 1/255…255).
    pub fn sub_ratio(&self, i: Input) -> f64 {
        let r = self.number("linkingTree", &format!("{}Ratio", i.name())).unwrap_or(1.0);
        if r.is_infinite() {
            255.0
        } else if r == 0.0 {
            1.0 / 255.0
        } else {
            r
        }
    }
    pub fn set_sub_ratio(&mut self, i: Input, r: f64) {
        // Debug formatting keeps the ".0" the official files use ("1.0").
        self.set("linkingTree", &format!("{}Ratio", i.name()), format!("{r:?}"));
    }
    pub fn output_mix(&self, o: Output) -> Mix {
        // Stored as 1 (A) or 2 (B).
        if self.number("mixRoutingTree", o.mix_attr()) == Some(2.0) { Mix::B } else { Mix::A }
    }
    pub fn set_output_mix(&mut self, o: Output, m: Mix) {
        self.set("mixRoutingTree", o.mix_attr(), (m as u8 + 1).to_string());
    }
}

/// An element with lights, named the way the profile names it ("mute1", "swear", "FaderMeter0").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Leds {
    pub element: String,
}

impl Leds {
    pub fn new(element: &str) -> Self {
        Self { element: element.to_string() }
    }
}

/// The mic profile. Values are on `dspTreeMicProfile` and `setupTreeMicProfile`.
pub struct MicProfile {
    doc: Doc,
}

pub const DSP: &str = "dspTreeMicProfile";
pub const SETUP: &str = "setupTreeMicProfile";

/// The Mini's six EQ bands: attribute stem, the official app's default centre (Hz) and its
/// fine-tune range (from the official app's mic page).
pub const MINI_EQ: [(&str, f32, f32, f32); 6] = [
    ("90HZ", 90.0, 30.0, 90.0),
    ("250HZ", 160.0, 100.0, 300.0),
    ("500HZ", 480.0, 310.0, 800.0),
    ("1KHZ", 1500.0, 800.0, 2500.0),
    ("3KHZ", 4500.0, 2600.0, 5000.0),
    ("8KHZ", 7800.0, 5100.0, 18000.0),
];

/// The full-size GoXLR's ten EQ bands (still sent to the Mini): attribute stem and default centre.
pub const FULL_EQ: [(&str, f64); 10] = [
    ("31.5HZ", 31.5),
    ("63HZ", 63.0),
    ("125HZ", 125.0),
    ("250HZ", 250.0),
    ("500HZ", 500.0),
    ("1KHZ", 1000.0),
    ("2KHZ", 2000.0),
    ("4KHZ", 4000.0),
    ("8KHZ", 8000.0),
    ("16KHZ", 16000.0),
];

impl MicProfile {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("opening {}", path.display()))?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self> {
        let doc = Doc::parse(text.trim_start_matches('\u{feff}')).context("parsing mic profile")?;
        if doc.find(DSP).is_none() {
            bail!("mic profile has no {DSP}");
        }
        Ok(Self { doc })
    }

    pub fn get(&self, element: &str, key: &str) -> Option<f64> {
        num(self.doc.find(element).and_then(|e| self.doc.attr(e, key)))
    }

    /// A DSP value as an integer, or `default` when the profile doesn't have it.
    pub fn dsp(&self, key: &str, default: i32) -> i32 {
        self.get(DSP, key).map_or(default, |v| v as i32)
    }

    pub fn set(&mut self, element: &str, key: &str, value: String) {
        let e = self.doc.find(element).unwrap_or_else(|| {
            let root = self.doc.find("MicProfileTree");
            self.doc.add(element, root)
        });
        self.doc.set_attr(e, key, value);
    }

    pub fn mic_type(&self) -> u8 {
        self.get(SETUP, "MIC_TYPE").map_or(0, |v| v as u8)
    }

    /// Gain in dB for mic type 0 (dynamic), 1 (condenser) or 2 (3.5 mm). Stored as dB × 65536.
    pub fn gain(&self, mic_type: u8) -> u16 {
        let key = ["DYNAMIC_MIC_GAIN", "CONDENSER_MIC_GAIN", "TRS_MIC_GAIN"][mic_type.min(2) as usize];
        self.get(SETUP, key).map_or(0, |v| ((v as u32) >> 16) as u16)
    }

    pub fn set_gain(&mut self, mic_type: u8, db: u16) {
        let key = ["DYNAMIC_MIC_GAIN", "CONDENSER_MIC_GAIN", "TRS_MIC_GAIN"][mic_type.min(2) as usize];
        self.set(SETUP, key, (u32::from(db) << 16).to_string());
    }

    /// Mini EQ band `b`: (frequency Hz, gain dB).
    pub fn mini_eq(&self, b: usize) -> (f32, i8) {
        let (stem, default, _, _) = MINI_EQ[b];
        let f = self.get(DSP, &format!("MIC_MINI_EQ_{stem}_F")).map_or(default, |v| v as f32);
        let g = self.get(DSP, &format!("MIC_MINI_EQ_{stem}_GAIN")).map_or(0, |v| v as i8);
        (f, g)
    }
}

/// A test file path. The temporary folder holding it is removed once every path from the same
/// `test_files` call is dropped.
#[cfg(test)]
pub struct TestPath {
    path: std::path::PathBuf,
    _dir: std::sync::Arc<TestDir>,
}

#[cfg(test)]
struct TestDir(std::path::PathBuf);

#[cfg(test)]
impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
impl std::ops::Deref for TestPath {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
impl AsRef<Path> for TestPath {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

/// Test files: the hand-written fixtures in tests/fixtures, written out as a real `.goxlr` zip
/// and mic profile in a fresh temporary folder (removed when both paths are dropped).
#[cfg(test)]
pub fn test_files() -> (TestPath, TestPath) {
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    let src = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let dir =
        std::env::temp_dir().join(format!("mini-console-test-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    std::fs::create_dir_all(&dir).unwrap();
    let guard = std::sync::Arc::new(TestDir(dir.clone()));
    let profile = dir.join("Test.goxlr");
    let mut zip = zip::ZipWriter::new(std::fs::File::create(&profile).unwrap());
    zip.start_file("profile.xml", zip::write::SimpleFileOptions::default()).unwrap();
    zip.write_all(&std::fs::read(src.join("test-profile.xml")).unwrap()).unwrap();
    zip.finish().unwrap();
    let mic = dir.join("Test.goxlrMicProfile");
    std::fs::copy(src.join("test-mic.xml"), &mic).unwrap();
    (TestPath { path: profile, _dir: guard.clone() }, TestPath { path: mic, _dir: guard })
}
