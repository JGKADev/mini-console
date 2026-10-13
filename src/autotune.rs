//! Mic auto-tune: analyses a guided recording of the dry and processed mic and proposes mic
//! settings (gain, gate, compressor, EQ, de-esser). Pure functions; the recording comes from
//! `asio`, the guided script and apply/undo live in `service`.
//!
//! Level zones are the official gain wizard's
//! (Good = −16.8…−5.3 dB of 250 ms dry peaks). The tone target is the long-term average speech
//! spectrum of Byrne et al. 1994 (row picked from the measured voice pitch) plus a gentle presence
//! lift. The level rules aim speech peaks at the middle of the Good zone, with the compressor and
//! make-up gain set so processed speech peaks land near −9 dB.

use serde_json::{Value, json};

/// One step of the guided script. The window shows the words; the service times the steps.
pub struct Phase {
    pub key: &'static str,
    pub secs: f64,
}

/// The full test: about 50 seconds.
pub const FULL: [Phase; 6] = [
    Phase { key: "silence", secs: 5.0 },
    Phase { key: "read", secs: 22.0 },
    Phase { key: "sibilance", secs: 6.0 },
    Phase { key: "plosive", secs: 6.0 },
    Phase { key: "loud", secs: 5.0 },
    Phase { key: "quiet", secs: 6.0 },
];

/// The check after trying new settings: about 22 seconds.
pub const CHECK: [Phase; 3] =
    [Phase { key: "silence", secs: 3.0 }, Phase { key: "read2", secs: 15.0 }, Phase { key: "loud", secs: 4.0 }];

/// Where each phase starts and ends in the recording (samples).
#[derive(Clone, Debug)]
pub struct Segment {
    pub key: &'static str,
    pub start: usize,
    pub end: usize,
}

// ---------- control tables (the official app's ranges) ----------
pub const RATIOS: [f64; 15] = [1.0, 1.1, 1.2, 1.4, 1.6, 1.8, 2.0, 2.5, 3.2, 4.0, 5.6, 8.0, 16.0, 32.0, 64.0];
pub const COMP_ATTACK_MS: [u32; 20] = [0, 2, 3, 4, 5, 6, 7, 8, 9, 10, 12, 14, 16, 18, 20, 23, 26, 30, 35, 40];
pub const COMP_RELEASE_MS: [u32; 20] =
    [0, 15, 25, 35, 45, 55, 65, 75, 85, 100, 115, 140, 170, 230, 340, 680, 1000, 1500, 2000, 3000];
fn gate_ms(i: i32) -> u32 {
    let i = i.clamp(0, 45) as u32;
    match i {
        0..=19 => (i + 1) * 10,
        20..=35 => 250 + (i - 20) * 50,
        _ => 1100 + (i - 36) * 100,
    }
}

/// The mic settings the auto-tune reads and changes. Mic type and phantom power are never touched.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MicSettings {
    /// Gain of the selected mic type, dB (0–72).
    pub gain: i32,
    /// Frequency (Hz) and gain (dB, ±9) of the six EQ bands.
    pub eq: [(f32, i32); 6],
    /// Threshold dB, attenuation %, attack index, release index.
    pub gate: [i32; 4],
    /// Threshold dB, ratio index, attack index, release index, make-up dB.
    pub comp: [i32; 5],
    pub deess: i32,
}

impl MicSettings {
    /// The window commands (see `control::apply`) that set these values.
    pub fn commands(&self) -> Vec<Value> {
        let mut out = vec![json!({ "cmd": "micGain", "value": self.gain })];
        for (band, (freq, gain)) in self.eq.iter().enumerate() {
            out.push(json!({ "cmd": "eq", "band": band, "freq": freq, "gain": gain }));
        }
        let [threshold, atten, attack, release] = self.gate;
        out.push(json!({ "cmd": "gate", "threshold": threshold, "atten": atten, "attack": attack, "release": release }));
        let [threshold, ratio, attack, release, makeup] = self.comp;
        out.push(json!({ "cmd": "comp", "threshold": threshold, "ratio": ratio, "attack": attack, "release": release, "makeup": makeup }));
        out.push(json!({ "cmd": "deess", "value": self.deess }));
        out
    }

    pub fn to_json(self) -> Value {
        json!({ "gain": self.gain, "eq": self.eq, "gate": self.gate, "comp": self.comp, "deess": self.deess })
    }
}

// ---------- reference speech spectrum (Byrne et al. 1994, Table II) ----------
const CENTRES: [f64; 25] = [
    63.0, 80.0, 100.0, 125.0, 160.0, 200.0, 250.0, 315.0, 400.0, 500.0, 630.0, 800.0, 1000.0, 1250.0, 1600.0, 2000.0, 2500.0,
    3150.0, 4000.0, 5000.0, 6300.0, 8000.0, 10000.0, 12500.0, 16000.0,
];
const REF_LOW: [f64; 25] = [
    38.6, 43.5, 54.4, 57.7, 56.8, 58.2, 59.7, 60.0, 62.4, 62.6, 60.6, 55.7, 53.1, 53.7, 52.3, 48.7, 48.9, 47.0, 46.0, 44.4, 43.3,
    42.4, 41.9, 39.8, 40.4,
];
const REF_HIGH: [f64; 25] = [
    37.0, 36.0, 37.5, 40.1, 53.4, 62.2, 60.9, 58.1, 61.7, 61.7, 60.4, 58.0, 54.3, 52.3, 51.7, 48.8, 47.3, 46.7, 45.3, 44.6, 45.2,
    44.9, 45.0, 42.8, 41.1,
];
/// "Podcast" presence lift on top of the reference, dB per band.
const TILT: [f64; 25] = [0., 0., 0., 0., 0., 0., 0., 0., 0., 0., 0., 0., 0., 0., 0., 0., 1., 2., 2., 2., 1., 0., 0., 0., 0.];
/// Bands used to line the levels up (250 Hz–4 kHz).
const NORM: std::ops::RangeInclusive<usize> = 6..=18;
/// The reference spectrum for a voice pitch (Hz; 0 when unknown).
fn reference(pitch: f64) -> &'static [f64; 25] {
    if pitch > 0.0 && pitch < 165.0 { &REF_LOW } else { &REF_HIGH }
}

/// Mean level of `bands` above `target` over the NORM bands: the offset that lines them up.
fn norm_offset(bands: &[f64; 25], target: &[f64; 25]) -> f64 {
    NORM.map(|i| bands[i] - target[i]).sum::<f64>() / NORM.count() as f64
}

/// Bands the EQ fit looks at (80 Hz–10 kHz): below, room rumble; above, most broadcast dynamics roll off.
const FIT: std::ops::RangeInclusive<usize> = 1..=22;

// ---------- small DSP helpers ----------
fn db(x: f64) -> f64 {
    20.0 * (x + 1e-12).log10()
}

fn pow_db(p: f64) -> f64 {
    10.0 * (p + 1e-30).log10()
}

/// numpy-style percentile (linear interpolation), `p` in 0–100.
fn percentile(v: &[f64], p: f64) -> f64 {
    percentile_sorted(&sorted(v), p)
}

fn sorted(v: &[f64]) -> Vec<f64> {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.total_cmp(b));
    s
}

/// `percentile` of values already in ascending order.
fn percentile_sorted(s: &[f64], p: f64) -> f64 {
    if s.is_empty() {
        return f64::NAN;
    }
    let k = (s.len() - 1) as f64 * p / 100.0;
    let (lo, hi) = (k.floor() as usize, k.ceil() as usize);
    s[lo] + (s[hi] - s[lo]) * (k - lo as f64)
}

fn median(v: &[f64]) -> f64 {
    percentile(v, 50.0)
}

/// exp(−2πik/SEG) for k < SEG/2: the FFT's twiddle factors.
fn twiddles() -> &'static [(f64, f64)] {
    static TABLE: std::sync::OnceLock<Vec<(f64, f64)>> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let ang = -2.0 * std::f64::consts::PI / SEG as f64;
        (0..SEG / 2).map(|k| ((ang * k as f64).cos(), (ang * k as f64).sin())).collect()
    })
}

/// In-place radix-2 FFT of (re, im) pairs; `a.len()` must be a power of two, at most SEG.
fn fft(a: &mut [(f64, f64)]) {
    let n = a.len();
    debug_assert!(n.is_power_of_two() && n <= SEG);
    let tw = twiddles();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            a.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let stride = SEG / len;
        for start in (0..n).step_by(len) {
            for k in 0..len / 2 {
                let (wr, wi) = tw[k * stride];
                let (ur, ui) = a[start + k];
                let (xr, xi) = a[start + k + len / 2];
                let (vr, vi) = (xr * wr - xi * wi, xr * wi + xi * wr);
                a[start + k] = (ur + vr, ui + vi);
                a[start + k + len / 2] = (ur - vr, ui - vi);
            }
        }
        len <<= 1;
    }
}

const SEG: usize = 4096;

/// Power spectrum (bins 0..=SEG/2) of one Hann-windowed segment, zero-padded to SEG.
fn power_spectrum(x: &[f32]) -> Vec<f64> {
    let n = x.len().min(SEG);
    let mut a = vec![(0.0, 0.0); SEG];
    for i in 0..n {
        let w = 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / n as f64).cos();
        a[i].0 = f64::from(x[i]) * w;
    }
    fft(&mut a);
    a[..=SEG / 2].iter().map(|(r, i)| r * r + i * i).collect()
}

fn band_power(p: &[f64], fs: f64, lo: f64, hi: f64) -> f64 {
    let bin = fs / SEG as f64;
    p.iter().enumerate().filter(|(k, _)| (*k as f64 * bin) >= lo && (*k as f64 * bin) < hi).map(|(_, v)| v).sum()
}

/// 1/3-octave band levels (dB, relative) of an averaged power spectrum.
fn third_octaves(p: &[f64], fs: f64) -> [f64; 25] {
    let k = 2f64.powf(1.0 / 6.0);
    CENTRES.map(|c| pow_db(band_power(p, fs, c / k, c * k)))
}

/// 50 ms frames of `x[start..end]`: (RMS dB, peak dB, first sample).
fn frames(x: &[f32], start: usize, end: usize, w: usize) -> Vec<(f64, f64, usize)> {
    let end = end.min(x.len());
    (start..end.saturating_sub(w - 1))
        .step_by(w)
        .map(|s| {
            let f = &x[s..s + w];
            let ms = f.iter().map(|&v| f64::from(v) * f64::from(v)).sum::<f64>() / w as f64;
            let pk = f.iter().fold(0f32, |m, &v| m.max(v.abs()));
            (db(ms.sqrt()), db(f64::from(pk)), s)
        })
        .collect()
}

/// Peaks of 250 ms windows (the official wizard's meter scale), dB.
fn peaks_250(x: &[f32], start: usize, end: usize, fs: f64) -> Vec<f64> {
    let w = (fs * 0.25) as usize;
    frames(x, start, end, w).into_iter().map(|f| f.1).collect()
}

/// Biquad (b0, b1, b2, a1, a2), direct form I.
fn biquad(x: impl IntoIterator<Item = f64>, c: [f64; 5]) -> Vec<f64> {
    let (mut x1, mut x2, mut y1, mut y2) = (0.0, 0.0, 0.0, 0.0);
    x.into_iter()
        .map(|v| {
            let y = c[0] * v + c[1] * x1 + c[2] * x2 - c[3] * y1 - c[4] * y2;
            (x2, x1, y2, y1) = (x1, v, y1, y);
            y
        })
        .collect()
}

/// Integrated loudness (ITU-R BS.1770 / EBU R128) of a mono signal, LUFS. −70 if silent.
pub fn loudness(x: &[f32], fs: f64) -> f64 {
    // K-weighting: high shelf, then high pass (coefficients for any rate, as pyloudnorm derives them).
    let (f0, g, q) = (1681.974450955533, 3.999843853973347, 0.7071752369554196);
    let k = (std::f64::consts::PI * f0 / fs).tan();
    let (vh, vb) = (10f64.powf(g / 20.0), 10f64.powf(g / 20.0).powf(0.4996667741545416));
    let a0 = 1.0 + k / q + k * k;
    let shelf = [
        (vh + vb * k / q + k * k) / a0,
        2.0 * (k * k - vh) / a0,
        (vh - vb * k / q + k * k) / a0,
        2.0 * (k * k - 1.0) / a0,
        (1.0 - k / q + k * k) / a0,
    ];
    let (f0, q) = (38.13547087602444, 0.5003270373238773);
    let k = (std::f64::consts::PI * f0 / fs).tan();
    let a0 = 1.0 + k / q + k * k;
    let hp = [1.0, -2.0, 1.0, 2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0];
    let y = biquad(biquad(x.iter().map(|&v| f64::from(v)), shelf), hp);
    let (block, hop) = ((fs * 0.4) as usize, (fs * 0.1) as usize);
    if y.len() < block {
        return -70.0;
    }
    let z: Vec<f64> = (0..=(y.len() - block) / hop)
        .map(|i| y[i * hop..i * hop + block].iter().map(|v| v * v).sum::<f64>() / block as f64)
        .collect();
    let lufs = |m: f64| -0.691 + 10.0 * (m + 1e-30).log10();
    let gated: Vec<f64> = z.iter().copied().filter(|&m| lufs(m) > -70.0).collect();
    if gated.is_empty() {
        return -70.0;
    }
    let rel = lufs(gated.iter().sum::<f64>() / gated.len() as f64) - 10.0;
    let kept: Vec<f64> = gated.into_iter().filter(|&m| lufs(m) > rel).collect();
    lufs(kept.iter().sum::<f64>() / kept.len().max(1) as f64)
}

/// Median voice pitch (Hz) by autocorrelation over the given frames; 0 if none is voiced.
fn pitch(x: &[f32], starts: &[usize], w: usize, fs: f64) -> f64 {
    let (lo, hi) = ((fs / 400.0) as usize, (fs / 60.0) as usize);
    let mut f0s = Vec::new();
    for &s in starts.iter().step_by((starts.len() / 60).max(1)) {
        let f = &x[s..s + w];
        let mean = f.iter().map(|&v| f64::from(v)).sum::<f64>() / w as f64;
        let f: Vec<f64> = f.iter().map(|&v| f64::from(v) - mean).collect();
        let ac = |lag: usize| (0..w - lag).map(|n| f[n] * f[n + lag]).sum::<f64>();
        let zero = ac(0);
        let (best, val) = (lo..hi.min(w - 1)).map(|l| (l, ac(l))).fold((0, f64::MIN), |b, c| if c.1 > b.1 { c } else { b });
        if best > 0 && val > 0.3 * zero {
            f0s.push(fs / best as f64);
        }
    }
    if f0s.is_empty() { 0.0 } else { median(&f0s) }
}

// ---------- analysis ----------

/// Everything measured from one take. Dry levels are dBFS at the gain used for the take.
#[derive(Clone, Debug)]
pub struct Analysis {
    pub fs: f64,
    pub noise: f64,
    pub noise_p90: f64,
    pub speech_secs: f64,
    pub speech_rms: [f64; 3],
    pub peak250: [f64; 2],
    pub loud_max: Option<f64>,
    pub clipped: usize,
    pub quiet_rms: Option<f64>,
    pub pitch: f64,
    pub dry_bands: [f64; 25],
    pub proc_bands: [f64; 25],
    pub sib: Option<(f64, f64)>,
    pub pops: usize,
    pub proc_noise: f64,
    pub proc_peak250: f64,
    pub proc_max: f64,
    pub proc_speech_rms: f64,
    pub proc_quiet: Option<f64>,
    pub proc_lufs: f64,
    /// Samples of the reading to play back (from the first word, at most 15 s).
    pub clip: (usize, usize),
}

impl Analysis {
    pub fn to_json(&self) -> Value {
        let r = |v: f64| (v * 10.0).round() / 10.0;
        let reference = reference(self.pitch);
        let (od, op) = (norm_offset(&self.dry_bands, reference), norm_offset(&self.proc_bands, reference));
        json!({
            "noise": r(self.noise), "speechSecs": r(self.speech_secs), "speechRms": r(self.speech_rms[1]),
            "peak250": r(self.peak250[0]), "peak250p90": r(self.peak250[1]),
            "loudMax": self.loud_max.map(r), "clipped": self.clipped, "quiet": self.quiet_rms.map(r),
            "snr": r(self.speech_rms[1] - self.noise), "pitch": self.pitch.round(),
            "procPeak250": r(self.proc_peak250), "procMax": r(self.proc_max), "procNoise": r(self.proc_noise),
            "procLufs": r(self.proc_lufs), "pops": self.pops,
            "sibilance": self.sib.map(|(d, p)| json!({ "dry": r(d), "processed": r(p) })),
            "bands": CENTRES.iter().enumerate().map(|(i, &c)| json!({
                "hz": c, "dry": r(self.dry_bands[i] - od - reference[i]), "processed": r(self.proc_bands[i] - op - reference[i]),
                "target": TILT[i],
            })).collect::<Vec<_>>(),
        })
    }
}

/// Measures a take. Err is a message for the user when the take can't be used.
pub fn analyse(dry: &[f32], processed: &[f32], fs: f64, segments: &[Segment], full: bool) -> Result<Analysis, String> {
    let w = (fs * 0.05) as usize;
    let skip = (fs * 0.5) as usize;
    let len = dry.len().min(processed.len());
    let seg = |key: &str| segments.iter().find(|s| s.key == key).map(|s| (s.start.min(len), s.end.min(len)));
    let (s0, s1) = seg("silence").ok_or("no silence step")?;
    let read_key = if full { "read" } else { "read2" };
    let (r0, r1) = seg(read_key).ok_or("no reading step")?;
    // The silence needs at least ten frames left after the first half second.
    if w == 0 || r1 <= r0 + w * 20 || s1 <= s0 + skip + w * 10 {
        return Err("The recording stopped early. Try again.".into());
    }

    // Silence: skip the first half second (the click of starting, a breath).
    let silent_rms = sorted(&frames(dry, s0 + skip, s1, w).iter().map(|f| f.0).collect::<Vec<_>>());
    if silent_rms.is_empty() {
        return Err("The quiet part of the recording was too short. Try again.".into());
    }
    let noise = percentile_sorted(&silent_rms, 50.0);
    let noise_p90 = percentile_sorted(&silent_rms, 90.0);
    let threshold = noise + 12.0;
    let proc_noise = median(&frames(processed, s0 + skip, s1, w).iter().map(|f| f.0).collect::<Vec<_>>());

    let read = frames(dry, r0, r1, w);
    let speech: Vec<&(f64, f64, usize)> = read.iter().filter(|f| f.0 > threshold).collect();
    let speech_secs = speech.len() as f64 * 0.05;
    let need = if full { 8.0 } else { 6.0 };
    if speech_secs < need {
        return Err(format!(
            "We only heard {speech_secs:.1} s of speech while you were reading (we need {need:.0} s). Start reading as soon as the prompt says so, at your normal distance, then try again."
        ));
    }
    let speech_rms = sorted(&speech.iter().map(|f| f.0).collect::<Vec<_>>());
    let speech_rms =
        [percentile_sorted(&speech_rms, 10.0), percentile_sorted(&speech_rms, 50.0), percentile_sorted(&speech_rms, 90.0)];

    // Processed mic while speaking: silent means muted somewhere before the tap.
    let proc_speech: Vec<f64> =
        speech.iter().map(|f| frames(processed, f.2, f.2 + w, w).first().map_or(-120.0, |p| p.0)).collect();
    let proc_speech_rms = median(&proc_speech);
    if proc_speech_rms < -75.0 {
        return Err("We can hear your mic, but the processed mic is silent. Is the mic muted (its fader's mute button or the cough button)? Unmute it and try again.".into());
    }

    let p250 = sorted(&peaks_250(dry, r0, r1, fs).into_iter().filter(|&p| p > threshold + 6.0).collect::<Vec<_>>());
    if p250.is_empty() {
        return Err("Your speech never rose clearly above the room noise. Speak at your normal level and distance from the mic, then try again.".into());
    }
    let peak250 = [percentile_sorted(&p250, 50.0), percentile_sorted(&p250, 90.0)];
    let proc_p250: Vec<f64> = peaks_250(processed, r0, r1, fs).into_iter().filter(|&p| p > proc_noise + 18.0).collect();
    let proc_peak250 = if proc_p250.is_empty() { -96.0 } else { median(&proc_p250) };

    let clipped = dry.iter().filter(|v| v.abs() > 0.999).count();
    let loud_max = seg("loud").and_then(|(a, b)| {
        let f = frames(dry, a, b, w);
        f.iter().any(|f| f.0 > threshold).then(|| f.iter().map(|f| f.1).fold(f64::MIN, f64::max))
    });
    let proc_max = processed.iter().skip(s1.min(processed.len())).fold(0f32, |m, v| m.max(v.abs()));
    let quiet = seg("quiet").map(|(a, b)| frames(dry, a, b, w));
    let quiet_rms = quiet.as_ref().and_then(|f| {
        let v: Vec<f64> = f.iter().filter(|f| f.0 > threshold).map(|f| f.0).collect();
        (v.len() >= 10).then(|| percentile(&v, 20.0))
    });
    let proc_quiet = quiet.as_ref().and_then(|f| {
        let v: Vec<f64> = f
            .iter()
            .filter(|f| f.0 > threshold)
            .map(|f| frames(processed, f.2, f.2 + w, w).first().map_or(-120.0, |p| p.0))
            .collect();
        (v.len() >= 10).then(|| percentile(&v, 20.0))
    });

    // Long-term spectra of the speech frames.
    let starts: Vec<usize> = speech.iter().map(|f| f.2).collect();
    let average = |x: &[f32], starts: &[usize]| {
        let mut acc = vec![0.0; SEG / 2 + 1];
        let mut n = 0;
        for &s in starts {
            // one 4096-sample window centred on each speech frame, every other frame
            let a = (s + w / 2).saturating_sub(SEG / 2);
            if a + SEG <= x.len() && (s / w).is_multiple_of(2) {
                for (o, p) in acc.iter_mut().zip(power_spectrum(&x[a..a + SEG])) {
                    *o += p;
                }
                n += 1;
            }
        }
        acc.iter().map(|v| v / n.max(1) as f64).collect::<Vec<f64>>()
    };
    let dry_bands = third_octaves(&average(dry, &starts), fs);
    let proc_bands = third_octaves(&average(processed, &starts), fs);

    // Sibilance: 5–10 kHz against 1–4 kHz, on the "she sells" line (or the reading).
    let sib_starts: Vec<usize> = match seg("sibilance") {
        Some((a, b)) => frames(dry, a, b, w).into_iter().filter(|f| f.0 > threshold).map(|f| f.2).collect(),
        None => starts.clone(),
    };
    let sib = (sib_starts.len() >= 10).then(|| {
        let ratio = |p: &[f64]| pow_db(band_power(p, fs, 5000.0, 10000.0)) - pow_db(band_power(p, fs, 1000.0, 4000.0));
        (ratio(&average(dry, &sib_starts)), ratio(&average(processed, &sib_starts)))
    });

    // Plosives: low-frequency bursts on "Peter Piper" well above the reading's.
    let lf = |s: usize| {
        let p = power_spectrum(&dry[s..(s + w).min(dry.len())]);
        pow_db(band_power(&p, fs, 20.0, 150.0))
    };
    let pops = seg("plosive").map_or(0, |(a, b)| {
        let read_lf: Vec<f64> = starts.iter().step_by(4).map(|&s| lf(s)).collect();
        let limit = percentile(&read_lf, 95.0) + 8.0;
        frames(dry, a, b, w).into_iter().filter(|f| f.0 > threshold && f.1 > -12.0 && lf(f.2) > limit).count()
    });

    let clip_start = starts.first().copied().unwrap_or(r0).saturating_sub(w * 4);
    let clip = (clip_start, r1.min(clip_start + (fs * 15.0) as usize));
    Ok(Analysis {
        fs,
        noise,
        noise_p90,
        speech_secs,
        speech_rms,
        peak250,
        loud_max,
        clipped,
        quiet_rms,
        pitch: pitch(dry, &starts, w, fs),
        dry_bands,
        proc_bands,
        sib,
        pops,
        proc_noise,
        proc_peak250,
        proc_max: db(f64::from(proc_max)),
        proc_speech_rms,
        proc_quiet,
        proc_lufs: loudness(&processed[r0..r1], fs),
        clip,
    })
}

// ---------- recommendations ----------

/// dB response at `f` of an RBJ peaking filter (the planning model for one Mini EQ band).
fn peaking_db(f: f64, f0: f64, gain: f64, octaves: f64, fs: f64) -> f64 {
    let a = 10f64.powf(gain / 40.0);
    let w0 = 2.0 * std::f64::consts::PI * f0 / fs;
    let alpha = w0.sin() * ((2f64.ln() / 2.0) * octaves * w0 / w0.sin()).sinh();
    let (b0, b1, b2) = (1.0 + alpha * a, -2.0 * w0.cos(), 1.0 - alpha * a);
    let (a0, a1, a2) = (1.0 + alpha / a, -2.0 * w0.cos(), 1.0 - alpha / a);
    let w = 2.0 * std::f64::consts::PI * f / fs;
    let h = |c0: f64, c1: f64, c2: f64| {
        let (re, im) = (c0 + c1 * w.cos() + c2 * (2.0 * w).cos(), -(c1 * w.sin() + c2 * (2.0 * w).sin()));
        (re * re + im * im).sqrt()
    };
    db(h(b0, b1, b2) / h(a0, a1, a2))
}

/// Solves `m x = v` (small, dense) by Gaussian elimination with partial pivoting.
fn solve(mut m: Vec<Vec<f64>>, mut v: Vec<f64>) -> Vec<f64> {
    let n = v.len();
    for c in 0..n {
        let p = (c..n).max_by(|&a, &b| m[a][c].abs().total_cmp(&m[b][c].abs())).unwrap();
        m.swap(c, p);
        v.swap(c, p);
        if m[c][c].abs() < 1e-12 {
            continue;
        }
        for r in c + 1..n {
            let k = m[r][c] / m[c][c];
            for j in c..n {
                m[r][j] -= k * m[c][j];
            }
            v[r] -= k * v[c];
        }
    }
    let mut x = vec![0.0; n];
    for r in (0..n).rev() {
        let s: f64 = (r + 1..n).map(|j| m[r][j] * x[j]).sum();
        x[r] = if m[r][r].abs() < 1e-12 { 0.0 } else { (v[r] - s) / m[r][r] };
    }
    x
}

/// Band-gain changes (dB) that move the processed spectrum toward the target.
fn eq_fit(a: &Analysis, eq: &[(f32, i32); 6]) -> ([i32; 6], f64) {
    let reference = reference(a.pitch);
    let target: [f64; 25] = std::array::from_fn(|i| reference[i] + TILT[i]);
    let off = norm_offset(&a.proc_bands, &target);
    let mut want = [0.0; 25];
    let mut worst: f64 = 0.0;
    for i in FIT {
        let mut d = target[i] - (a.proc_bands[i] - off);
        if CENTRES[i] < 250.0 {
            d = d.min(0.0); // below 250 Hz only cut: dips there are the voice's pitch, not the mic
        }
        worst = worst.max(d.abs());
        want[i] = if d.abs() < 1.5 { 0.0 } else { d.clamp(-6.0, 6.0) };
    }
    // weighted ridge least squares over the six band shapes (1-octave peaking filters)
    let shape: [[f64; 25]; 6] =
        std::array::from_fn(|b| std::array::from_fn(|i| peaking_db(CENTRES[i], f64::from(eq[b].0), 6.0, 1.0, a.fs) / 6.0));
    let weight = |i: usize| if (200.0..=6300.0).contains(&CENTRES[i]) { 1.0 } else { 0.5 };
    let mut m = vec![vec![0.0; 6]; 6];
    let mut v = vec![0.0; 6];
    for i in FIT {
        for r in 0..6 {
            v[r] += weight(i) * shape[r][i] * want[i];
            for c in 0..6 {
                m[r][c] += weight(i) * shape[r][i] * shape[c][i];
            }
        }
    }
    for (r, row) in m.iter_mut().enumerate() {
        row[r] += 0.6;
    }
    let g = solve(m, v);
    let mut out = [0; 6];
    for b in 0..6 {
        let step = g[b].round().clamp(-3.0, 3.0) as i32;
        let mut new = (eq[b].1 + step).clamp(-9, 9);
        if b == 5 {
            new = new.min(3.max(eq[b].1)); // never push the top band past +3 (it lifts hiss)
        }
        out[b] = new - eq[b].1;
    }
    (out, worst)
}

/// A proposed set of settings with the reasons, for the window.
pub struct Proposal {
    pub settings: MicSettings,
    pub changes: Vec<Value>,
    pub notes: Vec<String>,
}

fn change(list: &mut Vec<Value>, what: &str, from: String, to: String, why: String) {
    if from != to {
        list.push(json!({ "what": what, "from": minus(&from), "to": minus(&to), "why": minus(&why) }));
    }
}

/// Typographic minus signs for display, and no "−0".
fn minus(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c == '-' && chars.get(i + 1).is_some_and(|n| n.is_ascii_digit()) {
            let zero = chars.get(i + 1) == Some(&'0') && !chars.get(i + 2).is_some_and(|n| n.is_ascii_digit() || *n == '.');
            if !zero {
                out.push('−');
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn signed(v: i32) -> String {
    if v > 0 { format!("+{v} dB") } else { format!("{v} dB") }
}

/// Settings for the measured take. `full` = the whole test (sets everything); otherwise a check
/// after trying settings (only make-up gain, EQ and de-esser are refined).
pub fn propose(a: &Analysis, cur: &MicSettings, full: bool) -> Proposal {
    let mut s = *cur;
    let mut changes = Vec::new();
    let mut notes = Vec::new();

    // Gain: 250 ms peaks in the middle of the official Good zone, shouts below −3 dB.
    let mut d = 0;
    if full {
        let mut delta = -11.0 - a.peak250[0];
        if let Some(loud) = a.loud_max {
            let headroom = if a.clipped > 0 { -6.0 } else { -3.0 };
            delta = delta.min(headroom - loud);
        } else if a.clipped > 0 {
            delta = delta.min(-3.0);
        }
        let delta = if delta.abs() < 2.0 { 0 } else { delta.round() as i32 };
        s.gain = (cur.gain + delta).clamp(0, 72);
        d = s.gain - cur.gain;
        if cur.gain + delta > 72 {
            notes.push(
                "Your voice is quiet even at full gain. Move a little closer to the mic (5–10 cm) and run the test again.".into(),
            );
        }
        let why = if d < 0 {
            format!(
                "Your speech peaks reached {:.0} dB{} (Good zone −17…−5 dB).",
                a.peak250[1].max(a.loud_max.unwrap_or(-96.0)),
                if a.clipped > 0 { " and clipped" } else { "" }
            )
        } else {
            format!("Your speech peaks sat around {:.0} dB (Good zone −17…−5 dB).", a.peak250[0])
        };
        change(&mut changes, "Gain", format!("{} dB", cur.gain), format!("{} dB", s.gain), why);
    }
    let d = f64::from(d);

    if full {
        // Gate: above the room, below your quiet voice.
        let lower = a.noise_p90 + d + 6.0;
        let quiet = a.quiet_rms.unwrap_or(a.speech_rms[0]) + d;
        let upper = quiet - 6.0;
        let thr = if lower <= upper { (lower + 3.0).min(upper) } else { upper };
        if lower > upper {
            notes.push("The room noise is close to your quiet voice, so the gate can't separate them well. A quieter room or a closer mic helps.".into());
        }
        s.gate = [(thr.round() as i32).clamp(-59, 0), 60, 0, 19];
        change(
            &mut changes,
            "Gate threshold",
            format!("{} dB", cur.gate[0]),
            format!("{} dB", s.gate[0]),
            format!("Room noise {:.0} dB, your quietest speech {:.0} dB (after the gain change).", a.noise + d, quiet),
        );
        change(
            &mut changes,
            "Gate attenuation",
            format!("{}%", cur.gate[1]),
            "60%".into(),
            "Turns noise down between words without a hard cut.".into(),
        );
        change(
            &mut changes,
            "Gate attack / release",
            format!("{} / {} ms", gate_ms(cur.gate[2]), gate_ms(cur.gate[3])),
            "10 / 200 ms".into(),
            "Opens fast, closes gently.".into(),
        );

        // Compressor: 4:1 from your average speech level, gentle timing.
        let thr = (a.speech_rms[1] + d).round().clamp(-40.0, 0.0) as i32;
        s.comp[0] = thr;
        s.comp[1] = 9;
        s.comp[2] = 9;
        s.comp[3] = 9;
        // Make-up: processed 250 ms peaks near −9 dB. The compressor follows the average level more
        // than the peaks, so the reduction it takes off the peaks is estimated from how far the
        // loud end of the speech RMS (p90) sits above the threshold, at 4:1.
        let peak = a.peak250[0] + d;
        let reduction = (a.speech_rms[2] + d - f64::from(thr)).max(0.0) * (1.0 - 1.0 / RATIOS[9]);
        s.comp[4] = (-9.0 - peak + reduction).round().clamp(-6.0, 24.0) as i32;
        change(
            &mut changes,
            "Compressor threshold",
            format!("{} dB", cur.comp[0]),
            format!("{thr} dB"),
            format!("Your average speech level is {:.0} dB.", a.speech_rms[1] + d),
        );
        change(
            &mut changes,
            "Compressor ratio",
            format!("{}:1", RATIOS[cur.comp[1].clamp(0, 14) as usize]),
            "4:1".into(),
            "Evens out your level like a podcast, without squashing.".into(),
        );
        change(
            &mut changes,
            "Compressor attack / release",
            format!(
                "{} / {} ms",
                COMP_ATTACK_MS[cur.comp[2].clamp(0, 19) as usize],
                COMP_RELEASE_MS[cur.comp[3].clamp(0, 19) as usize]
            ),
            "10 / 100 ms".into(),
            "Keeps the start of words natural.".into(),
        );
    } else {
        // Make-up from the measured processed level of the check.
        let mut corr = -9.0 - a.proc_peak250;
        corr = corr.min(-1.5 - a.proc_max);
        if corr.abs() >= 2.0 {
            s.comp[4] = (cur.comp[4] + corr.round() as i32).clamp(-6, 24);
        }
    }
    if s.comp[4] != cur.comp[4] {
        change(
            &mut changes,
            "Make-up gain",
            signed(cur.comp[4]),
            signed(s.comp[4]),
            if full {
                "Brings your level back up after compression (peaks near −9 dB).".into()
            } else {
                format!("Your processed peaks were {:.0} dB (aim −9 dB, loudest below −1.5 dB).", a.proc_peak250)
            },
        );
    }

    // EQ toward the reference speech spectrum plus a little presence.
    let (steps, worst) = eq_fit(a, &cur.eq);
    for b in 0..6 {
        s.eq[b].1 = cur.eq[b].1 + steps[b];
        let f = cur.eq[b].0;
        let name = if f >= 1000.0 { format!("EQ {:.1} kHz", f / 1000.0).replace(".0 ", " ") } else { format!("EQ {f:.0} Hz") };
        change(
            &mut changes,
            &name,
            signed(cur.eq[b].1),
            signed(s.eq[b].1),
            if steps[b] < 0 {
                "Your voice is stronger here than a balanced podcast voice.".into()
            } else {
                "Your voice is weaker here than a balanced podcast voice.".into()
            },
        );
    }
    if !full && worst <= 3.0 && steps.iter().all(|&x| x == 0) {
        notes.push("The tone is within 3 dB of the target everywhere.".into());
    }

    // De-esser: the processed "s" sounds shouldn't be much brighter than your dry voice.
    if let Some((dry, proc)) = a.sib {
        // The EQ change moves the ratio too: count it before touching the de-esser.
        let response = |i: usize| {
            (0..6).map(|b| peaking_db(CENTRES[i], f64::from(cur.eq[b].0), f64::from(steps[b]), 1.0, a.fs)).sum::<f64>()
        };
        let mean = |r: std::ops::RangeInclusive<usize>| r.clone().map(response).sum::<f64>() / r.count() as f64;
        let excess = proc - dry + mean(19..=22) - mean(12..=18);
        if excess > 3.0 {
            s.deess = (cur.deess + 10 * ((excess - 3.0) / 2.0).ceil() as i32).min(60.max(cur.deess));
        } else if excess < -3.0 && cur.deess > 10 {
            s.deess = (cur.deess - 10).max(10);
        }
        change(
            &mut changes,
            "De-esser",
            format!("{}%", cur.deess),
            format!("{}%", s.deess),
            format!("\"S\" sounds come out {excess:+.1} dB brighter than from the mic itself (counting the EQ change)."),
        );
    }

    if a.pops > 0 {
        notes.push(format!(
            "We heard {} pop{} on the P sounds. A pop filter, or angling the mic slightly off your mouth, fixes that.",
            a.pops,
            if a.pops == 1 { "" } else { "s" }
        ));
    }
    if a.clipped > 0 && full {
        notes.push(format!("Your mic clipped ({} samples at full scale). The new gain leaves room for shouts.", a.clipped));
    }
    if a.speech_rms[1] - a.noise < 20.0 {
        notes.push(format!(
            "Your voice is only {:.0} dB above the room noise. A quieter room or a closer mic will help more than any setting.",
            a.speech_rms[1] - a.noise
        ));
    }
    if full && a.loud_max.is_none() {
        notes.push("We didn't hear the loud line, so the gain is set from your normal voice only.".into());
    }
    Proposal { settings: s, changes, notes: notes.iter().map(|n| minus(n)).collect() }
}

/// Discord's "Input Sensitivity" (dB) between the processed room noise and your quiet voice. An
/// approximation: Discord doesn't document its meter scale.
pub fn discord_sensitivity(a: &Analysis) -> i32 {
    let quiet = a.proc_quiet.unwrap_or(a.proc_speech_rms - 10.0);
    (((a.proc_noise + quiet) / 2.0).round() as i32).clamp(-100, 0)
}

// ---------- audio files ----------

/// 16-bit mono WAV.
pub fn wav(x: &[f32], fs: f64, gain: f64) -> Vec<u8> {
    let n = x.len() as u32;
    let rate = fs as u32;
    let mut out = Vec::with_capacity(44 + x.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + n * 2).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(n * 2).to_le_bytes());
    for &v in x {
        out.extend_from_slice(&(((f64::from(v) * gain).clamp(-1.0, 1.0) * 32767.0).round() as i16).to_le_bytes());
    }
    out
}

/// Linear gains that bring two clips to the same loudness (−18 LUFS, lowered together if a peak
/// would pass −1 dBFS).
pub fn matched_gains(a: &[f32], b: &[f32], fs: f64) -> (f64, f64) {
    let gain = |x: &[f32]| 10f64.powf((-18.0 - loudness(x, fs)) / 20.0);
    let (ga, gb) = (gain(a), gain(b));
    let peak = |x: &[f32], g: f64| f64::from(x.iter().fold(0f32, |m, v| m.max(v.abs()))) * g;
    let worst = peak(a, ga).max(peak(b, gb));
    let limit = 10f64.powf(-1.0 / 20.0);
    let k = if worst > limit { limit / worst } else { 1.0 };
    (ga * k, gb * k)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f64 = 48000.0;

    /// A crude "voice": harmonics of `f0` with a falling spectrum, gated on and off like syllables.
    fn voice(secs: f64, f0: f64, level: f64, seed: u32) -> Vec<f32> {
        let n = (secs * FS) as usize;
        let mut r = seed;
        (0..n)
            .map(|i| {
                let t = i as f64 / FS;
                let on = ((t * 4.0).fract() < 0.7) as i32 as f64;
                r = r.wrapping_mul(1_103_515_245).wrapping_add(12345);
                let noise = (f64::from(r >> 16) / 32768.0 - 1.0) * 0.02;
                let mut v = 0.0;
                for h in 1..40 {
                    let f = f0 * h as f64;
                    if f > 16000.0 {
                        break;
                    }
                    v += (2.0 * std::f64::consts::PI * f * t).sin() / h as f64;
                }
                ((v * 0.3 + noise) * on * level) as f32
            })
            .collect()
    }

    fn silence(secs: f64, level: f64) -> Vec<f32> {
        let mut r = 7u32;
        (0..(secs * FS) as usize)
            .map(|_| {
                r = r.wrapping_mul(1_103_515_245).wrapping_add(12345);
                ((f64::from(r >> 16) / 32768.0 - 1.0) * level) as f32
            })
            .collect()
    }

    fn take(level: f64) -> (Vec<f32>, Vec<Segment>) {
        let mut x = silence(5.0, 0.002);
        let mut segs = vec![Segment { key: "silence", start: 0, end: x.len() }];
        for (key, secs, l) in [
            ("read", 14.0, level),
            ("sibilance", 4.0, level),
            ("plosive", 4.0, level),
            ("loud", 3.0, level * 1.6),
            ("quiet", 4.0, level * 0.35),
        ] {
            let start = x.len();
            x.extend(voice(secs, 110.0, l, start as u32));
            segs.push(Segment { key, start, end: x.len() });
        }
        (x, segs)
    }

    /// Synthetic starting settings: round values, the Mini's default EQ centres, flat EQ.
    fn current() -> MicSettings {
        MicSettings {
            gain: 40,
            eq: [(90.0, 0), (160.0, 0), (480.0, 0), (1500.0, 0), (4500.0, 0), (7800.0, 0)],
            gate: [-40, 50, 0, 19],
            comp: [-20, 9, 9, 9, 5],
            deess: 20,
        }
    }

    #[test]
    fn fft_finds_a_tone() {
        let x: Vec<f32> = (0..SEG).map(|i| (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / FS).sin() as f32).collect();
        let p = power_spectrum(&x);
        let peak = p.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap().0;
        assert!(((peak as f64 * FS / SEG as f64) - 1000.0).abs() < 15.0);
    }

    #[test]
    fn loudness_of_a_full_scale_997_hz_sine_is_about_minus_3() {
        let x: Vec<f32> =
            (0..(FS * 3.0) as usize).map(|i| (2.0 * std::f64::consts::PI * 997.0 * i as f64 / FS).sin() as f32).collect();
        assert!((loudness(&x, FS) + 3.01).abs() < 0.15, "{}", loudness(&x, FS));
    }

    #[test]
    fn peaking_model_peaks_at_its_centre() {
        assert!((peaking_db(1000.0, 1000.0, 6.0, 1.0, FS) - 6.0).abs() < 0.01);
        assert!(peaking_db(100.0, 1000.0, 6.0, 1.0, FS).abs() < 0.5);
    }

    #[test]
    fn short_take_is_rejected() {
        let (x, mut segs) = take(0.3);
        segs[1].end = segs[1].start + (FS * 3.0) as usize;
        segs.retain(|s| s.key == "silence" || s.key == "read");
        let mut x2 = x.clone();
        for v in &mut x2[segs[1].start..] {
            *v *= 0.0005;
        }
        assert!(analyse(&x2, &x2, FS, &segs, true).is_err());
    }

    #[test]
    fn muted_processed_mic_is_reported() {
        let (x, segs) = take(0.3);
        let quiet = vec![0f32; x.len()];
        let e = analyse(&x, &quiet, FS, &segs, true).unwrap_err();
        assert!(e.contains("muted"), "{e}");
    }

    #[test]
    fn hot_take_lowers_the_gain_and_quiet_take_raises_it() {
        let (hot, segs) = take(0.9);
        let a = analyse(&hot, &hot, FS, &segs, true).unwrap();
        let p = propose(&a, &current(), true);
        assert!(p.settings.gain < current().gain, "gain {}", p.settings.gain);

        let (soft, segs) = take(0.05);
        let a = analyse(&soft, &soft, FS, &segs, true).unwrap();
        let p = propose(&a, &current(), true);
        assert!(p.settings.gain > current().gain, "gain {}", p.settings.gain);
        assert!(p.settings.gate[0] >= -59 && p.settings.gate[0] <= 0);
        assert_eq!(p.settings.comp[1], 9);
    }

    #[test]
    fn pitch_of_the_test_voice() {
        let (x, segs) = take(0.3);
        let a = analyse(&x, &x, FS, &segs, true).unwrap();
        assert!((a.pitch - 110.0).abs() < 5.0, "{}", a.pitch);
    }

    #[test]
    fn check_take_corrects_make_up_only_when_off() {
        let (x, segs) = take(0.3);
        let check: Vec<Segment> = segs
            .iter()
            .filter(|s| matches!(s.key, "silence" | "read" | "loud"))
            .map(|s| Segment { key: if s.key == "read" { "read2" } else { s.key }, ..s.clone() })
            .collect();
        let a = analyse(&x, &x, FS, &check, false).unwrap();
        let p = propose(&a, &current(), false);
        assert_eq!(p.settings.gain, current().gain);
        assert_eq!(p.settings.gate, current().gate);
        let makeup = current().comp[4];
        let expected = (f64::from(makeup) + (-9.0 - a.proc_peak250).min(-1.5 - a.proc_max).round()).clamp(-6.0, 24.0) as i32;
        if (-9.0 - a.proc_peak250).min(-1.5 - a.proc_max).abs() >= 2.0 {
            assert_eq!(p.settings.comp[4], expected);
        } else {
            assert_eq!(p.settings.comp[4], makeup);
        }
    }

    #[test]
    fn wav_header_is_44_bytes() {
        let w = wav(&[0.0, 0.5, -0.5], FS, 1.0);
        assert_eq!(w.len(), 44 + 6);
        assert_eq!(&w[0..4], b"RIFF");
        assert_eq!(i16::from_le_bytes([w[46], w[47]]), 16384);
    }

    #[test]
    fn display_minus() {
        assert_eq!(minus("-0 dB"), "0 dB");
        assert_eq!(minus("-45 dB, -0.5"), "−45 dB, −0.5");
        assert_eq!(minus("10-20 Hz"), "10−20 Hz");
        assert_eq!(minus("a - b"), "a - b");
    }

    #[test]
    fn gate_times() {
        assert_eq!(gate_ms(0), 10);
        assert_eq!(gate_ms(19), 200);
        assert_eq!(gate_ms(20), 250);
        assert_eq!(gate_ms(45), 2000);
    }
}
