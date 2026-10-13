//! Thin binding to the GoXLR driver's Thesycon TUSBAUDIO API (`goxlr_audioapi_x64.dll`).
//!
//! Signatures match the arguments observed live from the official app's calls into the same dll
//! (vendor requests: handle, recipient 1, type 0, request, value 0, index 0, length, buffer,
//! transferred, 200 ms timeout).

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use libloading::Library;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_TIMEOUT};
use windows_sys::Win32::System::Registry::{HKEY_CLASSES_ROOT, RRF_RT_REG_SZ, RegGetValueW};
use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

type FnVoid = unsafe extern "C" fn() -> u32;
type FnOpenByIndex = unsafe extern "C" fn(u32, *mut u32) -> u32;
type FnClose = unsafe extern "C" fn(u32) -> u32;
type FnProperties = unsafe extern "C" fn(u32, *mut Properties) -> u32;
type FnVendorOut = unsafe extern "C" fn(u32, u32, u32, u32, u32, u16, u16, *const u8, *mut u32, u32) -> u32;
type FnVendorIn = unsafe extern "C" fn(u32, u32, u32, u32, u32, u16, u16, *mut u8, *mut u32, u32) -> u32;
type FnRegisterNotification = unsafe extern "C" fn(u32, u32, HANDLE, u32) -> u32;
type FnReadNotification = unsafe extern "C" fn(u32, *const u32, *mut u8, u32, *mut u32) -> u32;

/// Driver status returned by ReadDeviceNotification when the queue is empty.
const STATUS_NO_MORE_NOTIFICATIONS: u32 = 3_992_977_442;
/// Transfer timeout passed to the vendor-request calls (the official app uses 200 ms).
const TRANSFER_TIMEOUT_MS: u32 = 200;
/// COM class id under which the driver registers its API dll.
const API_CLSID_KEY: &str = r"CLSID\{024D0372-641F-4B7B-8140-F4DFE458C982}\InprocServer32";
const FALLBACK_DLL: &str = r"C:\Program Files\TC-Helicon\GoXLR_Audio_Driver\x64\goxlr_audioapi_x64.dll";

pub const VID_TC_HELICON: i32 = 0x1220;
pub const PID_GOXLR_MINI: i32 = 0x8fe4;

#[repr(C)]
pub struct Properties {
    pub vendor_id: i32,
    pub product_id: i32,
    pub revision: i32,
    serial: [u16; 128],
    manufacturer: [u16; 128],
    model: [u16; 128],
    _unknown_number: i32,
    _unknown_string: [u16; 128],
}

impl Properties {
    pub fn model(&self) -> String {
        wide_to_string(&self.model)
    }
    pub fn manufacturer(&self) -> String {
        wide_to_string(&self.manufacturer)
    }
}

/// Kind of notification the driver delivers after `RegisterDeviceNotification`.
#[derive(Debug, PartialEq, Eq)]
pub enum Notification {
    /// A DCP reply is waiting to be read with bRequest 3.
    DataReady,
    /// A button, fader or encoder changed on the hardware.
    InputChanged,
    Other,
}

/// The driver's API dll, with every function this app calls resolved once at load.
pub struct Api {
    enumerate: FnVoid,
    device_count: FnVoid,
    open_by_index: FnOpenByIndex,
    close: FnClose,
    properties: FnProperties,
    vendor_out: FnVendorOut,
    vendor_in: FnVendorIn,
    register_notification: FnRegisterNotification,
    read_notification: FnReadNotification,
    /// Keeps the dll loaded while the function pointers above are in use.
    _lib: Library,
}

impl Api {
    pub fn load() -> Result<Self> {
        let path = locate_dll();
        let lib = unsafe { Library::new(&path) }.with_context(|| format!("loading {}", path.display()))?;
        fn sym<T: Copy>(lib: &Library, name: &str) -> Result<T> {
            let symbol = format!("TUSBAUDIO_{name}");
            unsafe { lib.get::<T>(symbol.as_bytes()).map(|s| *s) }
                .with_context(|| format!("TUSBAUDIO_{name} missing from the driver dll"))
        }
        Ok(Self {
            enumerate: sym(&lib, "EnumerateDevices")?,
            device_count: sym(&lib, "GetDeviceCount")?,
            open_by_index: sym(&lib, "OpenDeviceByIndex")?,
            close: sym(&lib, "CloseDevice")?,
            properties: sym(&lib, "GetDeviceProperties")?,
            vendor_out: sym(&lib, "ClassVendorRequestOut")?,
            vendor_in: sym(&lib, "ClassVendorRequestIn")?,
            register_notification: sym(&lib, "RegisterDeviceNotification")?,
            read_notification: sym(&lib, "ReadDeviceNotification")?,
            _lib: lib,
        })
    }

    pub fn device_count(&self) -> Result<u32> {
        unsafe {
            (self.enumerate)();
            Ok((self.device_count)())
        }
    }

    pub fn open(&self, index: u32) -> Result<u32> {
        let mut handle = 0u32;
        let status = unsafe { (self.open_by_index)(index, &mut handle) };
        check(status, "OpenDeviceByIndex")?;
        Ok(handle)
    }

    pub fn close(&self, handle: u32) {
        unsafe { (self.close)(handle) };
    }

    pub fn properties(&self, handle: u32) -> Result<Box<Properties>> {
        let mut props: Box<Properties> = Box::new(unsafe { std::mem::zeroed() });
        let status = unsafe { (self.properties)(handle, &mut *props) };
        check(status, "GetDeviceProperties")?;
        Ok(props)
    }

    /// Host-to-device vendor request (recipient interface, class/vendor type 0).
    pub fn vendor_out(&self, handle: u32, request: u8, data: &[u8]) -> Result<()> {
        let mut transferred = 0u32;
        let len: u16 = data.len().try_into().context("request too long")?;
        let status = unsafe {
            (self.vendor_out)(handle, 1, 0, request.into(), 0, 0, len, data.as_ptr(), &mut transferred, TRANSFER_TIMEOUT_MS)
        };
        check(status, "ClassVendorRequestOut")
    }

    /// Device-to-host vendor request. Returns the bytes actually transferred.
    pub fn vendor_in(&self, handle: u32, request: u8, max_len: u16) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; max_len as usize];
        let mut transferred = 0u32;
        let status = unsafe {
            (self.vendor_in)(handle, 1, 0, request.into(), 0, 0, max_len, buf.as_mut_ptr(), &mut transferred, TRANSFER_TIMEOUT_MS)
        };
        check(status, "ClassVendorRequestIn")?;
        buf.truncate((transferred as usize).min(buf.len()));
        Ok(buf)
    }
}

/// Auto-reset Win32 event the driver signals when notifications are queued for a handle.
pub struct NotificationEvent<'a> {
    api: &'a Api,
    handle: u32,
    event: HANDLE,
}

impl<'a> NotificationEvent<'a> {
    pub fn register(api: &'a Api, handle: u32) -> Result<Self> {
        let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
        if event.is_null() {
            bail!("CreateEventW failed");
        }
        let status = unsafe { (api.register_notification)(handle, u32::MAX, event, 0) };
        if status != 0 {
            unsafe { CloseHandle(event) };
            bail!("RegisterDeviceNotification failed: {status:#x}");
        }
        Ok(Self { api, handle, event })
    }

    /// Waits up to `timeout_ms` for the event, then drains the notification queue.
    pub fn wait(&self, timeout_ms: u32) -> Result<Vec<Notification>> {
        if unsafe { WaitForSingleObject(self.event, timeout_ms) } == WAIT_TIMEOUT {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            let mut len = 0u32;
            let flags = 0u32;
            let status =
                unsafe { (self.api.read_notification)(self.handle, &flags, buf.as_mut_ptr(), buf.len() as u32, &mut len) };
            if status == STATUS_NO_MORE_NOTIFICATIONS {
                break;
            }
            check(status, "ReadDeviceNotification")?;
            out.push(match &buf[..(len as usize).min(buf.len())] {
                [1, 1, 1, ..] => Notification::DataReady,
                [1, 1, 0, ..] => Notification::InputChanged,
                _ => Notification::Other,
            });
        }
        Ok(out)
    }
}

impl Drop for NotificationEvent<'_> {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.event) };
    }
}

fn check(status: u32, what: &str) -> Result<()> {
    if status == 0 { Ok(()) } else { bail!("{what} failed: status {status:#x}") }
}

fn locate_dll() -> PathBuf {
    let key: Vec<u16> = API_CLSID_KEY.encode_utf16().chain([0]).collect();
    let mut buf = [0u16; 512];
    let mut size = (buf.len() * 2) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CLASSES_ROOT,
            key.as_ptr(),
            std::ptr::null(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if status == 0 {
        let path = PathBuf::from(wide_to_string(&buf));
        if path.exists() {
            return path;
        }
    }
    PathBuf::from(FALLBACK_DLL)
}

fn wide_to_string(w: &[u16]) -> String {
    let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..end])
}
