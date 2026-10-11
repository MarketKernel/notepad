//! Explorer's "Open with" menu: registers this executable for all file types,
//! for the current user only (no administrator rights needed).
//!
//! HKCU\Software\Classes\MarketKernel.Notepad\shell\open\command = "<exe>" "%1"
//! HKCU\Software\Classes\*\OpenWithProgids\MarketKernel.Notepad
//!
//! The `*\OpenWithProgids` key is shared with other apps: only our own value is
//! added to or removed from it.

use std::io;
use std::path::Path;

use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_NONE, REG_OPTION_NON_VOLATILE, REG_SZ, REG_VALUE_TYPE, RRF_RT_ANY,
    RRF_RT_REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteKeyValueW, RegDeleteTreeW, RegGetValueW, RegSetValueExW,
};
use windows_sys::Win32::UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify};

const PROG_ID: &str = "MarketKernel.Notepad";
const CLASS: &str = r"Software\Classes\MarketKernel.Notepad";
const OPEN_WITH: &str = r"Software\Classes\*\OpenWithProgids";
/// Shown in the menu; plain "Notepad" would look exactly like Windows Notepad.
const MENU_NAME: &str = "Notepad (Rust)";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

fn check(err: u32) -> io::Result<()> {
    if err == 0 { Ok(()) } else { Err(io::Error::from_raw_os_error(err as i32)) }
}

fn command(exe: &Path) -> String {
    format!("\"{}\" \"%1\"", exe.display())
}

/// Sets a value of `subkey` (created if needed, existing values are kept).
/// `None` as the name sets the key's default value.
fn set(subkey: &str, name: Option<&str>, kind: REG_VALUE_TYPE, data: &[u8]) -> io::Result<()> {
    let mut key: HKEY = std::ptr::null_mut();
    let name = name.map(wide);
    unsafe {
        check(RegCreateKeyExW(
            HKEY_CURRENT_USER,
            wide(subkey).as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            std::ptr::null(),
            &mut key,
            std::ptr::null_mut(),
        ))?;
        let result = check(RegSetValueExW(
            key,
            name.as_ref().map_or(std::ptr::null(), |n| n.as_ptr()),
            0,
            kind,
            data.as_ptr(),
            data.len() as u32,
        ));
        RegCloseKey(key);
        result
    }
}

fn set_str(subkey: &str, name: Option<&str>, value: &str) -> io::Result<()> {
    let bytes: Vec<u8> = wide(value).iter().flat_map(|c| c.to_le_bytes()).collect();
    set(subkey, name, REG_SZ, &bytes)
}

fn get_str(subkey: &str, name: Option<&str>) -> Option<String> {
    let name = name.map(wide);
    let name_ptr = name.as_ref().map_or(std::ptr::null(), |n| n.as_ptr());
    let subkey = wide(subkey);
    let mut buf = vec![0u16; 1024];
    let mut len = (buf.len() * 2) as u32;
    let err = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            name_ptr,
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut len,
        )
    };
    (err == 0).then(|| String::from_utf16_lossy(&buf[..(len as usize / 2).saturating_sub(1)]))
}

fn value_exists(subkey: &str, name: &str) -> bool {
    let (subkey, name) = (wide(subkey), wide(name));
    let err = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            name.as_ptr(),
            RRF_RT_ANY,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    err == 0
}

/// Tells Explorer to reload file associations.
fn notify_shell() {
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED as i32, SHCNF_IDLIST, std::ptr::null(), std::ptr::null()) };
}

/// Whether "Open with" opens files with this very executable (a registration
/// made from another location, e.g. before the exe was moved, doesn't count).
pub fn is_registered() -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    get_str(&format!(r"{CLASS}\shell\open\command"), None).is_some_and(|c| c.eq_ignore_ascii_case(&command(&exe)))
        && value_exists(OPEN_WITH, PROG_ID)
}

pub fn register() -> io::Result<()> {
    let exe = std::env::current_exe()?;
    set_str(CLASS, None, "Text Document")?;
    set_str(&format!(r"{CLASS}\DefaultIcon"), None, &format!("\"{}\",0", exe.display()))?;
    set_str(&format!(r"{CLASS}\shell\open"), Some("FriendlyAppName"), MENU_NAME)?;
    set_str(&format!(r"{CLASS}\shell\open\command"), None, &command(&exe))?;
    set(OPEN_WITH, Some(PROG_ID), REG_NONE, &[])?;
    notify_shell();
    Ok(())
}

pub fn unregister() -> io::Result<()> {
    const NOT_FOUND: u32 = 2;
    let ignore_missing = |e: u32| check(if e == NOT_FOUND { 0 } else { e });
    unsafe {
        ignore_missing(RegDeleteKeyValueW(HKEY_CURRENT_USER, wide(OPEN_WITH).as_ptr(), wide(PROG_ID).as_ptr()))?;
        ignore_missing(RegDeleteTreeW(HKEY_CURRENT_USER, wide(CLASS).as_ptr()))?;
    }
    notify_shell();
    Ok(())
}
