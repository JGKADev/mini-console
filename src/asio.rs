//! A minimal ASIO host for the GoXLR driver, used by the mic auto-tune to record the dry
//! (unprocessed) and processed mic at the same time. Recording only: it sends nothing to the Mini.
//!
//! ASIO drivers are COM objects without a type library: the driver's CLSID doubles as the
//! interface id, and the methods are a plain vtable. Callbacks carry no user pointer, so the one
//! recording in progress lives in a static.
//!
//! The driver's callback runs on its real-time thread, so it never takes a lock or allocates: the
//! sample buffers are allocated up front, and the length and peak level are atomics.

// The vtable below is declared "system", which is the right calling convention for the driver's
// C++ methods only on 64-bit Windows (32-bit drivers use thiscall).
#[cfg(not(target_arch = "x86_64"))]
compile_error!("the ASIO host supports only x86_64 Windows");

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Result, bail};
use windows_sys::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows_sys::Win32::UI::WindowsAndMessaging::GetDesktopWindow;
use windows_sys::core::GUID;

/// HKLM\SOFTWARE\ASIO\GoXLR ASIO Driver\CLSID
const GOXLR_ASIO: GUID = GUID::from_u128(0x058274c2_505d_456b_ba18_d77dffaf0bf7);
/// Fallback channel numbers if the names change: 20 = "GoXLR-Dry Mic (unprocessed)",
/// 6 = "GoXLR-Mic L (processed)" (driver 5.57).
const DRY_CHANNEL: i32 = 20;
const PROCESSED_CHANNEL: i32 = 6;
/// Recording stops growing after this long (memory guard).
const MAX_SECONDS: usize = 180;

type This = *mut c_void;

#[repr(C)]
struct Vtbl {
    _query_interface: usize,
    _add_ref: usize,
    release: unsafe extern "system" fn(This) -> u32,
    init: unsafe extern "system" fn(This, *mut c_void) -> i32,
    _get_driver_name: usize,
    _get_driver_version: usize,
    get_error_message: unsafe extern "system" fn(This, *mut u8),
    start: unsafe extern "system" fn(This) -> i32,
    stop: unsafe extern "system" fn(This) -> i32,
    get_channels: unsafe extern "system" fn(This, *mut i32, *mut i32) -> i32,
    _get_latencies: usize,
    get_buffer_size: unsafe extern "system" fn(This, *mut i32, *mut i32, *mut i32, *mut i32) -> i32,
    _can_sample_rate: usize,
    get_sample_rate: unsafe extern "system" fn(This, *mut f64) -> i32,
    _set_sample_rate: usize,
    _get_clock_sources: usize,
    _set_clock_source: usize,
    _get_sample_position: usize,
    get_channel_info: unsafe extern "system" fn(This, *mut ChannelInfo) -> i32,
    create_buffers: unsafe extern "system" fn(This, *mut BufferInfo, i32, i32, *const Callbacks) -> i32,
    dispose_buffers: unsafe extern "system" fn(This) -> i32,
}

#[repr(C)]
struct ChannelInfo {
    channel: i32,
    is_input: i32,
    is_active: i32,
    group: i32,
    sample_type: i32,
    name: [u8; 32],
}

#[repr(C)]
struct BufferInfo {
    is_input: i32,
    channel: i32,
    buffers: [*mut c_void; 2],
}

#[repr(C)]
struct Callbacks {
    buffer_switch: extern "C" fn(i32, i32),
    sample_rate_changed: extern "C" fn(f64),
    message: extern "C" fn(i32, i32, *mut c_void, *mut f64) -> i32,
    buffer_switch_time_info: extern "C" fn(*mut c_void, i32, i32) -> *mut c_void,
}

const ASE_OK: i32 = 0;
const ASE_SUCCESS: i32 = 0x3f48_47a0;
fn asio_ok(r: i32) -> bool {
    r == ASE_OK || r == ASE_SUCCESS
}

/// What the buffer-switch callback needs, set up before the driver starts. Only the callback writes
/// through `out`, and only below `max_len`, so the sample vectors never reallocate.
struct Live {
    /// Per recorded channel: the driver's two half-buffers.
    buffers: Vec<[*mut c_void; 2]>,
    /// Per recorded channel: the ASIO sample type.
    sample_types: Vec<i32>,
    /// Per recorded channel: the start of its preallocated sample storage.
    out: Vec<*mut f32>,
    frames: usize,
    max_len: usize,
}

/// The recording the callback fills; null when none is running.
static LIVE: AtomicPtr<Live> = AtomicPtr::new(std::ptr::null_mut());
/// Callbacks currently inside `on_buffer_switch` (teardown waits for zero).
static IN_CALLBACK: AtomicUsize = AtomicUsize::new(0);
/// Samples recorded per channel so far.
static LEN: AtomicUsize = AtomicUsize::new(0);
/// Peak of the first channel since the level was last read (f32 bits; never negative).
static PEAK: AtomicU32 = AtomicU32::new(0);
/// A driver session exists, from `start` until its thread has let the driver go.
static BUSY: AtomicBool = AtomicBool::new(false);

fn sample(sample_type: i32, p: *const u8, i: usize) -> f32 {
    unsafe {
        match sample_type {
            16 => f32::from(i16::from_le_bytes(*(p.add(i * 2) as *const [u8; 2]))) / 32768.0,
            17 => {
                let b = *(p.add(i * 3) as *const [u8; 3]);
                (i32::from_le_bytes([0, b[0], b[1], b[2]]) as f32) / 2_147_483_648.0
            }
            18 => (i32::from_le_bytes(*(p.add(i * 4) as *const [u8; 4])) as f32) / 2_147_483_648.0,
            19 => f32::from_le_bytes(*(p.add(i * 4) as *const [u8; 4])),
            _ => 0.0,
        }
    }
}

extern "C" fn on_buffer_switch(index: i32, _direct: i32) {
    IN_CALLBACK.fetch_add(1, Ordering::SeqCst);
    let live = LIVE.load(Ordering::SeqCst);
    if !live.is_null() {
        // Safe: teardown clears LIVE and waits for IN_CALLBACK to drain before freeing it.
        let live = unsafe { &*live };
        let len = LEN.load(Ordering::Relaxed);
        if len + live.frames <= live.max_len {
            let half = (index & 1) as usize;
            let mut peak = 0f32;
            for k in 0..live.buffers.len() {
                let p = live.buffers[k][half] as *const u8;
                for i in 0..live.frames {
                    let s = if p.is_null() { 0.0 } else { sample(live.sample_types[k], p, i) };
                    if k == 0 {
                        peak = peak.max(s.abs());
                    }
                    unsafe { live.out[k].add(len + i).write(s) };
                }
            }
            // Non-negative floats order the same as their bits.
            PEAK.fetch_max(peak.to_bits(), Ordering::Relaxed);
            LEN.store(len + live.frames, Ordering::Release);
        }
    }
    IN_CALLBACK.fetch_sub(1, Ordering::SeqCst);
}

extern "C" fn on_rate_changed(_rate: f64) {}

extern "C" fn on_message(selector: i32, value: i32, _message: *mut c_void, _opt: *mut f64) -> i32 {
    match selector {
        1 => i32::from(matches!(value, 1 | 2)), // selector supported: only these two
        2 => 2,                                 // engine version
        _ => 0,                                 // no time info: the driver calls bufferSwitch
    }
}

extern "C" fn on_buffer_switch_time_info(params: *mut c_void, index: i32, direct: i32) -> *mut c_void {
    on_buffer_switch(index, direct);
    params
}

static CALLBACKS: Callbacks = Callbacks {
    buffer_switch: on_buffer_switch,
    sample_rate_changed: on_rate_changed,
    message: on_message,
    buffer_switch_time_info: on_buffer_switch_time_info,
};

/// A recording of the dry mic (channel 0) and processed mic (channel 1).
pub struct Recorder {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<Result<Vec<Vec<f32>>>>>,
    pub sample_rate: f64,
}

/// What a finished recording returns.
pub struct Take {
    pub sample_rate: f64,
    pub dry: Vec<f32>,
    pub processed: Vec<f32>,
}

impl Recorder {
    /// Opens the GoXLR ASIO driver and starts recording. Fails if another program holds ASIO.
    pub fn start() -> Result<Self> {
        if BUSY.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
            bail!("a recording is already running");
        }
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<Result<f64>>();
        let stop2 = stop.clone();
        // The session thread clears BUSY when it lets the driver go, whatever happens here.
        let thread = std::thread::spawn(move || run(stop2, tx));
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(sample_rate)) => Ok(Self { stop, thread: Some(thread), sample_rate }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                // If the driver starts later, the session sees nobody waiting and shuts it down.
                stop.store(true, Ordering::SeqCst);
                bail!("the GoXLR ASIO driver didn't start in time")
            }
        }
    }

    /// Samples recorded so far.
    pub fn len(&self) -> usize {
        LEN.load(Ordering::Acquire)
    }

    /// Peak level of the dry mic since the last call, in dBFS.
    pub fn take_peak_db(&self) -> f64 {
        let peak = f32::from_bits(PEAK.swap(0, Ordering::Relaxed));
        if peak <= 0.0 { -96.0 } else { 20.0 * f64::from(peak).log10() }
    }

    /// Stops and returns the audio.
    pub fn finish(mut self) -> Result<Take> {
        self.stop.store(true, Ordering::SeqCst);
        let thread = self.thread.take().ok_or_else(|| anyhow::anyhow!("no recording"))?;
        let data = thread.join().map_err(|_| anyhow::anyhow!("the recording thread panicked"))??;
        let mut data = data.into_iter();
        Ok(Take {
            sample_rate: self.sample_rate,
            dry: data.next().unwrap_or_default(),
            processed: data.next().unwrap_or_default(),
        })
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The driver session, on its own thread: COM apartment, init, buffers, start; then waits for the
/// stop flag and tears everything down in reverse. Returns the recorded channels.
fn run(stop: Arc<AtomicBool>, started: mpsc::Sender<Result<f64>>) -> Result<Vec<Vec<f32>>> {
    // S_OK and S_FALSE both need a matching CoUninitialize; a failure (another apartment) doesn't.
    struct Busy;
    impl Drop for Busy {
        fn drop(&mut self) {
            BUSY.store(false, Ordering::SeqCst);
        }
    }
    let _busy = Busy;
    let com = unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) } >= 0;
    let result = session(&stop, &started);
    if let Err(e) = &result {
        let _ = started.send(Err(anyhow::anyhow!("{e:#}")));
    }
    if com {
        unsafe { CoUninitialize() };
    }
    result
}

/// Unpublishes the recording and waits until no callback is still reading it.
fn unpublish() {
    LIVE.store(std::ptr::null_mut(), Ordering::SeqCst);
    while IN_CALLBACK.load(Ordering::SeqCst) != 0 {
        std::thread::yield_now();
    }
}

fn session(stop: &AtomicBool, started: &mpsc::Sender<Result<f64>>) -> Result<Vec<Vec<f32>>> {
    let mut obj: This = std::ptr::null_mut();
    let hr = unsafe { CoCreateInstance(&GOXLR_ASIO, std::ptr::null_mut(), CLSCTX_INPROC_SERVER, &GOXLR_ASIO, &mut obj) };
    if hr < 0 || obj.is_null() {
        bail!("the GoXLR ASIO driver isn't installed (error {hr:#x})");
    }
    let vt = unsafe { &**(obj as *const *const Vtbl) };
    let release = || unsafe { (vt.release)(obj) };

    let error_text = || {
        let mut buf = [0u8; 128];
        unsafe { (vt.get_error_message)(obj, buf.as_mut_ptr()) };
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..end]).into_owned()
    };
    if unsafe { (vt.init)(obj, GetDesktopWindow()) } == 0 {
        let msg = error_text();
        release();
        bail!("the GoXLR ASIO driver didn't open ({msg}). Another program (a DAW or OBS) may be using it");
    }

    let setup = || -> Result<(Vec<BufferInfo>, i32, Vec<i32>, f64)> {
        let (mut inputs, mut outputs) = (0, 0);
        unsafe { (vt.get_channels)(obj, &mut inputs, &mut outputs) };
        let find = |want: &dyn Fn(&str) -> bool, fallback: i32| -> (i32, i32) {
            let mut sample_type = 18;
            for c in 0..inputs {
                let mut info = ChannelInfo { channel: c, is_input: 1, is_active: 0, group: 0, sample_type: 0, name: [0; 32] };
                if asio_ok(unsafe { (vt.get_channel_info)(obj, &mut info) }) {
                    let end = info.name.iter().position(|&b| b == 0).unwrap_or(32);
                    let name = String::from_utf8_lossy(&info.name[..end]).to_lowercase();
                    if c == fallback {
                        sample_type = info.sample_type;
                    }
                    if want(&name) {
                        return (c, info.sample_type);
                    }
                }
            }
            (fallback, sample_type)
        };
        let (dry, dry_type) = find(&|n| n.contains("dry mic"), DRY_CHANNEL);
        let (processed, processed_type) = find(&|n| n.contains("mic l (processed)"), PROCESSED_CHANNEL);
        if dry >= inputs || processed >= inputs {
            bail!("the GoXLR ASIO driver has only {inputs} inputs");
        }
        for t in [dry_type, processed_type] {
            if !matches!(t, 16..=19) {
                bail!("unsupported ASIO sample format {t}");
            }
        }
        let (mut min, mut max, mut preferred, mut granularity) = (0, 0, 0, 0);
        unsafe { (vt.get_buffer_size)(obj, &mut min, &mut max, &mut preferred, &mut granularity) };
        let mut rate = 0.0;
        unsafe { (vt.get_sample_rate)(obj, &mut rate) };
        let infos = [dry, processed].map(|c| BufferInfo { is_input: 1, channel: c, buffers: [std::ptr::null_mut(); 2] });
        Ok((infos.into(), preferred.max(32), vec![dry_type, processed_type], if rate > 0.0 { rate } else { 48000.0 }))
    };
    let (mut infos, frames, sample_types, rate) = match setup() {
        Ok(x) => x,
        Err(e) => {
            release();
            return Err(e);
        }
    };
    let r = unsafe { (vt.create_buffers)(obj, infos.as_mut_ptr(), infos.len() as i32, frames, &CALLBACKS) };
    if !asio_ok(r) {
        let msg = error_text();
        release();
        bail!("the GoXLR ASIO driver refused the buffers ({msg})");
    }

    // All the memory the recording can use, allocated now so the callback never reallocates.
    let max_len = rate as usize * MAX_SECONDS;
    let mut data: Vec<Vec<f32>> = infos.iter().map(|_| Vec::with_capacity(max_len)).collect();
    let live = Box::into_raw(Box::new(Live {
        buffers: infos.iter().map(|b| b.buffers).collect(),
        sample_types,
        out: data.iter_mut().map(|d| d.as_mut_ptr()).collect(),
        frames: frames as usize,
        max_len,
    }));
    LEN.store(0, Ordering::SeqCst);
    PEAK.store(0, Ordering::SeqCst);
    LIVE.store(live, Ordering::SeqCst);

    // Stops the driver and frees everything; returns the samples recorded.
    let teardown = |data: &mut Vec<Vec<f32>>, running: bool| {
        if running {
            unsafe { (vt.stop)(obj) };
        }
        unpublish();
        unsafe {
            (vt.dispose_buffers)(obj);
            drop(Box::from_raw(live));
        }
        release();
        let len = LEN.load(Ordering::Acquire);
        for d in data.iter_mut() {
            // Safe: the callback wrote the first `len` samples of every channel, below the capacity.
            unsafe { d.set_len(len) };
        }
    };

    if !asio_ok(unsafe { (vt.start)(obj) }) {
        let msg = error_text();
        teardown(&mut data, false);
        bail!("the GoXLR ASIO driver didn't start ({msg})");
    }
    if started.send(Ok(rate)).is_err() {
        // `start` gave up waiting: nobody will own this recording.
        teardown(&mut data, true);
        bail!("the recording was abandoned");
    }
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(10));
    }
    teardown(&mut data, true);
    Ok(data)
}
