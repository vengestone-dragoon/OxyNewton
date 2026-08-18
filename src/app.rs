//! winit `ApplicationHandler`: owns every monitor's window, wgpu renderer,
//! and simulation thread, and routes input to the right one.
//!
//! Interaction is a global gate — the first keypress or left-click on *any*
//! window flips one shared [`Started`] flag that every simulation thread
//! already polls, so all monitors start falling at once — while pointer
//! move/grab/release stays scoped to whichever window it happened in, via
//! that window's own [`SimHandle`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rapier2d::prelude::Vector;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, MouseButton, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{KeyCode, PhysicalKey};
#[cfg(not(debug_assertions))]
use winit::window::WindowLevel;
use winit::window::{Fullscreen, Window, WindowAttributes, WindowId};

use crate::assets::DesktopEntry;
use crate::engine::{self, Engine};
use crate::os;
use crate::render::{Gpu, WindowRenderer};
use crate::sim::{self, SimCommand, SimHandle, Started};

/// How long after every window is up and focused to ignore input for
/// purposes of the start gate. Window creation/focus-stealing (especially
/// the release-only `AlwaysOnTop` promotion) can itself trigger a stray
/// input event — winit's own synthetic-key-on-focus being one confirmed
/// source, but not necessarily the only one — within a few milliseconds of
/// startup; real user input never arrives that fast.
const STARTUP_GRACE_PERIOD: Duration = Duration::from_millis(250);

/// Which arrow keys are currently held. Gravity is recomputed directly from
/// this snapshot every time it changes, rather than nudged incrementally per
/// key event, so a key repeating or being held longer never changes the
/// force's magnitude — only which keys are down at that instant does.
#[derive(Default)]
struct HeldDirections {
    up: bool,
    down: bool,
    left: bool,
    right: bool,
}

impl HeldDirections {
    /// A single fixed-magnitude gravity vector pointing in the net direction
    /// of whichever arrow keys are held. Diagonals (e.g. Up+Right) normalize
    /// to the same magnitude as a single axis, so the force is always
    /// uniform regardless of direction. Falls back to straight down when
    /// nothing is held.
    fn to_gravity(&self) -> Vector {
        let x = (self.right as i32 - self.left as i32) as f32;
        let y = (self.down as i32 - self.up as i32) as f32;
        let direction = if x == 0.0 && y == 0.0 { Vector::new(0.0, 1.0) } else { Vector::new(x, y).normalize() };
        direction * engine::GRAVITY_MAGNITUDE
    }
}

struct MonitorWindow {
    window: Arc<Window>,
    renderer: WindowRenderer,
    sim: SimHandle,
    world_size: Vector,
    /// Last cursor position in this window's world space. `MouseInput`
    /// doesn't carry a position in winit, so this is what turns a press into
    /// a `SimCommand::Grab` at the right point.
    last_cursor: Vector,
}

pub struct App {
    gpu: Gpu,
    /// Captured per-monitor data, consumed into real windows/engines the
    /// first time `resumed()` runs (window creation isn't allowed before
    /// then).
    pending: Vec<(os::Monitor, DesktopEntry)>,
    windows: HashMap<WindowId, MonitorWindow>,
    started: Started,
    running: Arc<AtomicBool>,
    held_directions: HeldDirections,
    /// Reset to "now" once every window from this run has been created,
    /// shown, and focused; see `STARTUP_GRACE_PERIOD`.
    startup: Instant,
}

impl App {
    pub fn new(entries: Vec<(os::Monitor, DesktopEntry)>) -> Self {
        Self {
            gpu: Gpu::new(),
            pending: entries,
            windows: HashMap::new(),
            started: Started::new(),
            running: Arc::new(AtomicBool::new(true)),
            held_directions: HeldDirections::default(),
            startup: Instant::now(),
        }
    }

    /// Whether input right now is allowed to trigger the start gate: not
    /// already started, and past the post-window-setup grace period (see
    /// `STARTUP_GRACE_PERIOD`).
    fn can_start(&self) -> bool {
        !self.started.is_started() && self.startup.elapsed() >= STARTUP_GRACE_PERIOD
    }

    /// Updates arrow-key state from a keyboard event and, if it changed
    /// which directions are held, overwrites gravity on every window's
    /// engine at once — gravity is one shared "which way is down" for the
    /// whole desktop, not a per-monitor setting.
    fn handle_direction_key(&mut self, key_event: &KeyEvent) {
        let PhysicalKey::Code(code) = key_event.physical_key else {
            return;
        };
        let pressed = key_event.state == ElementState::Pressed;
        let changed = match code {
            KeyCode::ArrowUp => {
                self.held_directions.up = pressed;
                true
            }
            KeyCode::ArrowDown => {
                self.held_directions.down = pressed;
                true
            }
            KeyCode::ArrowLeft => {
                self.held_directions.left = pressed;
                true
            }
            KeyCode::ArrowRight => {
                self.held_directions.right = pressed;
                true
            }
            _ => false,
        };
        if !changed {
            return;
        }

        let gravity = self.held_directions.to_gravity();
        for monitor_window in self.windows.values() {
            monitor_window.sim.send(SimCommand::Gravity(gravity));
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.pending.is_empty() {
            return; // already set up (winit may redeliver `Resumed`)
        }

        let monitors: Vec<_> = event_loop.available_monitors().collect();

        for (monitor, entry) in self.pending.drain(..) {
            // `os::enumerate_monitors`/capture (Win32 `HMONITOR`) and
            // `event_loop.available_monitors()` (winit) are independent
            // enumerations, so match them up by physical bounds rather than
            // assuming their orders agree.
            let Some(bounds) = os::monitor_bounds(&monitor) else {
                continue;
            };
            let Some(handle) = monitors.iter().find(|m| {
                let pos = m.position();
                let size = m.size();
                pos.x == bounds.x
                    && pos.y == bounds.y
                    && size.width as i32 == bounds.width
                    && size.height as i32 == bounds.height
            }) else {
                eprintln!("no matching display for a captured monitor; skipping its window");
                continue;
            };

            // Icon/taskbar positions were captured in this monitor's real
            // pixel space (`bounds`, from `GetMonitorInfoW`), which is also
            // what the fullscreen window actually gets sized to — not
            // necessarily the wallpaper file's own native resolution (a
            // wallpaper is commonly stored larger/smaller than the current
            // display mode and scaled to fit).
            let world_size = Vector::new(bounds.width as f32, bounds.height as f32);
            let engine = Engine::new(
                entry.background.to_rgba8(),
                entry.icon_images,
                entry.icon_positions,
                entry.icon_sizes,
                entry.taskbar_images,
                entry.taskbar_positions,
                entry.taskbar_sizes,
                world_size,
            );

            // Windows only delivers `RedrawRequested` to *visible* windows —
            // showing this only after a first successful render would
            // deadlock (no render is ever attempted because the window
            // never gets a chance to redraw). Shown immediately instead, so
            // the very first frame may briefly show whatever the swapchain's
            // default contents are, same tradeoff the old pixels-based app
            // made.
            let attributes = WindowAttributes::default()
                .with_title("OxyNewton")
                .with_decorations(false)
                .with_fullscreen(Some(Fullscreen::Borderless(Some(handle.clone()))));
            let window = Arc::new(
                event_loop
                    .create_window(attributes)
                    .expect("failed to create a monitor window"),
            );

            let renderer = WindowRenderer::new(
                &self.gpu,
                Arc::clone(&window),
                world_size,
                engine.background(),
                engine.icons(),
                engine.icon_sizes(),
                engine.taskbar_slices(),
                engine.taskbar_sizes(),
            );

            // Done after the GPU surface is set up rather than right after
            // window creation: doing it earlier (immediately after
            // `create_window`, before the surface exists) reliably crashed
            // the release build with an access violation inside wgpu/DX12 —
            // looks like changing z-order mid-fullscreen-transition raced
            // with surface setup. Debug builds never hit it, presumably
            // just because they're slow enough to not race.
            #[cfg(not(debug_assertions))]
            window.set_window_level(WindowLevel::AlwaysOnTop);

            // The old pixels-based version did this explicitly too. Without
            // it a borderless/topmost window can end up visible but not
            // actually focused, which is enough for Windows to swallow
            // keyboard input to it (and can make mouse-button delivery flaky
            // in multi-monitor topmost setups).
            window.focus_window();

            let sim = sim::spawn(engine, self.started.clone(), Arc::clone(&self.running));

            self.windows.insert(
                window.id(),
                MonitorWindow {
                    window,
                    renderer,
                    sim,
                    world_size,
                    last_cursor: Vector::ZERO,
                },
            );
        }

        for monitor_window in self.windows.values() {
            monitor_window.window.request_redraw();
        }

        // Every window is created/shown/focused as of here — start the grace
        // period now rather than from `App::new()`, since GPU/window setup
        // above can itself take a nontrivial amount of time.
        self.startup = Instant::now();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, window_id: WindowId, event: WindowEvent) {
        // Handled before the per-window borrow below (and regardless of
        // which window has focus) since both the start gate and gravity are
        // global, not scoped to one monitor's window.
        if let WindowEvent::KeyboardInput { event: key_event, is_synthetic: false, .. } = &event {
            // `is_synthetic` events aren't real user input — winit fabricates
            // them for whatever keys are physically held whenever a window
            // gains/loses focus, so calling `focus_window()` on window
            // creation was enough to immediately fire a "key press" here and
            // skip the wait entirely.
            if key_event.state == ElementState::Pressed && self.can_start() {
                self.started.start();
            }
            self.handle_direction_key(key_event);
        }

        let Some(monitor_window) = self.windows.get_mut(&window_id) else {
            return;
        };

        match event {
            WindowEvent::CloseRequested => {
                // One overlay app, N windows: closing any of them closes all
                // of them. Flip `running` before `exit()` returns so every
                // sim thread notices and stops before `App` (and its
                // `SimHandle`s) get dropped and joined.
                self.running.store(false, Ordering::Relaxed);
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                monitor_window.renderer.resize(&self.gpu, size);
            }
            WindowEvent::ScaleFactorChanged { .. } => {
                let size = monitor_window.window.inner_size();
                monitor_window.renderer.resize(&self.gpu, size);
            }
            WindowEvent::CursorMoved { position, .. } => {
                let inner = monitor_window.window.inner_size();
                let scale = Vector::new(
                    monitor_window.world_size.x / (inner.width.max(1) as f32),
                    monitor_window.world_size.y / (inner.height.max(1) as f32),
                );
                let point = Vector::new(position.x as f32 * scale.x, position.y as f32 * scale.y);
                monitor_window.last_cursor = point;
                monitor_window.sim.send(SimCommand::Move(point));
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if button != MouseButton::Left {
                    return;
                }
                if !self.started.is_started() {
                    // The very first press across any window just starts
                    // the simulation everywhere; it doesn't also grab. Presses
                    // during the startup grace period are ignored entirely
                    // rather than consumed as "the" starting press, in case
                    // they're spurious (see `STARTUP_GRACE_PERIOD`).
                    if state == ElementState::Pressed && self.can_start() {
                        self.started.start();
                    }
                    return;
                }
                match state {
                    ElementState::Pressed => monitor_window.sim.send(SimCommand::Grab(monitor_window.last_cursor)),
                    ElementState::Released => monitor_window.sim.send(SimCommand::Release),
                }
            }
            WindowEvent::RedrawRequested => {
                let snapshot = monitor_window.sim.snapshot();
                monitor_window.renderer.render(&self.gpu, &snapshot);
                monitor_window.window.request_redraw();
            }
            _ => {}
        }
    }
}
