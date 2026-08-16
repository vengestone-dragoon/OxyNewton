//win.rs
use image::{DynamicImage, ImageReader};
use std::path::PathBuf;
use std::thread::sleep;
use std::time::Duration;
use pixels::wgpu::naga::SwizzleComponent::W;
use windows::core::{BOOL, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::{BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateSolidBrush, DeleteDC, DeleteObject, GetDC, GetDIBits, InvalidateRect, ReleaseDC, SelectObject, UpdateWindow, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CAPTUREBLT, DIB_RGB_COLORS, ROP_CODE, SRCCOPY};
use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{
    VirtualAllocEx, VirtualFreeEx, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
};
use windows::Win32::System::Registry::{
    RegOpenKeyExW, RegQueryValueExW, HKEY_CURRENT_USER, KEY_READ, REG_SZ, REG_VALUE_TYPE,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_VM_OPERATION, PROCESS_VM_READ, PROCESS_VM_WRITE,
};
use windows::Win32::UI::Controls::{
    LVIR_BOUNDS, LVITEMW, LVM_GETITEMCOUNT, LVM_GETITEMPOSITION, LVM_GETITEMRECT,
};
use windows::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, EnumWindows, FindWindowExW, FindWindowW, GetParent, GetSystemMetrics, GetWindowRect, GetWindowThreadProcessId, IsIconic, IsWindowVisible, PeekMessageW, RegisterClassExW, SendMessageW, SetParent, ShowWindow, SystemParametersInfoW, TranslateMessage, UnregisterClassW, MSG, PM_REMOVE, SM_CXSCREEN, SM_CYSCREEN, SMTO_NORMAL, SW_MINIMIZE, SW_SHOWNOACTIVATE, SPI_GETDESKWALLPAPER, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WNDCLASSEXW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP, WS_VISIBLE, WS_CHILD, SetWindowPos, HWND_BOTTOM, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE};
use windows::Win32::UI::WindowsAndMessaging::SendMessageTimeoutW;

// ---------------------------------------------------------------------------
// String helpers
// ---------------------------------------------------------------------------

pub fn encode_wide(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub enum CollisionShape {
    Circle,
    Quad,
}

pub struct IconData {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub rotation: f32,
    pub image: Option<(u32, u32, Vec<u8>)>,
    pub shape: CollisionShape,
}

// ---------------------------------------------------------------------------
// Desktop listview / wallpaper host discovery
// ---------------------------------------------------------------------------

pub fn get_desktop_listview() -> Option<HWND> {
    unsafe {
        let progman = FindWindowW(
            PCWSTR::from_raw(encode_wide("Progman").as_ptr()),
            None,
        )
            .ok()?;
        
        let mut shell_view = FindWindowExW(
            Some(progman),
            None,
            PCWSTR::from_raw(encode_wide("SHELLDLL_DefView").as_ptr()),
            None,
        )
            .ok();
        
        if shell_view.is_none() {
            let mut worker_w = HWND(std::ptr::null_mut());
            loop {
                worker_w = FindWindowExW(
                    None,
                    Some(worker_w),
                    PCWSTR::from_raw(encode_wide("WorkerW").as_ptr()),
                    None,
                )
                    .ok()?;
                shell_view = FindWindowExW(
                    Some(worker_w),
                    None,
                    PCWSTR::from_raw(encode_wide("SHELLDLL_DefView").as_ptr()),
                    None,
                )
                    .ok();
                if shell_view.is_some() {
                    break;
                }
            }
        }
        
        FindWindowExW(
            Some(shell_view.unwrap()),
            None,
            PCWSTR::from_raw(encode_wide("SysListView32").as_ptr()),
            None,
        )
            .ok()
    }
}

// ---------------------------------------------------------------------------
// Window minimise / restore
// ---------------------------------------------------------------------------

fn collect_and_minimize_windows(hwnd_listview: HWND) -> Vec<HWND> {
    let mut shell_hwnds: Vec<HWND> = Vec::new();
    unsafe {
        if let Ok(tray) = FindWindowW(
            PCWSTR::from_raw(encode_wide("Shell_TrayWnd").as_ptr()),
            None,
        ) {
            shell_hwnds.push(tray);
        }
        shell_hwnds.push(hwnd_listview);
        if let Ok(sv) = GetParent(hwnd_listview) {
            shell_hwnds.push(sv);
            if let Ok(host) = GetParent(sv) {
                shell_hwnds.push(host);
            }
        }
    }
    
    struct CallbackData {
        shell_hwnds: Vec<HWND>,
        to_minimize: Vec<HWND>,
    }
    
    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let data = &mut *(lparam.0 as *mut CallbackData);
        if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            return BOOL(1);
        }
        if data.shell_hwnds.contains(&hwnd) {
            return BOOL(1);
        }
        data.to_minimize.push(hwnd);
        BOOL(1)
    }
    
    let mut data = CallbackData {
        shell_hwnds,
        to_minimize: Vec::new(),
    };
    
    unsafe {
        let _ = EnumWindows(
            Some(enum_proc),
            LPARAM(&mut data as *mut _ as isize),
        );
        for &hwnd in &data.to_minimize {
            let _ = ShowWindow(hwnd, SW_MINIMIZE);
        }
    }
    
    data.to_minimize
}

fn restore_windows(windows_to_restore: &[HWND]) {
    unsafe {
        for &hwnd in windows_to_restore {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
    }
}

// ---------------------------------------------------------------------------
// Wallpaper helpers (kept for external callers)
// ---------------------------------------------------------------------------

pub fn get_wallpaper_path_from_registry() -> Option<String> {
    unsafe {
        let subkey = encode_wide(r"Control Panel\Desktop");
        let value_name = encode_wide("WallPaper");
        
        let mut hkey = windows::Win32::System::Registry::HKEY::default();
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR::from_raw(subkey.as_ptr()),
            Some(0),
            KEY_READ,
            &mut hkey,
        )
            .ok();
        
        let mut data_type = REG_VALUE_TYPE(0);
        let mut byte_len = 0u32;
        let _ = RegQueryValueExW(
            hkey,
            PCWSTR::from_raw(value_name.as_ptr()),
            None,
            Some(&mut data_type),
            None,
            Some(&mut byte_len),
        );
        
        if data_type != REG_SZ || byte_len == 0 {
            return None;
        }
        
        let mut buf = vec![0u16; byte_len as usize / 2 + 1];
        RegQueryValueExW(
            hkey,
            PCWSTR::from_raw(value_name.as_ptr()),
            None,
            Some(&mut data_type),
            Some(buf.as_mut_ptr() as *mut u8),
            Some(&mut byte_len),
        )
            .ok();
        
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..len]))
    }
}

pub fn get_wallpaper_pixels() -> Option<DynamicImage> {
    let mut buffer = [0u16; 260];
    unsafe {
        SystemParametersInfoW(
            SPI_GETDESKWALLPAPER,
            buffer.len() as u32,
            Some(buffer.as_mut_ptr() as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
            .map_err(|e| println!("error getting wallpaper: {}", e))
            .ok();
    }
    let len = buffer.iter().position(|&i| i == 0).unwrap_or(buffer.len());
    let path_str = String::from_utf16_lossy(&buffer[..len]);
    let path = PathBuf::from(path_str);
    ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()
}

// ---------------------------------------------------------------------------
// Backdrop window injection
// ---------------------------------------------------------------------------

/// Finds the WorkerW that sits directly above the wallpaper and below the
/// desktop icons. This is the injection point for our backdrop window.
/// Sends the 0x052C message to Progman first to ensure the WorkerW exists.
unsafe fn find_desktop_injection_parent() -> Option<HWND> {
    let progman = FindWindowW(
        PCWSTR::from_raw(encode_wide("Progman").as_ptr()),
        None,
    ).ok()?;
    
    // Trigger WorkerW creation if needed
    SendMessageTimeoutW(progman, 0x052C, WPARAM(0), LPARAM(0), SMTO_NORMAL, 1000, None);
    
    // Find the WorkerW that directly CONTAINS SHELLDLL_DefView.
    // We will inject into this one, behind DefView using SetWindowPos.
    struct SearchData { target: HWND }
    
    unsafe extern "system" fn enum_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let data = &mut *(lparam.0 as *mut SearchData);
        let shell_view = FindWindowExW(
            Some(hwnd),
            None,
            PCWSTR::from_raw(encode_wide("SHELLDLL_DefView").as_ptr()),
            None,
        );
        if shell_view.is_ok() {
            data.target = hwnd; // this is the WorkerW (or Progman) containing the icons
        }
        BOOL(1)
    }
    
    let mut data = SearchData { target: HWND(std::ptr::null_mut()) };
    let _ = EnumWindows(Some(enum_cb), LPARAM(&mut data as *mut _ as isize));
    
    if !data.target.0.is_null() {
        Some(data.target)
    } else {
        Some(progman)
    }
}

/// Pump pending messages so WM_PAINT fires, then block until DWM has
/// composited the current frame. After this returns, a BitBlt with
/// CAPTUREBLT is guaranteed to include our backdrop window.
unsafe fn flush_dwm() {
    let mut msg = MSG::default();
    while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
    let _ = DwmFlush();
}

/// Creates a full-screen, borderless child window filled with `color` (0x00RRGGBB)
/// parented into the desktop injection layer (above wallpaper, below icons).
/// Returns the HWND; caller must call `destroy_backdrop_window` when done.
unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    DefWindowProcW(hwnd, msg, wparam, lparam)
}
unsafe fn create_backdrop_window(color: u32) -> Option<HWND> {
    let hinstance = GetModuleHandleW(None).ok()?;
    let class_name = encode_wide("_OxyNewtonBackdrop");
    
    let r = (color >> 16) & 0xFF;
    let g = (color >> 8) & 0xFF;
    let b = color & 0xFF;
    let colorref = b << 16 | g << 8 | r;
    let brush = CreateSolidBrush(windows::Win32::Foundation::COLORREF(colorref));
    
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wnd_proc),
        hInstance: hinstance.into(),
        lpszClassName: PCWSTR(class_name.as_ptr()),
        hbrBackground: brush,
        ..Default::default()
    };
    let _ = RegisterClassExW(&wc);
    
    let injection_parent = find_desktop_injection_parent()?;
    
    // Size to match the injection parent exactly
    let mut parent_rect = RECT::default();
    GetWindowRect(injection_parent, &mut parent_rect).ok()?;
    let w = parent_rect.right - parent_rect.left;
    let h = parent_rect.bottom - parent_rect.top;
    
    let hwnd = CreateWindowExW(
        WS_EX_NOACTIVATE,
        PCWSTR(class_name.as_ptr()),
        None,
        WS_CHILD | WS_VISIBLE,
        0, 0, w, h,
        Some(injection_parent),
        None,
        Some(hinstance.into()),
        None,
    ).ok().unwrap();
    
    // Push our window to the BOTTOM of the Z-order within this parent.
    // SHELLDLL_DefView (icons) will remain above us, wallpaper below.
    SetWindowPos(
        hwnd,
        Some(HWND_BOTTOM),
        0, 0, 0, 0,
        SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
    ).ok()?;
    
    InvalidateRect(Some(hwnd), None, true);
    UpdateWindow(hwnd);
    flush_dwm();
    
    Some(hwnd)
}

unsafe fn destroy_backdrop_window(hwnd: HWND) {
    let _ = DestroyWindow(hwnd);
    flush_dwm();
}

// ---------------------------------------------------------------------------
// Screen capture
// ---------------------------------------------------------------------------

/// Capture an arbitrary rect from the composed screen.
/// CAPTUREBLT ensures DWM-composited content (shadows, transparency) is included.
fn capture_screen_rect(rect: RECT) -> Option<Vec<u8>> {
    unsafe {
        let w = rect.right - rect.left;
        let h = rect.bottom - rect.top;
        if w <= 0 || h <= 0 {
            return None;
        }
        
        let hdc_screen = GetDC(None);
        let hdc_mem = CreateCompatibleDC(Some(hdc_screen));
        let hbm = CreateCompatibleBitmap(hdc_screen, w, h);
        let old = SelectObject(hdc_mem, hbm.into());
        
        // ROP_CODE combines SRCCOPY with CAPTUREBLT to capture layered windows.
        let _ = BitBlt(
            hdc_mem,
            0,
            0,
            w,
            h,
            Some(hdc_screen),
            rect.left,
            rect.top,
            ROP_CODE(SRCCOPY.0 | CAPTUREBLT.0),
        );
        
        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // negative = top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        
        let mut buf = vec![0u8; (w * h * 4) as usize];
        GetDIBits(
            hdc_mem,
            hbm,
            0,
            h as u32,
            Some(buf.as_mut_ptr() as _),
            &mut bmi,
            DIB_RGB_COLORS,
        );
        
        SelectObject(hdc_mem, old);
        let _ = DeleteObject(hbm.into());
        let _ = DeleteDC(hdc_mem);
        ReleaseDC(None, hdc_screen);
        
        // BGRA → RGBA
        for px in buf.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        
        Some(buf)
    }
}

// ---------------------------------------------------------------------------
// Pixel math
// ---------------------------------------------------------------------------

fn extract_subrect(full_buf: &[u8], full_w: i32, rect: RECT) -> Vec<u8> {
    let w = rect.right - rect.left;
    let h = rect.bottom - rect.top;
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for row in 0..h {
        let start = (((rect.top + row) * full_w + rect.left) * 4) as usize;
        out.extend_from_slice(&full_buf[start..start + (w * 4) as usize]);
    }
    out
}

fn diff_to_rgba(white_buf: &[u8], black_buf: &[u8]) -> Vec<u8> {
    assert_eq!(white_buf.len(), black_buf.len());
    let mut rgba = Vec::with_capacity(white_buf.len());
    for i in (0..white_buf.len()).step_by(4) {
        let rw = white_buf[i] as f32;
        let gw = white_buf[i + 1] as f32;
        let bw = white_buf[i + 2] as f32;
        let rb = black_buf[i] as f32;
        let gb = black_buf[i + 1] as f32;
        let bb = black_buf[i + 2] as f32;
        
        let alpha = (((1.0 - (rw - rb) / 255.0)
            + (1.0 - (gw - gb) / 255.0)
            + (1.0 - (bw - bb) / 255.0))
            / 3.0)
            .clamp(0.0, 1.0);
        let alpha_byte = (alpha * 255.0).round() as u8;
        
        let (r, g, b) = if alpha > 0.01 {
            (
                (rb / alpha).clamp(0.0, 255.0) as u8,
                (gb / alpha).clamp(0.0, 255.0) as u8,
                (bb / alpha).clamp(0.0, 255.0) as u8,
            )
        } else {
            (0, 0, 0)
        };
        
        rgba.extend_from_slice(&[r, g, b, alpha_byte]);
    }
    rgba
}

// ---------------------------------------------------------------------------
// Main capture entry point
// ---------------------------------------------------------------------------

/// Capture all desktop icons and taskbar shards with true transparency.
///
/// Strategy:
///   1. Minimise all non-shell windows.
///   2. Inject a full-screen white backdrop window behind the desktop icons
///      (above the wallpaper, via SetParent into the WorkerW layer).
///   3. Wait for DWM to composite, then capture.
///   4. Destroy white backdrop, inject black, capture again.
///   5. Diff the two captures to extract true RGBA with compositor effects.
///   6. Restore minimised windows. Wallpaper is never touched.
pub fn capture_all_icons(
    _original_wallpaper: &str, // kept for API compatibility, no longer used
    _screen_w: u32,
    _screen_h: u32,
    num_taskbar_shards: i32,
) -> Vec<IconData> {
    let mut icons = Vec::new();
    
    let hwnd_listview = match get_desktop_listview() {
        Some(h) => h,
        None => return icons,
    };
    
    // -----------------------------------------------------------------------
    // 1. Enumerate icon rects (unchanged from original).
    // -----------------------------------------------------------------------
    let listview_screen_rect: RECT = unsafe {
        let mut r = RECT::default();
        let _ = GetWindowRect(hwnd_listview, &mut r);
        r
    };
    
    let icon_rects: Vec<(POINT, RECT)> = unsafe {
        let mut process_id = 0;
        GetWindowThreadProcessId(hwnd_listview, Some(&mut process_id));
        
        let process_handle = match OpenProcess(
            PROCESS_VM_OPERATION | PROCESS_VM_READ | PROCESS_VM_WRITE,
            false,
            process_id,
        )
            .ok()
        {
            Some(h) => h,
            None => return icons,
        };
        
        let buf_size = std::mem::size_of::<LVITEMW>()
            .max(std::mem::size_of::<RECT>())
            .max(std::mem::size_of::<POINT>());
        let remote_mem = VirtualAllocEx(
            process_handle,
            None,
            buf_size,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        );
        if remote_mem.is_null() {
            return icons;
        }
        
        let count = SendMessageW(hwnd_listview, LVM_GETITEMCOUNT, None, None).0;
        let mut rects = Vec::new();
        
        for i in 0..count {
            let mut pos = POINT::default();
            SendMessageW(
                hwnd_listview,
                LVM_GETITEMPOSITION,
                Some(WPARAM(i as usize)),
                Some(LPARAM(remote_mem as isize)),
            );
            let _ = ReadProcessMemory(
                process_handle,
                remote_mem,
                &mut pos as *mut _ as _,
                std::mem::size_of::<POINT>(),
                None,
            );
            
            let bounds_flag = LVIR_BOUNDS as i32;
            let _ = windows::Win32::System::Diagnostics::Debug::WriteProcessMemory(
                process_handle,
                remote_mem,
                &bounds_flag as *const _ as _,
                std::mem::size_of::<i32>(),
                None,
            );
            let mut rect = RECT::default();
            SendMessageW(
                hwnd_listview,
                LVM_GETITEMRECT,
                Some(WPARAM(i as usize)),
                Some(LPARAM(remote_mem as isize)),
            );
            let _ = ReadProcessMemory(
                process_handle,
                remote_mem,
                &mut rect as *mut _ as _,
                std::mem::size_of::<RECT>(),
                None,
            );
            
            let screen_rect = RECT {
                left: listview_screen_rect.left + rect.left,
                top: listview_screen_rect.top + rect.top,
                right: listview_screen_rect.left + rect.right,
                bottom: listview_screen_rect.top + rect.bottom,
            };
            
            rects.push((pos, screen_rect));
        }
        
        VirtualFreeEx(process_handle, remote_mem, 0, MEM_RELEASE).ok();
        rects
    };
    
    // -----------------------------------------------------------------------
    // 2. Taskbar rect.
    // -----------------------------------------------------------------------
    let taskbar_screen_rect: Option<RECT> = unsafe {
        FindWindowW(
            PCWSTR::from_raw(encode_wide("Shell_TrayWnd").as_ptr()),
            None,
        )
            .ok()
            .and_then(|tray| {
                let mut r = RECT::default();
                GetWindowRect(tray, &mut r).ok()?;
                Some(r)
            })
    };
    
    if icon_rects.is_empty() && taskbar_screen_rect.is_none() {
        return icons;
    }
    
    let lv_capture_rect: Option<RECT> = if !icon_rects.is_empty() {
        Some(RECT {
            left: icon_rects.iter().map(|(_, r)| r.left).min().unwrap(),
            top: icon_rects.iter().map(|(_, r)| r.top).min().unwrap(),
            right: icon_rects.iter().map(|(_, r)| r.right).max().unwrap(),
            bottom: icon_rects.iter().map(|(_, r)| r.bottom).max().unwrap(),
        })
    } else {
        None
    };
    
    // -----------------------------------------------------------------------
    // 3. Minimise non-shell windows.
    // -----------------------------------------------------------------------
    let minimized_windows = collect_and_minimize_windows(hwnd_listview);
    // Brief pause for minimize animations to complete.
    sleep(Duration::from_millis(300));
    
    // -----------------------------------------------------------------------
    // 4. White backdrop pass.
    // -----------------------------------------------------------------------
    let mut white_lv: Vec<u8> = Vec::new();
    let mut white_taskbar: Vec<u8> = Vec::new();
    
    unsafe {
        if let Some(hwnd) = create_backdrop_window(0x00FFFFFF) {
            if let Some(rect) = lv_capture_rect {
                white_lv = capture_screen_rect(rect).unwrap_or_default();
            }
            if let Some(rect) = taskbar_screen_rect {
                white_taskbar = capture_screen_rect(rect).unwrap_or_default();
            }
            destroy_backdrop_window(hwnd);
        }
    }
    
    // -----------------------------------------------------------------------
    // 5. Black backdrop pass.
    // -----------------------------------------------------------------------
    let mut black_lv: Vec<u8> = Vec::new();
    let mut black_taskbar: Vec<u8> = Vec::new();
    
    unsafe {
        if let Some(hwnd) = create_backdrop_window(0x00000000) {
            if let Some(rect) = lv_capture_rect {
                black_lv = capture_screen_rect(rect).unwrap_or_default();
            }
            if let Some(rect) = taskbar_screen_rect {
                black_taskbar = capture_screen_rect(rect).unwrap_or_default();
            }
            destroy_backdrop_window(hwnd);
        }
    }
    
    // -----------------------------------------------------------------------
    // 6. Restore minimised windows. Wallpaper was never touched.
    // -----------------------------------------------------------------------
    restore_windows(&minimized_windows);
    
    // -----------------------------------------------------------------------
    // 7. Diff listview icons (unchanged from original).
    // -----------------------------------------------------------------------
    if let Some(capture_rect) = lv_capture_rect {
        let capture_w = capture_rect.right - capture_rect.left;
        
        for (pos, screen_rect) in &icon_rects {
            let local = RECT {
                left: screen_rect.left - capture_rect.left,
                top: screen_rect.top - capture_rect.top,
                right: screen_rect.right - capture_rect.left,
                bottom: screen_rect.bottom - capture_rect.top,
            };
            
            if local.left < 0
                || local.top < 0
                || local.right > capture_w
                || local.bottom > (capture_rect.bottom - capture_rect.top)
            {
                continue;
            }
            
            if white_lv.is_empty() || black_lv.is_empty() {
                break;
            }
            
            let white_crop = extract_subrect(&white_lv, capture_w, local);
            let black_crop = extract_subrect(&black_lv, capture_w, local);
            let rgba = diff_to_rgba(&white_crop, &black_crop);
            
            let w = screen_rect.right - screen_rect.left;
            let h = screen_rect.bottom - screen_rect.top;
            
            icons.push(IconData {
                x: pos.x + w / 2,
                y: pos.y + h / 2,
                width: w,
                height: h,
                rotation: 0.0,
                image: Some((w as u32, h as u32, rgba)),
                shape: CollisionShape::Circle,
            });
        }
    }
    
    // -----------------------------------------------------------------------
    // 8. Diff and shard the taskbar (unchanged from original).
    // -----------------------------------------------------------------------
    if let Some(tb_rect) = taskbar_screen_rect {
        if !white_taskbar.is_empty() && !black_taskbar.is_empty() {
            let tb_w = (tb_rect.right - tb_rect.left) as u32;
            let tb_h = (tb_rect.bottom - tb_rect.top) as u32;
            
            let full_rgba = diff_to_rgba(&white_taskbar, &black_taskbar);
            
            let shard_w = tb_w as i32 / num_taskbar_shards;
            
            for i in 0..num_taskbar_shards {
                let start_x = if i == 0 { 0 } else { i * shard_w - 1 };
                let current_w = if i == num_taskbar_shards - 1 {
                    tb_w as i32 - start_x
                } else {
                    shard_w + 2
                };
                
                let shard_rect = RECT {
                    left: start_x,
                    top: 0,
                    right: start_x + current_w,
                    bottom: tb_h as i32,
                };
                let shard_rgba = extract_subrect(&full_rgba, tb_w as i32, shard_rect);
                
                icons.push(IconData {
                    x: tb_rect.left + start_x + current_w / 2,
                    y: tb_rect.top + tb_h as i32 / 2,
                    width: current_w,
                    height: tb_h as i32,
                    rotation: 0.0,
                    image: Some((current_w as u32, tb_h, shard_rgba)),
                    shape: CollisionShape::Quad,
                });
            }
        }
    }
    
    icons
}