//! Windows desktop integration: one running copy at a time, and the "start with Windows" entry.
//!
//! Autostart is a per-user Run entry (HKCU\Software\Microsoft\Windows\CurrentVersion\Run), the same
//! mechanism the official app uses. It points at the exe that turned it on, with `--minimized`.

use std::path::PathBuf;

use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
use windows_sys::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowW, SW_RESTORE, SetForegroundWindow, ShowWindow};

pub const WINDOW_TITLE: &str = "Mini Console";
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "Mini Console";
/// Run value written by builds from before the rename; removed whenever autostart is changed.
const OLD_RUN_VALUE: &str = "GoXLR Mini Console";
pub const MINIMIZED_FLAG: &str = "--minimized";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Claims the single-instance lock. Returns false (after bringing the open window to the front)
/// when another copy is already running. The lock lives until the process exits.
pub fn claim_single_instance() -> bool {
    let name = wide("Local\\mini-console");
    unsafe {
        let handle = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        // No lock at all (an unexpected failure): run rather than silently exit.
        if handle.is_null() || GetLastError() != ERROR_ALREADY_EXISTS {
            return true; // keep the handle open on purpose
        }
        let title = wide(WINDOW_TITLE);
        let window = FindWindowW(std::ptr::null(), title.as_ptr());
        if !window.is_null() {
            ShowWindow(window, SW_RESTORE);
            SetForegroundWindow(window);
        }
        false
    }
}

fn run_command() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    Some(format!("\"{}\" {MINIMIZED_FLAG}", exe.display()))
}

/// The current Run entry, if any.
fn read_entry() -> Option<String> {
    read_value(RUN_VALUE).or_else(|| read_value(OLD_RUN_VALUE))
}

fn read_value(name: &str) -> Option<String> {
    let (key, value) = (wide(RUN_KEY), wide(name));
    let mut buf = vec![0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if status != 0 {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}

/// Autostart state for the window: whether it is on, and whether it starts this exe.
pub fn autostart() -> (bool, bool) {
    match read_entry() {
        Some(entry) => (true, Some(entry) == run_command()),
        None => (false, false),
    }
}

pub fn set_autostart(on: bool) -> anyhow::Result<()> {
    let (key, value) = (wide(RUN_KEY), wide(RUN_VALUE));
    let old = wide(OLD_RUN_VALUE);
    unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), old.as_ptr()) };
    let status = if on {
        let data = wide(&run_command().ok_or_else(|| anyhow::anyhow!("can't find this program's path"))?);
        unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                value.as_ptr(),
                REG_SZ,
                data.as_ptr().cast(),
                (data.len() * 2) as u32,
            )
        }
    } else {
        match unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), value.as_ptr()) } {
            2 => 0, // ERROR_FILE_NOT_FOUND: already off
            s => s,
        }
    };
    anyhow::ensure!(status == 0, "Windows refused the change (error {status})");
    Ok(())
}

/// A shell folder where the user actually has it. Documents and Desktop are often redirected (for
/// example into OneDrive), so %USERPROFILE%\Documents can be the wrong place.
fn known_folder(id: &windows_sys::core::GUID) -> Option<PathBuf> {
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::SHGetKnownFolderPath;
    let mut path = std::ptr::null_mut();
    let hr = unsafe { SHGetKnownFolderPath(id, 0, std::ptr::null_mut(), &mut path) };
    let found = (hr == 0 && !path.is_null()).then(|| {
        let len = (0..).take_while(|&i| unsafe { *path.add(i) } != 0).count();
        PathBuf::from(String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(path, len) }))
    });
    unsafe { CoTaskMemFree(path.cast()) };
    found
}

fn profile_dir(name: &str) -> PathBuf {
    PathBuf::from(std::env::var("USERPROFILE").unwrap_or_default()).join(name)
}

pub fn documents() -> PathBuf {
    known_folder(&windows_sys::Win32::UI::Shell::FOLDERID_Documents).unwrap_or_else(|| profile_dir("Documents"))
}

pub fn desktop() -> PathBuf {
    known_folder(&windows_sys::Win32::UI::Shell::FOLDERID_Desktop).unwrap_or_else(|| profile_dir("Desktop"))
}

/// %LOCALAPPDATA%\mini-console: our settings and backups. The first time, it takes over the folder
/// of builds from before the rename (%LOCALAPPDATA%\goxlr-mini).
pub fn data_dir() -> anyhow::Result<PathBuf> {
    let local = PathBuf::from(std::env::var("LOCALAPPDATA")?);
    let (dir, old) = (local.join("mini-console"), local.join("goxlr-mini"));
    if !dir.exists() && old.is_dir() {
        let _ = std::fs::rename(&old, &dir);
    }
    Ok(dir)
}
