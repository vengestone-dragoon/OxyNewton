//! Per-engine simulation thread: steps physics on a fixed tick, independent
//! of rendering and of every other engine's thread, so N monitors' physics
//! never serialize behind each other or behind the render/event loop.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rapier2d::prelude::Vector;

use crate::engine::{Engine, EngineSnapshot};

/// Target simulation tick rate. Decoupled from any window's refresh rate —
/// each engine steps on its own schedule regardless of how often it's drawn.
const TICK_RATE: f64 = 120.0;

/// Upper bound on a single tick's `dt`. Without this, a stall (breakpoint,
/// laptop sleep, a slow tick) would hand the physics step a huge elapsed
/// time and launch every icon off-screen in one frame.
const MAX_DT: f32 = 0.05;

/// Commands the main/event-loop thread sends to one engine's simulation
/// thread. Pointer coordinates are already in that engine's world space.
pub enum SimCommand {
    Move(Vector),
    Grab(Vector),
    Release,
    /// Overwrites this engine's gravity outright (see `Engine::set_gravity`).
    Gravity(Vector),
}

/// Whether the simulation has been kicked off yet, shared by every engine's
/// simulation thread. Each thread only ever reads it to decide whether to
/// step this tick, so `Relaxed` ordering is enough — there's no other state
/// that needs to be synchronized alongside it.
#[derive(Clone)]
pub struct Started(Arc<AtomicBool>);

impl Started {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub fn start(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_started(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// A running simulation thread for one `Engine`. Dropping this blocks until
/// the thread notices `running` has gone false and exits — callers must
/// flip `running` first (e.g. on window close) or the drop will hang.
pub struct SimHandle {
    commands: Sender<SimCommand>,
    snapshot: Arc<Mutex<EngineSnapshot>>,
    join: Option<JoinHandle<()>>,
}

impl SimHandle {
    pub fn send(&self, command: SimCommand) {
        // The receiver only goes away once the thread has exited, which only
        // happens after `running` goes false — at that point nobody's
        // sending new commands anyway, so a failed send is fine to ignore.
        let _ = self.commands.send(command);
    }

    pub fn snapshot(&self) -> EngineSnapshot {
        self.snapshot.lock().unwrap().clone()
    }
}

impl Drop for SimHandle {
    fn drop(&mut self) {
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Spawns a dedicated thread that steps `engine` on a fixed tick until
/// `running` goes false. Physics only advances once `started.is_started()`
/// — until then the engine stays frozen, but a snapshot is still published
/// every tick so the render thread always has something to draw.
pub fn spawn(mut engine: Engine, started: Started, running: Arc<AtomicBool>) -> SimHandle {
    let (tx, rx) = mpsc::channel();
    let snapshot = Arc::new(Mutex::new(engine.snapshot()));
    let snapshot_handle = Arc::clone(&snapshot);
    let tick_duration = Duration::from_secs_f64(1.0 / TICK_RATE);

    let join = thread::spawn(move || {
        let mut last_tick = Instant::now();

        while running.load(Ordering::Relaxed) {
            loop {
                match rx.try_recv() {
                    Ok(SimCommand::Move(point)) => engine.move_pointer(point),
                    Ok(SimCommand::Grab(point)) => {
                        engine.try_grab(point);
                    }
                    Ok(SimCommand::Release) => engine.release_grab(),
                    Ok(SimCommand::Gravity(gravity)) => engine.set_gravity(gravity),
                    Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
                }
            }

            let now = Instant::now();
            if started.is_started() {
                let dt = (now - last_tick).as_secs_f32().min(MAX_DT);
                engine.process(dt);
            }
            last_tick = now;

            *snapshot_handle.lock().unwrap() = engine.snapshot();

            thread::sleep(tick_duration);
        }
    });

    SimHandle {
        commands: tx,
        snapshot,
        join: Some(join),
    }
}
