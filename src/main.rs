#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod assets;
mod engine;
mod os;
mod render;
mod sim;

use app::App;
use winit::event_loop::{ControlFlow, EventLoop};

fn main() {
    os::prelude();

    let mut entries = Vec::new();
    for (index, monitor) in os::enumerate_monitors().into_iter().enumerate() {
        let Some(entry) = assets::capture_desktop_entry(index as i32, &monitor) else {
            continue;
        };
        entries.push((monitor, entry));
    }

    let event_loop = EventLoop::new().expect("failed to create event loop");
    event_loop.set_control_flow(ControlFlow::Poll);

    let mut app = App::new(entries);
    event_loop.run_app(&mut app).expect("event loop exited with an error");
}
