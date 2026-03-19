use windows::Win32::Foundation::{HWND, LPARAM, POINT};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, FindWindowExW, SendMessageW};
use windows::Win32::UI::Controls::{LVM_GETITEMCOUNT, LVM_GETITEMPOSITION};
use windows::core::PCWSTR;

pub fn get_desktop_listview() -> Option<HWND> {
    unsafe {
        let progman = FindWindowW(PCWSTR::from_raw(encode_wide("Progman").as_ptr()), None).ok()?;
        let mut shell_view = FindWindowExW(Some(progman), None, PCWSTR::from_raw(encode_wide("SHELLDLL_DefView").as_ptr()), None).ok();
        
        if shell_view.is_none() {
            let mut worker_w = HWND(std::ptr::null_mut());
            loop {
                worker_w = FindWindowExW(None, Some(worker_w), PCWSTR::from_raw(encode_wide("WorkerW").as_ptr()), None).ok()?;
                shell_view = FindWindowExW(Some(worker_w), None, PCWSTR::from_raw(encode_wide("SHELLDLL_DefView").as_ptr()), None).ok();
                if shell_view.is_some() { break; }
            }
        }
        
        FindWindowExW(Some(shell_view.unwrap()), None, PCWSTR::from_raw(encode_wide("SysListView32").as_ptr()), None).ok()
    }
}

pub fn encode_wide(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

