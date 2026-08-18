//! OS-agnostic orchestration + pixel math on top of whichever platform backend `os.rs`
//! selects for this compile target. No `windows` (or any other platform) crate type
//! appears anywhere in this file — the only platform-specific thing referenced is the
//! opaque `os::Monitor` handle, which this module never inspects, only passes through.
//!
//! Turns a platform backend's raw capture into a per-monitor [`DesktopEntry`]: the
//! wallpaper, real desktop icon images/positions/sizes (already alpha-correct, passed
//! through as-is), and real taskbar element images/positions/sizes (still needing
//! per-element alpha recovered here by diffing a white/black composited pair, since
//! taskbar elements — unlike desktop icons — have no non-screen-capture source).
//! Everything on `DesktopEntry` is a plain `Vec<T>` of engine/image types, ready to
//! hand straight to `Engine::new`.

use image::{DynamicImage, GenericImageView, RgbaImage};
use rapier2d::prelude::Vector;

use crate::os;

/// A screen-space rectangle, always in physical pixels.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Bounds {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Bounds {
    pub fn right(self) -> i32 {
        self.x + self.width
    }

    pub fn bottom(self) -> i32 {
        self.y + self.height
    }

    pub fn intersects(self, other: Bounds) -> bool {
        self.x < other.right() && other.x < self.right() && self.y < other.bottom() && other.y < self.bottom()
    }

    pub fn center(self) -> (i32, i32) {
        (self.x + self.width / 2, self.y + self.height / 2)
    }

    /// This bounds translated so `origin` becomes (0, 0).
    pub fn relative_to(self, origin: Bounds) -> Bounds {
        Bounds {
            x: self.x - origin.x,
            y: self.y - origin.y,
            width: self.width,
            height: self.height,
        }
    }
}

pub struct DesktopEntry {
    pub monitor_id: i32,
    pub background: DynamicImage,
    pub icon_images: Vec<RgbaImage>,
    pub icon_positions: Vec<Vector>,
    pub icon_sizes: Vec<Vector>,
    pub taskbar_images: Vec<RgbaImage>,
    pub taskbar_positions: Vec<Vector>,
    pub taskbar_sizes: Vec<Vector>,
}

/// Captures everything for one monitor and turns it into an engine-ready
/// [`DesktopEntry`]. All the OS-specific work (minimizing windows, injecting backdrops,
/// screen-grabbing, UI Automation) happens in [`os::capture_desktop`]; everything here
/// is pure image/geometry math over its `Bounds`/`image` output.
pub fn capture_desktop_entry(monitor_id: i32, monitor: &os::Monitor) -> Option<DesktopEntry> {
    let raw = os::capture_desktop(monitor)?;

    // Desktop icons already come back as real, alpha-correct RGBA (rendered directly
    // from the Shell) — no diffing/cropping needed, unlike the taskbar below.
    let icon_images = raw.icon_images;
    // Positions/sizes are handed to Engine::new, whose world space is [0, world_size]
    // per monitor — so they're relative to *this monitor's* origin, not the (possibly
    // negative, for a monitor left of/above the primary) absolute screen coordinate.
    let icon_local_bounds: Vec<Bounds> = raw.icon_bounds.iter().map(|b| b.relative_to(raw.monitor_bounds)).collect();
    let (icon_positions, icon_sizes) = bounds_to_vectors(&icon_local_bounds);

    let taskbar_images = diff_and_crop(
        raw.taskbar_capture_bounds,
        &raw.taskbar_white,
        &raw.taskbar_black,
        &raw.taskbar_bounds,
    );
    let taskbar_local_bounds: Vec<Bounds> =
        raw.taskbar_bounds.iter().map(|b| b.relative_to(raw.monitor_bounds)).collect();
    let (taskbar_positions, taskbar_sizes) = bounds_to_vectors(&taskbar_local_bounds);

    let background = crop_background_to_monitor(raw.background, raw.monitor_bounds, raw.virtual_screen_bounds);

    Some(DesktopEntry {
        monitor_id,
        background,
        icon_images,
        icon_positions,
        icon_sizes,
        taskbar_images,
        taskbar_positions,
        taskbar_sizes,
    })
}

/// Diffs a white/black capture pair (if both are present) and crops out each element's
/// sub-image. Falls back to an empty vec if either capture is missing (e.g. the
/// taskbar/icon layer wasn't found on this monitor).
fn diff_and_crop(
    capture_bounds: Option<Bounds>,
    white: &Option<RgbaImage>,
    black: &Option<RgbaImage>,
    element_bounds: &[Bounds],
) -> Vec<RgbaImage> {
    match (capture_bounds, white, black) {
        (Some(capture_bounds), Some(w), Some(b)) => {
            let diffed = alpha_diff(w, b);
            element_bounds.iter().map(|eb| crop(&diffed, eb.relative_to(capture_bounds))).collect()
        }
        _ => Vec::new(),
    }
}

/// Splits each bounds into an engine-ready center position and full (width, height)
/// size, in the same order as the input.
fn bounds_to_vectors(bounds: &[Bounds]) -> (Vec<Vector>, Vec<Vector>) {
    bounds
        .iter()
        .map(|b| {
            let (cx, cy) = b.center();
            (Vector::new(cx as f32, cy as f32), Vector::new(b.width as f32, b.height as f32))
        })
        .unzip()
}

/// Crops the wallpaper image to this monitor's slice, but only when the wallpaper is
/// clearly a single image spanning the whole virtual desktop (its size matches
/// `virtual_screen_bounds`); per-monitor/fill wallpapers can't be sliced this way, so
/// the full image is returned as a best-effort fallback.
fn crop_background_to_monitor(image: DynamicImage, monitor_bounds: Bounds, virtual_screen_bounds: Bounds) -> DynamicImage {
    let (iw, ih) = image.dimensions();
    if iw != virtual_screen_bounds.width as u32 || ih != virtual_screen_bounds.height as u32 {
        return image;
    }

    let local = monitor_bounds.relative_to(virtual_screen_bounds);
    let x = local.x.clamp(0, iw as i32) as u32;
    let y = local.y.clamp(0, ih as i32) as u32;
    let w = (local.right().clamp(0, iw as i32) as u32).saturating_sub(x);
    let h = (local.bottom().clamp(0, ih as i32) as u32).saturating_sub(y);
    image.crop_imm(x, y, w, h)
}

/// Recovers true per-pixel RGBA from two captures of the same content composited over
/// solid white and solid black. Over background `bg`: `C = a*fg + (1-a)*bg`. So over
/// white, `Cw = a*fg + (1-a)*255`; over black, `Cb = a*fg`. Subtracting gives
/// `a = 1 - (Cw - Cb) / 255`, and `fg = Cb / a` unpremultiplies the black-backed capture.
fn alpha_diff(white: &RgbaImage, black: &RgbaImage) -> RgbaImage {
    let (w, h) = white.dimensions();
    let white_buf = white.as_raw();
    let black_buf = black.as_raw();
    let mut out = vec![0u8; white_buf.len()];

    for i in (0..white_buf.len()).step_by(4) {
        let rw = white_buf[i] as f32;
        let gw = white_buf[i + 1] as f32;
        let bw = white_buf[i + 2] as f32;
        let rb = black_buf[i] as f32;
        let gb = black_buf[i + 1] as f32;
        let bb = black_buf[i + 2] as f32;

        let alpha = (((1.0 - (rw - rb) / 255.0) + (1.0 - (gw - gb) / 255.0) + (1.0 - (bw - bb) / 255.0)) / 3.0)
            .clamp(0.0, 1.0);

        let (r, g, b) = if alpha > 0.01 {
            (
                (rb / alpha).clamp(0.0, 255.0) as u8,
                (gb / alpha).clamp(0.0, 255.0) as u8,
                (bb / alpha).clamp(0.0, 255.0) as u8,
            )
        } else {
            (0, 0, 0)
        };

        out[i] = r;
        out[i + 1] = g;
        out[i + 2] = b;
        out[i + 3] = (alpha * 255.0).round() as u8;
    }

    RgbaImage::from_raw(w, h, out).expect("output buffer matches input dimensions")
}

/// Crops `local` (in `img`'s own coordinate space) out of `img`, clamped to its extents.
fn crop(img: &RgbaImage, local: Bounds) -> RgbaImage {
    let (iw, ih) = img.dimensions();
    let x0 = local.x.clamp(0, iw as i32) as u32;
    let y0 = local.y.clamp(0, ih as i32) as u32;
    let x1 = local.right().clamp(0, iw as i32) as u32;
    let y1 = local.bottom().clamp(0, ih as i32) as u32;
    let w = x1.saturating_sub(x0);
    let h = y1.saturating_sub(y0);
    image::imageops::crop_imm(img, x0, y0, w, h).to_image()
}
