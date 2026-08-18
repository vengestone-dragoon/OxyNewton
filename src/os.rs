//! Compile-time OS backend selection.
//!
//! `main.rs` and `assets.rs` call only the functions re-exported here — never a
//! backend module directly — so picking up a new target is a matter of adding a
//! `#[cfg(target_os = "...")]` block below plus an `os/<name>.rs` that exposes the same
//! shape as `os::win` (`Monitor`, `RawCapture`, `prelude`, `enumerate_monitors`,
//! `capture_desktop`, `monitor_bounds`), not touching any other file.
//!
//! `prelude()` is the one-time startup hook: each backend runs whatever list of
//! platform-specific setup steps it needs (on Windows today, just opting into
//! per-monitor DPI awareness) behind that single call, so `main.rs` never has to know
//! what, if anything, a given target requires before capture functions are used.

#[cfg(target_os = "windows")]
mod win;
#[cfg(target_os = "windows")]
#[allow(unused_imports)] // RawCapture documents the contract even where it's only named via inference
pub use win::{capture_desktop, enumerate_monitors, monitor_bounds, prelude, Monitor, RawCapture};
