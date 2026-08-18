//! Windows platform backend, selected by `os.rs` when `target_os = "windows"`.
//!
//! Everything OS-specific lives here: window/element discovery, cross-process memory
//! reads, UI Automation, screen capture, and solid-color backdrop injection. The only
//! things this module exposes (via `os.rs`) are opaque handles (`Monitor`) and plain
//! data (`RawCapture`, built from `Bounds`/`image` types) — callers outside `os/` stay
//! free of any `windows` crate type, so a future `os/mac.rs`/`os/linux.rs` exposing the
//! same `Monitor`/`RawCapture`/`enable_dpi_awareness`/`enumerate_monitors`/
//! `capture_desktop` shape can stand in on another target without touching them.

use std::mem::size_of;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread::sleep;
use std::time::Duration;

use image::{DynamicImage, ImageReader, RgbaImage};
use windows::core::{BOOL, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, DeleteObject, EnumDisplayMonitors, GetMonitorInfoW, InvalidateRect, RedrawWindow,
    UpdateWindow, HDC, HMONITOR, MONITORINFO, RDW_ALLCHILDREN, RDW_INVALIDATE, RDW_UPDATENOW,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Diagnostics::Debug::{ReadProcessMemory, WriteProcessMemory};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{
    VirtualAllocEx, VirtualFreeEx, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_VM_OPERATION, PROCESS_VM_READ, PROCESS_VM_WRITE,
};
use windows::Win32::UI::Accessibility::{CUIAutomation, TreeScope_Descendants};
use windows::Win32::UI::Controls::{
    LVIF_TEXT, LVIR_BOUNDS, LVIR_ICON, LVIR_LABEL, LVITEMW, LVM_GETITEMCOUNT, LVM_GETITEMRECT,
    LVM_GETITEMTEXTW,
};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, EnumWindows, FindWindowExW,
    FindWindowW, GetClassNameW, GetSystemMetrics, GetWindowRect, GetWindowThreadProcessId,
    PeekMessageW, RegisterClassExW, SendMessageW, SetWindowPos, SystemParametersInfoW,
    TranslateMessage, UnregisterClassW,
    FE_FONTSMOOTHINGCLEARTYPE, FE_FONTSMOOTHINGSTANDARD, MSG, PM_REMOVE,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SPIF_SENDCHANGE,
    SPI_GETDESKWALLPAPER, SPI_GETFONTSMOOTHING, SPI_GETFONTSMOOTHINGTYPE, SPI_SETFONTSMOOTHINGTYPE,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WNDCLASSEXW, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
};

use crate::assets::Bounds;

mod capture;
mod shell_icons;

fn encode_wide(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn bounds_from_rect(r: RECT) -> Bounds {
    Bounds {
        x: r.left,
        y: r.top,
        width: r.right - r.left,
        height: r.bottom - r.top,
    }
}

/// An opaque handle to a monitor, obtained from [`enumerate_monitors`]. Callers can't
/// inspect it — it exists purely to be passed back into [`capture_desktop`].
#[derive(Clone, Copy)]
pub struct Monitor(HMONITOR);

/// Everything captured for one monitor, in plain OS-agnostic types (`Bounds` and
/// `image` crate types only). `assets.rs` turns this into the engine-ready
/// `DesktopEntry`: `icon_images` is already real, alpha-correct RGBA (rendered
/// directly from the Shell, not screen-captured — see `shell_icons`); the taskbar
/// pair still needs diffing (white/black composited over the live desktop) and
/// cropping per element, since there's no non-screen-capture way to get taskbar
/// element bitmaps.
pub struct RawCapture {
    pub monitor_bounds: Bounds,
    pub virtual_screen_bounds: Bounds,
    pub background: DynamicImage,
    pub icon_bounds: Vec<Bounds>,
    pub icon_images: Vec<RgbaImage>,
    pub taskbar_bounds: Vec<Bounds>,
    pub taskbar_capture_bounds: Option<Bounds>,
    pub taskbar_white: Option<RgbaImage>,
    pub taskbar_black: Option<RgbaImage>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum TaskbarElementKind {
    Start,
    Search,
    ShowDesktop,
    SystemTray,
    /// Any other individually-addressable element (a running/pinned app button, a
    /// panel button like task view or widgets, etc.) — nothing downstream needs a
    /// finer-grained label than "not one of the special cases above".
    Item,
    /// Real, unclaimed taskbar space: either a gap between elements, or the leftover
    /// part of a container after its genuine (smaller, nested) children were carved
    /// out of it — see `trim_nested_containers`.
    Empty,
}

#[derive(Debug, Clone, Copy)]
struct TaskbarElement {
    kind: TaskbarElementKind,
    bounds: Bounds,
}

// ---------------------------------------------------------------------------
// Startup
// ---------------------------------------------------------------------------

/// Every step that must run once before any other function in this module. Add new
/// Windows-specific startup requirements here rather than as one-off calls from
/// `os.rs`/`main.rs` — `prelude()` (called once via `os::prelude()`) just runs the list.
const SETUP_STEPS: &[fn()] = &[enable_dpi_awareness];

pub fn prelude() {
    for step in SETUP_STEPS {
        step();
    }
}

/// Opts the process into per-monitor DPI awareness — without it, `GetWindowRect`/
/// monitor rects come back in DPI-scaled logical coordinates that won't line up with
/// the physical pixels `BitBlt` captures on any display above 100% scaling.
fn enable_dpi_awareness() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

// ---------------------------------------------------------------------------
// Monitors
// ---------------------------------------------------------------------------

unsafe extern "system" fn enum_monitors_proc(
    hmonitor: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    let monitors = unsafe { &mut *(lparam.0 as *mut Vec<HMONITOR>) };
    monitors.push(hmonitor);
    BOOL(1)
}

pub fn enumerate_monitors() -> Vec<Monitor> {
    let mut monitors: Vec<HMONITOR> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(enum_monitors_proc),
            LPARAM(&mut monitors as *mut _ as isize),
        );
    }
    monitors.into_iter().map(Monitor).collect()
}

pub fn monitor_bounds(monitor: &Monitor) -> Option<Bounds> {
    unsafe {
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(monitor.0, &mut info).as_bool() {
            Some(bounds_from_rect(info.rcMonitor))
        } else {
            None
        }
    }
}

/// Bounding rect of the whole virtual desktop (the union of every monitor), which may
/// have a negative origin when a monitor is positioned left of / above the primary.
fn virtual_screen_bounds() -> Bounds {
    unsafe {
        Bounds {
            x: GetSystemMetrics(SM_XVIRTUALSCREEN),
            y: GetSystemMetrics(SM_YVIRTUALSCREEN),
            width: GetSystemMetrics(SM_CXVIRTUALSCREEN),
            height: GetSystemMetrics(SM_CYVIRTUALSCREEN),
        }
    }
}

fn window_bounds(hwnd: HWND) -> Option<Bounds> {
    unsafe {
        let mut rect = RECT::default();
        GetWindowRect(hwnd, &mut rect).ok()?;
        Some(bounds_from_rect(rect))
    }
}

// ---------------------------------------------------------------------------
// Desktop listview / wallpaper host discovery
// ---------------------------------------------------------------------------

fn get_desktop_listview() -> Option<HWND> {
    unsafe {
        let progman = FindWindowW(PCWSTR::from_raw(encode_wide("Progman").as_ptr()), None).ok()?;

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
// Desktop icon details (cross-process)
// ---------------------------------------------------------------------------

/// One desktop icon's on-screen layout: the overall bounds (icon + label, one physics
/// object), the icon graphic's own sub-rect within that, the label text's sub-rect, and
/// the item's display name (used to look up its real thumbnail/icon — see
/// `shell_icons`).
pub(super) struct IconDetail {
    pub bounds: Bounds,
    pub icon_rect: Bounds,
    pub label_rect: Bounds,
    pub name: String,
}

/// Reads the on-screen layout of every item in the desktop icon listview. `listview`
/// lives in `explorer.exe`, so this round-trips through a small chunk of memory
/// allocated in that process (`VirtualAllocEx`/`WriteProcessMemory`/`ReadProcessMemory`)
/// for both the `LVM_GETITEMRECT` rects and the `LVM_GETITEMTEXTW` label text.
fn enumerate_desktop_icon_details(listview: HWND) -> Vec<IconDetail> {
    unsafe {
        let mut listview_rect = RECT::default();
        if GetWindowRect(listview, &mut listview_rect).is_err() {
            return Vec::new();
        }

        let mut process_id = 0u32;
        GetWindowThreadProcessId(listview, Some(&mut process_id));

        let Ok(process_handle) = OpenProcess(
            PROCESS_VM_OPERATION | PROCESS_VM_READ | PROCESS_VM_WRITE,
            false,
            process_id,
        ) else {
            return Vec::new();
        };

        // One remote buffer reused for every query: the first size_of::<RECT>() bytes
        // double as the LVM_GETITEMRECT request/response, and as the LVITEMW struct
        // for LVM_GETITEMTEXTW (RECT is smaller than LVITEMW, so this is safe); the
        // text itself lands right after the LVITEMW struct, in the same allocation.
        const TEXT_BUF_CHARS: usize = 260;
        let text_offset = size_of::<LVITEMW>();
        let buf_size = text_offset + TEXT_BUF_CHARS * 2;
        let remote_mem = VirtualAllocEx(process_handle, None, buf_size, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
        if remote_mem.is_null() {
            let _ = CloseHandle(process_handle);
            return Vec::new();
        }
        let remote_text = (remote_mem as usize + text_offset) as *mut core::ffi::c_void;

        let count = SendMessageW(listview, LVM_GETITEMCOUNT, None, None).0;
        let mut details = Vec::with_capacity(count.max(0) as usize);

        for i in 0..count {
            let read_rect = |which: u32| -> Option<Bounds> {
                // LVM_GETITEMRECT expects the LVIR_* code pre-written into rect.left,
                // and overwrites the whole struct with the actual rect on return.
                let request_code = which as i32;
                WriteProcessMemory(process_handle, remote_mem, &request_code as *const _ as *const _, size_of::<i32>(), None)
                    .ok()?;
                SendMessageW(listview, LVM_GETITEMRECT, Some(WPARAM(i as usize)), Some(LPARAM(remote_mem as isize)));
                let mut rect = RECT::default();
                ReadProcessMemory(process_handle, remote_mem, &mut rect as *mut _ as *mut _, size_of::<RECT>(), None).ok()?;
                Some(Bounds {
                    x: listview_rect.left + rect.left,
                    y: listview_rect.top + rect.top,
                    width: rect.right - rect.left,
                    height: rect.bottom - rect.top,
                })
            };

            let (Some(bounds), Some(icon_rect), Some(label_rect)) =
                (read_rect(LVIR_BOUNDS), read_rect(LVIR_ICON), read_rect(LVIR_LABEL))
            else {
                continue;
            };

            let item = LVITEMW {
                mask: LVIF_TEXT,
                iSubItem: 0,
                pszText: PWSTR(remote_text as *mut u16),
                cchTextMax: TEXT_BUF_CHARS as i32,
                ..Default::default()
            };
            let mut name = String::new();
            if WriteProcessMemory(process_handle, remote_mem, &item as *const _ as *const _, size_of::<LVITEMW>(), None).is_ok()
            {
                SendMessageW(listview, LVM_GETITEMTEXTW, Some(WPARAM(i as usize)), Some(LPARAM(remote_mem as isize)));
                let mut text_buf = vec![0u16; TEXT_BUF_CHARS];
                if ReadProcessMemory(process_handle, remote_text, text_buf.as_mut_ptr() as *mut _, TEXT_BUF_CHARS * 2, None)
                    .is_ok()
                {
                    let len = text_buf.iter().position(|&c| c == 0).unwrap_or(text_buf.len());
                    name = String::from_utf16_lossy(&text_buf[..len]);
                }
            }

            details.push(IconDetail { bounds, icon_rect, label_rect, name });
        }

        let _ = VirtualFreeEx(process_handle, remote_mem, 0, MEM_RELEASE);
        let _ = CloseHandle(process_handle);
        details
    }
}

// ---------------------------------------------------------------------------
// Taskbar window discovery
// ---------------------------------------------------------------------------

/// Finds the taskbar window that lives on `monitor`: `Shell_TrayWnd` for the primary
/// monitor, or the matching `Shell_SecondaryTrayWnd` instance for a secondary one
/// (present when "show taskbar on all displays" is enabled).
fn find_taskbar_window(monitor: &Monitor) -> Option<HWND> {
    let target_bounds = monitor_bounds(monitor)?;

    unsafe {
        if let Ok(primary) = FindWindowW(PCWSTR::from_raw(encode_wide("Shell_TrayWnd").as_ptr()), None) {
            let mut rect = RECT::default();
            if GetWindowRect(primary, &mut rect).is_ok() && bounds_from_rect(rect).intersects(target_bounds) {
                return Some(primary);
            }
        }

        struct SearchData {
            target_bounds: Bounds,
            class_name: Vec<u16>,
            found: HWND,
        }

        unsafe extern "system" fn enum_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
            let data = unsafe { &mut *(lparam.0 as *mut SearchData) };
            let mut class_buf = [0u16; 256];
            let len = unsafe { GetClassNameW(hwnd, &mut class_buf) };
            if len <= 0 {
                return BOOL(1);
            }
            if &class_buf[..len as usize] != &data.class_name[..data.class_name.len() - 1] {
                return BOOL(1);
            }
            let mut rect = RECT::default();
            if unsafe { GetWindowRect(hwnd, &mut rect) }.is_ok()
                && bounds_from_rect(rect).intersects(data.target_bounds)
            {
                data.found = hwnd;
                return BOOL(0); // stop enumerating, we found it
            }
            BOOL(1)
        }

        let mut data = SearchData {
            target_bounds,
            class_name: encode_wide("Shell_SecondaryTrayWnd"),
            found: HWND(std::ptr::null_mut()),
        };
        let _ = EnumWindows(Some(enum_cb), LPARAM(&mut data as *mut _ as isize));

        if !data.found.0.is_null() {
            Some(data.found)
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Taskbar elements (UI Automation)
// ---------------------------------------------------------------------------

fn classify_taskbar_element(name: &str, automation_id: &str) -> TaskbarElementKind {
    let name = name.to_lowercase();
    let id = automation_id.to_lowercase();
    let text = format!("{name} {id}");

    if text.contains("start") {
        TaskbarElementKind::Start
    } else if text.contains("search") {
        TaskbarElementKind::Search
    } else if text.contains("show desktop") || text.contains("showdesktop") {
        TaskbarElementKind::ShowDesktop
    } else if text.contains("tray") || text.contains("notification") || text.contains("overflow") {
        TaskbarElementKind::SystemTray
    } else {
        TaskbarElementKind::Item
    }
}

/// Horizontal gaps in `[start, end)` not covered by any of `occupied` spans (each an
/// `(x, right)` pair, given in any order and free to overlap). Spans narrower than a
/// few pixels are dropped as UIA rounding noise rather than becoming degenerate
/// slivers.
fn residual_spans(start: i32, end: i32, occupied: &[(i32, i32)]) -> Vec<(i32, i32)> {
    const MIN_SPAN: i32 = 3;

    let mut spans: Vec<(i32, i32)> =
        occupied.iter().map(|&(x, r)| (x.max(start), r.min(end))).filter(|&(x, r)| r > x).collect();
    spans.sort_by_key(|&(x, _)| x);

    let mut gaps = Vec::new();
    let mut cursor = start;
    for (x, r) in spans {
        if x > cursor + MIN_SPAN {
            gaps.push((cursor, x));
        }
        cursor = cursor.max(r);
    }
    if end > cursor + MIN_SPAN {
        gaps.push((cursor, end));
    }
    gaps
}

/// Splits a wide empty-space rect into roughly square tiles (each close to
/// `bounds.height` wide) instead of one long thin plank, so it becomes several
/// reasonably-shaped physics objects rather than one oddly-proportioned one. Tile
/// count is just `width / height` rounded to the nearest whole tile (minimum one),
/// with the leftover width spread evenly across all tiles rather than dumped into a
/// single narrower/wider one at an edge.
fn split_into_squares(bounds: Bounds) -> Vec<Bounds> {
    if bounds.width <= 0 || bounds.height <= 0 {
        return vec![bounds];
    }

    let tile_count = (bounds.width as f32 / bounds.height as f32).round().max(1.0) as i32;
    (0..tile_count)
        .map(|i| {
            let x0 = bounds.x + bounds.width * i / tile_count;
            let x1 = bounds.x + bounds.width * (i + 1) / tile_count;
            Bounds { x: x0, y: bounds.y, width: x1 - x0, height: bounds.height }
        })
        .collect()
}

fn bounds_roughly_equal(a: Bounds, b: Bounds) -> bool {
    const TOLERANCE: i32 = 2;
    (a.x - b.x).abs() <= TOLERANCE
        && (a.y - b.y).abs() <= TOLERANCE
        && (a.width - b.width).abs() <= TOLERANCE
        && (a.height - b.height).abs() <= TOLERANCE
}

/// `inner`'s horizontal span falls within `outer`'s (with a couple of pixels of
/// tolerance for UIA rounding) — the check `trim_nested_containers` uses to decide
/// whether one element is really nested inside another.
fn horizontally_contained(inner: Bounds, outer: Bounds) -> bool {
    const TOLERANCE: i32 = 2;
    inner.x >= outer.x - TOLERANCE && inner.right() <= outer.right() + TOLERANCE
}

fn area(b: Bounds) -> i64 {
    b.width as i64 * b.height as i64
}

/// The UIA tree for a taskbar nests real elements inside layout-only wrapper
/// containers (and sometimes inside other real elements, e.g. a "Running
/// applications" toolbar around each app button) at a depth that varies by Windows
/// build — walking a fixed number of levels and guessing which ones are "real" is
/// fragile. Instead, `enumerate_taskbar_elements` collects *every* descendant with a
/// non-empty rect, and this trims each one down: if an element has other, smaller
/// elements nested inside its own horizontal span, the element itself is dropped and
/// replaced by whatever's left of its span once those nested elements are carved out
/// (as `Empty`) — otherwise (a genuine leaf) it's kept as-is. This is what actually
/// removes the "container drawn on top of its own children" duplication, rather than
/// guessing which UIA nodes are containers ahead of time.
///
/// `taskbar_bounds` is used (rather than the trimmed container's own `y`/`height`) for
/// every residual `Empty` piece: it's genuine blank taskbar background, which visually
/// always spans the taskbar's full height, regardless of whether the UIA element it was
/// carved out of happened to have inset/padded bounds narrower than that.
fn trim_nested_containers(elements: &[TaskbarElement], taskbar_bounds: Bounds) -> Vec<TaskbarElement> {
    let mut out = Vec::with_capacity(elements.len());
    for (i, el) in elements.iter().enumerate() {
        let children: Vec<(i32, i32)> = elements
            .iter()
            .enumerate()
            .filter(|&(j, other)| {
                j != i && area(other.bounds) < area(el.bounds) && horizontally_contained(other.bounds, el.bounds)
            })
            .map(|(_, other)| (other.bounds.x, other.bounds.right()))
            .collect();

        if children.is_empty() {
            out.push(*el);
            continue;
        }
        for (x, r) in residual_spans(el.bounds.x, el.bounds.right(), &children) {
            out.push(TaskbarElement {
                kind: TaskbarElementKind::Empty,
                bounds: Bounds { x, y: taskbar_bounds.y, width: r - x, height: taskbar_bounds.height },
            });
        }
    }
    out
}

fn union_taskbar_elements(elements: &[TaskbarElement], kind: TaskbarElementKind) -> Option<TaskbarElement> {
    let first = elements.first()?.bounds;
    let mut left = first.x;
    let mut top = first.y;
    let mut right = first.right();
    let mut bottom = first.bottom();
    for e in &elements[1..] {
        left = left.min(e.bounds.x);
        top = top.min(e.bounds.y);
        right = right.max(e.bounds.right());
        bottom = bottom.max(e.bounds.bottom());
    }
    Some(TaskbarElement { kind, bounds: Bounds { x: left, y: top, width: right - left, height: bottom - top } })
}

/// Collects real, individually-addressable taskbar elements via UI Automation (most
/// taskbar buttons are not separate HWNDs on modern Windows, so this is the only
/// reliable way to get their bounds), classified by loose substring matching on
/// name/automation-id rather than hardcoded exact IDs since those aren't stable across
/// Windows builds/locales.
///
/// The raw UIA tree contains both real elements and the wrapper containers around them
/// (and around each other, at varying nesting depth) — collected naively, that produces
/// exactly the duplication this used to have: a "whole toolbar" element drawn on top of
/// its own individual app-icon children. `trim_nested_containers` resolves that
/// geometrically rather than by guessing tree shape. The system tray is a special case
/// on top of that: it's always merged into one panel rather than kept per-icon (icons
/// inside it would otherwise "contain" each other in confusing ways, and per-element
/// notification icons aren't reliably enumerable across Windows builds anyway). Any
/// remaining gaps (including from the taskbar's own edges to the first/last element)
/// are filled with `Empty` entries so the result tiles the whole taskbar with no holes.
fn enumerate_taskbar_elements(taskbar_hwnd: HWND) -> Vec<TaskbarElement> {
    let mut taskbar_rect = RECT::default();
    if unsafe { GetWindowRect(taskbar_hwnd, &mut taskbar_rect) }.is_err() {
        return Vec::new();
    }
    let taskbar_bounds = bounds_from_rect(taskbar_rect);

    let found = collect_taskbar_elements(taskbar_hwnd).unwrap_or_default();

    // Drop near-exact duplicate rects (a wrapper whose bounds happen to match its
    // single child's) before containment/trimming logic sees them.
    let mut deduped: Vec<TaskbarElement> = Vec::with_capacity(found.len());
    for el in found {
        if !deduped.iter().any(|kept| bounds_roughly_equal(kept.bounds, el.bounds)) {
            deduped.push(el);
        }
    }

    let (tray_fragments, mut rest): (Vec<TaskbarElement>, Vec<TaskbarElement>) =
        deduped.into_iter().partition(|e| e.kind == TaskbarElementKind::SystemTray);
    if let Some(tray) = union_taskbar_elements(&tray_fragments, TaskbarElementKind::SystemTray) {
        rest.push(tray);
    }

    let mut tiled = trim_nested_containers(&rest, taskbar_bounds);

    let occupied: Vec<(i32, i32)> = tiled.iter().map(|e| (e.bounds.x, e.bounds.right())).collect();
    for (x, r) in residual_spans(taskbar_bounds.x, taskbar_bounds.right(), &occupied) {
        tiled.push(TaskbarElement {
            kind: TaskbarElementKind::Empty,
            bounds: Bounds { x, y: taskbar_bounds.y, width: r - x, height: taskbar_bounds.height },
        });
    }

    // Every Empty entry so far (both the residual space left over from trimming a
    // container, and the top-level gap fills just above) is still one rect per gap,
    // however wide — split each into roughly square tiles rather than leaving long
    // thin planks.
    let mut tiled: Vec<TaskbarElement> = tiled
        .into_iter()
        .flat_map(|el| {
            if el.kind == TaskbarElementKind::Empty {
                split_into_squares(el.bounds)
                    .into_iter()
                    .map(|bounds| TaskbarElement { kind: TaskbarElementKind::Empty, bounds })
                    .collect()
            } else {
                vec![el]
            }
        })
        .collect();

    tiled.sort_by_key(|e| e.bounds.x);
    tiled
}

fn collect_taskbar_elements(taskbar_hwnd: HWND) -> windows::core::Result<Vec<TaskbarElement>> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let automation: windows::Win32::UI::Accessibility::IUIAutomation =
            CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;

        let root = automation.ElementFromHandle(taskbar_hwnd)?;
        let condition = automation.CreateTrueCondition()?;
        let descendants = root.FindAll(TreeScope_Descendants, &condition)?;

        let mut elements = Vec::new();
        let count = descendants.Length()?;
        for i in 0..count {
            let element = descendants.GetElement(i)?;
            let Ok(rect) = element.CurrentBoundingRectangle() else {
                continue;
            };
            let bounds = bounds_from_rect(rect);
            if bounds.width <= 0 || bounds.height <= 0 {
                continue;
            }
            let name = element.CurrentName().map(|b| b.to_string()).unwrap_or_default();
            let automation_id = element.CurrentAutomationId().map(|b| b.to_string()).unwrap_or_default();
            elements.push(TaskbarElement { kind: classify_taskbar_element(&name, &automation_id), bounds });
        }
        Ok(elements)
    }
}

// ---------------------------------------------------------------------------
// Font smoothing
// ---------------------------------------------------------------------------

fn font_smoothing_enabled() -> bool {
    let mut enabled = BOOL(0);
    unsafe {
        let _ = SystemParametersInfoW(
            SPI_GETFONTSMOOTHING,
            0,
            Some(&mut enabled as *mut _ as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
    }
    enabled.as_bool()
}

fn font_smoothing_type() -> u32 {
    let mut smoothing_type = 0u32;
    unsafe {
        let _ = SystemParametersInfoW(
            SPI_GETFONTSMOOTHINGTYPE,
            0,
            Some(&mut smoothing_type as *mut _ as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
    }
    smoothing_type
}

/// `SPI_SETFONTSMOOTHINGTYPE` is one of the API's pointer-sized-value-not-pointer
/// quirks: the new type is encoded directly in the `pvparam` slot, not pointed to by it.
fn set_font_smoothing_type(smoothing_type: u32) {
    unsafe {
        let _ = SystemParametersInfoW(
            SPI_SETFONTSMOOTHINGTYPE,
            0,
            Some(smoothing_type as usize as *mut _),
            SPIF_SENDCHANGE,
        );
    }
}

fn redraw_now(hwnd: HWND) {
    unsafe {
        let _ = RedrawWindow(Some(hwnd), None, None, RDW_INVALIDATE | RDW_UPDATENOW | RDW_ALLCHILDREN);
    }
}

/// While alive, forces standard (non-ClearType) font smoothing system-wide; restores
/// whatever was set before on drop.
///
/// ClearType blends each RGB channel independently based on subpixel coverage, rather
/// than blending all three by one shared coverage value. `alpha_diff` assumes the
/// latter — a single alpha recovered from the white/black difference and applied to
/// all three channels — which is exactly how normal alpha-blended content (icon
/// bitmaps, standard grayscale-AA text) composites, but not how ClearType text does.
/// Under ClearType the recovered alpha for icon label text comes out wrong and the
/// unpremultiplied color fringes at glyph edges. Forcing standard smoothing for the
/// duration of the capture keeps every pixel single-alpha, at the cost of the icon
/// labels being captured in that appearance rather than ClearType's.
struct FontSmoothingGuard {
    original_type: u32,
}

impl FontSmoothingGuard {
    /// Switches away from ClearType if it's currently active. Returns `None` (nothing
    /// to restore) if font smoothing is off entirely or already using standard AA —
    /// both of those already blend with a single alpha per pixel.
    fn apply_if_active() -> Option<Self> {
        if !font_smoothing_enabled() {
            return None;
        }
        let original_type = font_smoothing_type();
        if original_type != FE_FONTSMOOTHINGCLEARTYPE {
            return None;
        }
        set_font_smoothing_type(FE_FONTSMOOTHINGSTANDARD);
        Some(Self { original_type })
    }
}

impl Drop for FontSmoothingGuard {
    fn drop(&mut self) {
        set_font_smoothing_type(self.original_type);
    }
}

// ---------------------------------------------------------------------------
// Wallpaper
// ---------------------------------------------------------------------------

fn get_desktop_background_image() -> Option<DynamicImage> {
    let mut buffer = [0u16; 260];
    unsafe {
        let _ = SystemParametersInfoW(
            SPI_GETDESKWALLPAPER,
            buffer.len() as u32,
            Some(buffer.as_mut_ptr() as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
    }
    let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    let path = std::path::PathBuf::from(String::from_utf16_lossy(&buffer[..len]));
    ImageReader::open(path).ok()?.with_guessed_format().ok()?.decode().ok()
}

// ---------------------------------------------------------------------------
// Backdrop window injection
// ---------------------------------------------------------------------------

static BACKDROP_CLASS_COUNTER: AtomicU32 = AtomicU32::new(0);

unsafe extern "system" fn backdrop_wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Empirically, one `DwmFlush` alone isn't enough of a guarantee for a `BitBlt`
/// immediately afterward to see the backdrop window's latest state — composition can
/// still lag a frame behind the paint that just happened. This is the extra settle time
/// added on top of flushing twice.
const DWM_SETTLE_DELAY: Duration = Duration::from_millis(120);

/// Pumps pending messages so `WM_PAINT` fires, then blocks until DWM has composited the
/// change — twice, plus a short settle delay (see `DWM_SETTLE_DELAY`), since a single
/// `DwmFlush` can return after a frame that still predates whatever was just painted or
/// destroyed. After this returns, a `BitBlt` with `CAPTUREBLT` reliably includes (or
/// excludes) the backdrop window's latest state instead of racing it.
fn flush_dwm() {
    unsafe {
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = DwmFlush();
        let _ = DwmFlush();
    }
    sleep(DWM_SETTLE_DELAY);
}

/// Ensures the backdrop window class and its background brush are torn down even if
/// something between registering the class and creating the window fails and returns
/// early (`find_desktop_injection_parent`, `GetWindowRect`, `CreateWindowExW` below all
/// use `?`) — without this, those early returns would leak both the brush and the
/// registered class.
struct BackdropClassGuard {
    name: Vec<u16>,
    hinstance: windows::Win32::Foundation::HINSTANCE,
    brush: windows::Win32::Graphics::Gdi::HBRUSH,
}

impl Drop for BackdropClassGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = UnregisterClassW(PCWSTR(self.name.as_ptr()), Some(self.hinstance));
            let _ = DeleteObject(self.brush.into());
        }
    }
}

/// Creates a full-screen(-ish) solid `color` topmost popup positioned directly behind
/// `target` in the Z-order, waits for DWM to composite it, runs `f`, then tears the
/// window down and waits for DWM again — so by the time this returns, the desktop
/// looks exactly like it did before the call.
///
/// Only used for the taskbar now — desktop icons are rendered directly from the Shell
/// (see `shell_icons`) rather than screen-captured, so they never need a backdrop.
fn with_backdrop<R>(target: HWND, color: [u8; 3], f: impl FnOnce() -> R) -> Option<R> {
    unsafe {
        let hinstance = GetModuleHandleW(None).ok()?;
        let id = BACKDROP_CLASS_COUNTER.fetch_add(1, Ordering::Relaxed);
        let class_name = encode_wide(&format!("_OxyNewtonBackdrop{id}"));

        let colorref = (color[2] as u32) << 16 | (color[1] as u32) << 8 | color[0] as u32;
        let brush = CreateSolidBrush(COLORREF(colorref));

        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(backdrop_wnd_proc),
            hInstance: hinstance.into(),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            hbrBackground: brush,
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 {
            let _ = DeleteObject(brush.into());
            return None;
        }
        let _class_guard = BackdropClassGuard {
            name: class_name.clone(),
            hinstance: hinstance.into(),
            brush,
        };

        let mut rect = RECT::default();
        GetWindowRect(target, &mut rect).ok()?;

        let hwnd = CreateWindowExW(
            WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            PCWSTR(class_name.as_ptr()),
            None,
            WS_POPUP | WS_VISIBLE,
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top,
            None,
            None,
            Some(hinstance.into()),
            None,
        )
        .ok()?;

        let _ = SetWindowPos(hwnd, Some(target), 0, 0, 0, 0, SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE);

        let _ = InvalidateRect(Some(hwnd), None, true);
        let _ = UpdateWindow(hwnd);
        flush_dwm();

        let result = f();

        let _ = DestroyWindow(hwnd);
        flush_dwm();
        // `_class_guard` drops here, unregistering the class and deleting the brush.

        Some(result)
    }
}

// ---------------------------------------------------------------------------
// Top-level capture
// ---------------------------------------------------------------------------

/// Runs one white-or-black backdrop pass behind the taskbar and captures it. `None` if
/// there's no taskbar on this monitor, or the backdrop/capture itself failed.
fn capture_taskbar_pass(
    monitor: &Monitor,
    taskbar_hwnd: HWND,
    taskbar_capture_bounds: Bounds,
    color: [u8; 3],
) -> Option<RgbaImage> {
    with_backdrop(taskbar_hwnd, color, || capture::capture_screen_rect(monitor, taskbar_capture_bounds)).flatten()
}

/// Captures everything for one monitor and returns the raw (undiffed/uncropped-for-the-
/// taskbar) result for `assets.rs` to turn into a `DesktopEntry`. Desktop icons are
/// rendered straight from the Shell (`shell_icons`, no screen capture involved at all);
/// only the taskbar still needs a white/black backdrop pass, since there's no
/// non-screen-capture way to get real per-element taskbar bitmaps.
pub fn capture_desktop(monitor: &Monitor) -> Option<RawCapture> {
    let bounds = monitor_bounds(monitor)?;
    let listview = get_desktop_listview()?;
    let taskbar_hwnd = find_taskbar_window(monitor);

    let icon_details: Vec<IconDetail> = enumerate_desktop_icon_details(listview)
        .into_iter()
        .filter(|d| d.bounds.intersects(bounds))
        .collect();
    let icon_bounds: Vec<Bounds> = icon_details.iter().map(|d| d.bounds).collect();
    let icon_images = shell_icons::render_desktop_icons(&icon_details);

    let taskbar_elements = taskbar_hwnd.map(enumerate_taskbar_elements).unwrap_or_default();
    let taskbar_bounds: Vec<Bounds> = taskbar_elements.iter().map(|e| e.bounds).collect();
    let taskbar_capture_bounds = taskbar_hwnd.and_then(window_bounds);

    // See `FontSmoothingGuard`: ClearType text breaks the single-alpha-per-pixel
    // assumption the white/black diff relies on, so it's held off for both passes.
    let smoothing_guard = FontSmoothingGuard::apply_if_active();
    let has_smoothing_guard = smoothing_guard.is_some();
    if has_smoothing_guard {
        if let Some(hwnd) = taskbar_hwnd {
            redraw_now(hwnd);
        }
    }

    let (taskbar_white, taskbar_black) = match (taskbar_hwnd, taskbar_capture_bounds) {
        (Some(hwnd), Some(cap_bounds)) => (
            capture_taskbar_pass(monitor, hwnd, cap_bounds, [255, 255, 255]),
            capture_taskbar_pass(monitor, hwnd, cap_bounds, [0, 0, 0]),
        ),
        _ => (None, None),
    };

    drop(smoothing_guard);
    if has_smoothing_guard {
        if let Some(hwnd) = taskbar_hwnd {
            redraw_now(hwnd);
        }
    }

    let background = get_desktop_background_image().unwrap_or_else(|| DynamicImage::new_rgba8(1, 1));

    Some(RawCapture {
        monitor_bounds: bounds,
        virtual_screen_bounds: virtual_screen_bounds(),
        background,
        icon_bounds,
        icon_images,
        taskbar_bounds,
        taskbar_capture_bounds,
        taskbar_white,
        taskbar_black,
    })
}
