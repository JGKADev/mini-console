//! Live levels for the PC channels (Chat, Music, Game, System, Sample).
//!
//! The Mini's USB protocol only reports the mic level (0x80c). The other channels are Windows
//! playback devices of the GoXLR driver, so their levels come from Windows instead: the peak meter
//! of each GoXLR render endpoint (what Windows is sending to that channel). Line In and Console
//! are analogue inputs on the Mini and have no meter anywhere.

use windows::Win32::Devices::FunctionDiscovery::{PKEY_Device_DeviceDesc, PKEY_DeviceInterface_FriendlyName};
use windows::Win32::Media::Audio::Endpoints::IAudioMeterInformation;
use windows::Win32::Media::Audio::{DEVICE_STATE_ACTIVE, IMMDeviceEnumerator, MMDeviceEnumerator, eRender};
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, STGM_READ};
use windows::core::PWSTR;

/// Windows device name (as the GoXLR driver names them) → the app's channel name.
const CHANNELS: [(&str, &str); 5] =
    [("chat", "chat"), ("music", "music"), ("game", "game"), ("system", "system"), ("sample", "sample")];

pub struct Meters {
    found: Vec<(&'static str, IAudioMeterInformation)>,
}

fn take_string(p: PWSTR) -> String {
    let s = unsafe { p.to_string() }.unwrap_or_default();
    unsafe { CoTaskMemFree(Some(p.0 as *const _)) };
    s
}

impl Meters {
    /// Finds the GoXLR playback devices. Call on the thread that will read the meters.
    pub fn open() -> windows::core::Result<Self> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let devices = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
            // A device that fails to answer is skipped: the others still get meters.
            let meter = |i: u32| -> windows::core::Result<Option<(&'static str, IAudioMeterInformation)>> {
                let device = devices.Item(i)?;
                let props = device.OpenPropertyStore(STGM_READ)?;
                let interface = take_string(PropVariantToStringAlloc(&props.GetValue(&PKEY_DeviceInterface_FriendlyName)?)?);
                if !interface.to_lowercase().contains("goxlr") {
                    return Ok(None);
                }
                let name = take_string(PropVariantToStringAlloc(&props.GetValue(&PKEY_Device_DeviceDesc)?)?).to_lowercase();
                match CHANNELS.iter().find(|(n, _)| name == *n) {
                    Some((_, channel)) => Ok(Some((*channel, device.Activate::<IAudioMeterInformation>(CLSCTX_ALL, None)?))),
                    None => Ok(None),
                }
            };
            let found = (0..devices.GetCount()?).filter_map(|i| meter(i).ok().flatten()).collect();
            Ok(Self { found })
        }
    }

    pub fn is_empty(&self) -> bool {
        self.found.is_empty()
    }

    /// Peak level of each channel in dBFS (−72 for silence). None if a device went away.
    pub fn read(&self) -> Option<Vec<(&'static str, f64)>> {
        self.found
            .iter()
            .map(|(name, meter)| {
                let peak = unsafe { meter.GetPeakValue() }.ok()?;
                let db = if peak <= 0.0 { -72.0 } else { (20.0 * f64::from(peak).log10()).clamp(-72.0, 0.0) };
                Some((*name, db))
            })
            .collect()
    }
}
