use image::{DynamicImage, ImageReader};
use std::path::PathBuf;
use windows::core::PCWSTR;
//win.rs
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, SRCCOPY};
use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows::Win32::System::Memory::{VirtualAllocEx, VirtualFreeEx, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_VM_OPERATION, PROCESS_VM_READ, PROCESS_VM_WRITE};
use windows::Win32::UI::Controls::{LVIR_BOUNDS, LVITEMW, LVM_GETITEMCOUNT, LVM_GETITEMPOSITION, LVM_GETITEMRECT};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowExW, FindWindowW, GetWindowThreadProcessId, SendMessageW, SystemParametersInfoW, SPI_GETDESKWALLPAPER, SPI_SETDESKWALLPAPER, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS};

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
    pub image: Option<(u32,u32,Vec<u8>)>,
    pub shape: CollisionShape,
}


pub fn get_wallpaper_pixels() -> Option<DynamicImage> {
    let mut buffer = [0u16; 260];
    unsafe {
        SystemParametersInfoW(
            SPI_GETDESKWALLPAPER,
            buffer.len() as u32,
            Some(buffer.as_mut_ptr() as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        ).map_err(|e| { println!("error getting wallpaper: {}", e)}).ok();
    }
    
    let len = buffer.iter().position(|&i| i == 0).unwrap_or(buffer.len());
    let path_str = String::from_utf16_lossy(&buffer[..len]);
    let path = PathBuf::from(path_str);
    
    let img = ImageReader::open(path).unwrap().with_guessed_format().ok()?.decode().ok()?;
    
    Some(img)
}

pub fn slice_taskbar(num_shards: i32) -> Vec<IconData> {
    let mut shards = Vec::new();
    unsafe {
        let tray_hwnd = FindWindowW(PCWSTR::from_raw(encode_wide("Shell_TrayWnd").as_ptr()), None)
            .unwrap_or(HWND(std::ptr::null_mut()));
        
        if tray_hwnd.0.is_null() { return shards; }
        
        let mut rect = RECT::default();
        let _ = windows::Win32::UI::WindowsAndMessaging::GetWindowRect(tray_hwnd, &mut rect);
        
        let full_width = rect.right - rect.left;
        let full_height = rect.bottom - rect.top;
        
        // 1. Capture the entire taskbar once
        let Some((_, _, full_buffer)) = capture_screen_region(rect) else { return shards; };
        
        let shard_width = full_width / num_shards;
        
        for i in 0..num_shards {
            let start_x = if i == 0 {0}else{(i*shard_width) - 1};
            // Ensure the last shard covers any remaining pixels
            let current_shard_w = if i == num_shards - 1 { full_width - start_x } else { shard_width + 2 };
            
            // 2. Extract the sub-image for this shard
            let mut shard_buffer = Vec::with_capacity((current_shard_w * full_height * 4) as usize);
            
            for y in 0..full_height {
                let row_start = ((y * full_width + start_x) * 4) as usize;
                let row_end = row_start + (current_shard_w * 4) as usize;
                shard_buffer.extend_from_slice(&full_buffer[row_start..row_end]);
            }
            
            shards.push(IconData {
                x: rect.left + start_x + (current_shard_w / 2),
                y: rect.top + (full_height / 2),
                width: current_shard_w + 2,
                height: full_height,
                rotation: 0.0,
                image: Some((current_shard_w as u32, full_height as u32, shard_buffer)),
                shape: CollisionShape::Quad,
            });
        }
    }
    shards
}

// A more generic capture function since Taskbar parts are different windows
pub fn capture_screen_region(rect: RECT) -> Option<(u32, u32, Vec<u8>)> {
    unsafe {
        let hdc_screen = windows::Win32::Graphics::Gdi::GetDC(None); // Capture from Desktop DC
        let hdc_mem = CreateCompatibleDC(Some(hdc_screen));
        
        let width = rect.right - rect.left;
        let height = rect.bottom - rect.top;
        
        let hbm_mem = CreateCompatibleBitmap(hdc_screen, width, height);
        let old_obj = SelectObject(hdc_mem, hbm_mem.into());
        
        let _ = BitBlt(hdc_mem, 0, 0, width, height, Some(hdc_screen), rect.left, rect.top, SRCCOPY);
        
        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        
        let mut buffer = vec![0u8; (width * height * 4) as usize];
        GetDIBits(hdc_screen, hbm_mem, 0, height as u32, Some(buffer.as_mut_ptr() as _), &mut bmi, DIB_RGB_COLORS);
        
        // Clean up
        SelectObject(hdc_mem, old_obj);
        let _ = DeleteObject(hbm_mem.into());
        let _ = DeleteDC(hdc_mem);
        windows::Win32::Graphics::Gdi::ReleaseDC(None, hdc_screen);
        
        for pixel in buffer.chunks_exact_mut(4) {
            pixel.swap(0, 2); // BGRA to RGBA
        }
        
        Some((width as u32, height as u32, buffer))
    }
}


/// Temporarily sets the wallpaper to a solid color BMP written to a temp file,
/// forces a desktop refresh, and captures the full listview DC.
/// Returns raw BGRA buffer of the entire listview area.
/// Renders the desktop listview directly into a memory DC with a solid background color.
/// This bypasses all open windows entirely — no screen capture involved.
use windows::Win32::Graphics::Gdi::{
    RedrawWindow, RDW_ALLCHILDREN, RDW_ERASE, RDW_INVALIDATE, RDW_UPDATENOW
    // ... rest of your imports
};


fn set_wallpaper_color_and_wait(hwnd_listview: HWND, color: [u8; 3]) -> Option<()> {
    let path = std::env::temp_dir().join(
        if color[0] > 128 { "oxynewton_white.bmp" } else { "oxynewton_black.bmp" }
    );
    write_solid_bmp(&path, color)?;
    
    unsafe {
        let wide_path = encode_wide(path.to_str()?);
        // Set the wallpaper
        SystemParametersInfoW(
            SPI_SETDESKWALLPAPER,
            0,
            Some(wide_path.as_ptr() as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(
                windows::Win32::UI::WindowsAndMessaging::SPIF_UPDATEINIFILE.0
                    | windows::Win32::UI::WindowsAndMessaging::SPIF_SENDCHANGE.0
            ),
        ).ok()?;
        
        // Force synchronous repaint of the listview and all its children
        RedrawWindow(
            Some(hwnd_listview),
            None,
            None,
            RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN | RDW_UPDATENOW,
        ).ok().expect("TODO: panic message");
    }
    Some(())
}

fn capture_listview_on_color(
    hwnd_listview: HWND,
    color: [u8; 3],
    rect: RECT,
) -> Option<Vec<u8>> {
    // 1. Change wallpaper and force synchronous repaint
    set_wallpaper_color_and_wait(hwnd_listview, color)?;
    
    // 2. Now BitBlt directly from the listview's own DC —
    //    this reads what it just painted, not the composited screen
    unsafe {
        let w = rect.right - rect.left;
        let h = rect.bottom - rect.top;
        
        // Get DC of the listview window itself (not the screen)
        let hdc_listview = windows::Win32::Graphics::Gdi::GetDC(Some(hwnd_listview));
        let hdc_mem = CreateCompatibleDC(Some(hdc_listview));
        let hbm_mem = CreateCompatibleBitmap(hdc_listview, w, h);
        let old_obj = SelectObject(hdc_mem, hbm_mem.into());
        
        // BitBlt from the listview DC — rect coords are relative to the listview window
        let _ = BitBlt(
            hdc_mem, 0, 0, w, h,
            Some(hdc_listview),
            rect.left, rect.top,  // these are already listview-relative from LVM_GETITEMRECT
            SRCCOPY,
        );
        
        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        
        let mut buffer = vec![0u8; (w * h * 4) as usize];
        GetDIBits(
            hdc_mem, hbm_mem, 0, h as u32,
            Some(buffer.as_mut_ptr() as _),
            &mut bmi,
            DIB_RGB_COLORS,
        );
        
        SelectObject(hdc_mem, old_obj);
        let _ = DeleteObject(hbm_mem.into());
        let _ = DeleteDC(hdc_mem);
        windows::Win32::Graphics::Gdi::ReleaseDC(Some(hwnd_listview), hdc_listview);
        
        for pixel in buffer.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
        
        Some(buffer)
    }
}

/// Writes a 2x2 solid color BMP (Windows will tile/stretch it as wallpaper).
fn write_solid_bmp(path: &std::path::Path, color: [u8; 3]) -> Option<()> {
    
    // BMP header for a 2x2 24-bit image
    let pixel = [color[2], color[1], color[0]]; // BMP is BGR
    let row_padded: Vec<u8> = [pixel, pixel, [0,0,0]] // 6 bytes + 2 padding = 8 bytes per row
        .concat().to_vec();
    let file_size: u32 = 54 + (row_padded.len() as u32 * 2);
    
    let mut bmp: Vec<u8> = Vec::with_capacity(file_size as usize);
    // BMP File Header
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&file_size.to_le_bytes());
    bmp.extend_from_slice(&0u32.to_le_bytes()); // reserved
    bmp.extend_from_slice(&54u32.to_le_bytes()); // pixel data offset
    // DIB Header (BITMAPINFOHEADER)
    bmp.extend_from_slice(&40u32.to_le_bytes()); // header size
    bmp.extend_from_slice(&2i32.to_le_bytes());  // width
    bmp.extend_from_slice(&(-2i32).to_le_bytes()); // height (negative = top-down)
    bmp.extend_from_slice(&1u16.to_le_bytes());  // color planes
    bmp.extend_from_slice(&24u16.to_le_bytes()); // bits per pixel
    bmp.extend_from_slice(&0u32.to_le_bytes());  // no compression
    bmp.extend_from_slice(&(row_padded.len() as u32 * 2).to_le_bytes()); // image size
    bmp.extend_from_slice(&2835i32.to_le_bytes()); // X pixels per meter
    bmp.extend_from_slice(&2835i32.to_le_bytes()); // Y pixels per meter
    bmp.extend_from_slice(&0u32.to_le_bytes());  // colors in table
    bmp.extend_from_slice(&0u32.to_le_bytes());  // important colors
    // Pixel data (2 rows)
    bmp.extend_from_slice(&row_padded);
    bmp.extend_from_slice(&row_padded);
    
    std::fs::write(path, &bmp).ok()?;
    Some(())
}

/// Restores the original wallpaper path.
fn restore_wallpaper(original_path: &str) {
    unsafe {
        let wide = encode_wide(original_path);
        let _ = SystemParametersInfoW(
            SPI_SETDESKWALLPAPER,
            0,
            Some(wide.as_ptr() as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(
                windows::Win32::UI::WindowsAndMessaging::SPIF_UPDATEINIFILE.0
                    | windows::Win32::UI::WindowsAndMessaging::SPIF_SENDCHANGE.0
            ),
        );
    }
}

/// Extract a sub-region from a full flat RGBA buffer.
fn extract_subrect(
    full_buf: &[u8],
    full_w: i32,
    rect: RECT,
) -> Vec<u8> {
    let x = rect.left;
    let y = rect.top;
    let w = rect.right - rect.left;
    let h = rect.bottom - rect.top;
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for row in 0..h {
        let src_row = y + row;
        let start = ((src_row * full_w + x) * 4) as usize;
        out.extend_from_slice(&full_buf[start..start + (w * 4) as usize]);
    }
    out
}

/// Given pixels captured over white and black backgrounds, compute true RGBA.
fn diff_to_rgba(white_buf: &[u8], black_buf: &[u8]) -> Vec<u8> {
    assert_eq!(white_buf.len(), black_buf.len());
    let mut rgba = Vec::with_capacity(white_buf.len());
    
    for i in (0..white_buf.len()).step_by(4) {
        // Captured buffers are already RGBA after our BGR->RGB swap
        let rw = white_buf[i]     as f32;
        let gw = white_buf[i + 1] as f32;
        let bw = white_buf[i + 2] as f32;
        
        let rb = black_buf[i]     as f32;
        let gb = black_buf[i + 1] as f32;
        let bb = black_buf[i + 2] as f32;
        
        // Alpha derived from any channel; average for robustness
        // alpha = 1 - (white - black), clamped
        let a_r = 1.0 - (rw - rb) / 255.0;
        let a_g = 1.0 - (gw - gb) / 255.0;
        let a_b = 1.0 - (bw - bb) / 255.0;
        let alpha = ((a_r + a_g + a_b) / 3.0).clamp(0.0, 1.0);
        let alpha_byte = (alpha * 255.0).round() as u8;
        
        // True color = black_result / alpha (black bg contributes 0)
        let (r, g, b) = if alpha > 0.01 {
            (
                (rb / alpha).clamp(0.0, 255.0) as u8,
                (gb / alpha).clamp(0.0, 255.0) as u8,
                (bb / alpha).clamp(0.0, 255.0) as u8,
            )
        } else {
            (0, 0, 0) // fully transparent, color doesn't matter
        };
        
        rgba.extend_from_slice(&[r, g, b, alpha_byte]);
    }
    rgba
}

/// Single-pass icon capture using white/black wallpaper diff for true transparency.
/// Call this instead of get_icon_data in a loop.
pub fn capture_all_icons(original_wallpaper: &str) -> Vec<IconData> {
    let mut icons = Vec::new();
    
    let hwnd_listview = match get_desktop_listview() {
        Some(h) => h,
        None => return icons,
    };
    
    // --- 1. Collect all icon rects from the listview ---
    let icon_rects: Vec<(POINT, RECT)> = unsafe {
        let mut process_id = 0;
        GetWindowThreadProcessId(hwnd_listview, Some(&mut process_id));
        
        let process_handle = match OpenProcess(
            PROCESS_VM_OPERATION | PROCESS_VM_READ | PROCESS_VM_WRITE,
            false,
            process_id,
        ).ok() {
            Some(h) => h,
            None => return icons,
        };
        
        let buf_size = size_of::<LVITEMW>().max(size_of::<RECT>()).max(size_of::<POINT>());
        let remote_mem = VirtualAllocEx(
            process_handle, None, buf_size,
            MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE,
        );
        if remote_mem.is_null() { return icons; }
        
        let count = SendMessageW(hwnd_listview, LVM_GETITEMCOUNT, None, None).0;
        
        let mut rects = Vec::new();
        for i in 0..count {
            // Get position
            let mut pos = POINT::default();
            SendMessageW(hwnd_listview, LVM_GETITEMPOSITION,
                         Some(WPARAM(i as usize)), Some(LPARAM(remote_mem as isize)));
            let _ = ReadProcessMemory(process_handle, remote_mem,
                                      &mut pos as *mut _ as _, size_of::<POINT>(), None);
            
            // Get bounding rect
            let bounds_flag = LVIR_BOUNDS as i32;
            let _ = windows::Win32::System::Diagnostics::Debug::WriteProcessMemory(
                process_handle, remote_mem,
                &bounds_flag as *const _ as _, size_of::<i32>(), None,
            );
            let mut rect = RECT::default();
            SendMessageW(hwnd_listview, LVM_GETITEMRECT,
                         Some(WPARAM(i as usize)), Some(LPARAM(remote_mem as isize)));
            let _ = ReadProcessMemory(process_handle, remote_mem,
                                      &mut rect as *mut _ as _, size_of::<RECT>(), None);
            
            rects.push((pos, rect));
        }
        
        VirtualFreeEx(process_handle, remote_mem, 0, MEM_RELEASE).ok();
        rects
    };
    
    if icon_rects.is_empty() { return icons; }
    
    // --- 2. Capture rect is already listview-local from LVM_GETITEMRECT ---
    // No need to compute a bounding box offset — rects from LVM_GETITEMRECT
    // are relative to the listview window origin already.
    let min_x = icon_rects.iter().map(|(_, r)| r.left).min().unwrap_or(0);
    let min_y = icon_rects.iter().map(|(_, r)| r.top).min().unwrap_or(0);
    let max_x = icon_rects.iter().map(|(_, r)| r.right).max().unwrap_or(0);
    let max_y = icon_rects.iter().map(|(_, r)| r.bottom).max().unwrap_or(0);
    let capture_rect = RECT { left: min_x, top: min_y, right: max_x, bottom: max_y };
    let full_w = max_x - min_x;
    
    // --- 3. Capture on white then black using wallpaper swap + listview DC ---
    let white_full = match capture_listview_on_color(hwnd_listview, [255, 255, 255], capture_rect) {
        Some(b) => b,
        None => { restore_wallpaper(original_wallpaper); return icons; }
    };
    let black_full = match capture_listview_on_color(hwnd_listview, [0, 0, 0], capture_rect) {
        Some(b) => b,
        None => { restore_wallpaper(original_wallpaper); return icons; }
    };
    
    // --- 4. Restore wallpaper ---
    restore_wallpaper(original_wallpaper);
    // Force one final redraw so the real wallpaper comes back before we return
    unsafe {
        RedrawWindow(
            Some(hwnd_listview), None, None,
            RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN | RDW_UPDATENOW,
        ).ok().expect("TODO: panic message");
    }
    // --- 5. Diff each icon rect and build IconData ---
    for (pos, rect) in &icon_rects {
        // Translate rect relative to capture_rect origin
        let local_rect = RECT {
            left:  rect.left  - min_x,
            top:   rect.top   - min_y,
            right: rect.right - min_x,
            bottom: rect.bottom - min_y,
        };
        
        let white_crop = extract_subrect(&white_full, full_w, local_rect);
        let black_crop = extract_subrect(&black_full, full_w, local_rect);
        let rgba = diff_to_rgba(&white_crop, &black_crop);
        
        let w = rect.right - rect.left;
        let h = rect.bottom - rect.top;
        
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
    
    icons
}