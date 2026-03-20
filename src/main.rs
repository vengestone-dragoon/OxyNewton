//main.rs
#![windows_subsystem = "windows"]
mod win;

use crate::win::{capture_all_icons, get_wallpaper_pixels, slice_taskbar, CollisionShape, IconData};
use image::imageops::FilterType;
use image::DynamicImage;
use pixels::{Pixels, SurfaceTexture};
use rapier2d::prelude::*;
use std::sync::Arc;
use std::time::Instant;
use windows::Win32::UI::Controls::LVM_GETITEMCOUNT;
use windows::Win32::UI::WindowsAndMessaging::{SendMessageW, SystemParametersInfoW, SPI_GETDESKWALLPAPER, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Fullscreen, Window, WindowAttributes, WindowId};
#[cfg(not(debug_assertions))]
use winit::window::WindowLevel;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let event_loop = EventLoop::new()?;
    let mut main = AppMain::new();
    event_loop.set_control_flow(ControlFlow::Poll);
    event_loop.run_app(&mut main)?;
    Ok(())
}

struct AppMain<'a> {
    startup_time: Instant,
    window: Option<Arc<Window>>,
    window_w: u32,
    window_h: u32,
    pixels: Option<Pixels<'a>>,
    desktop_background: Option<DynamicImage>,
    adjusted_background: Option<Vec<u8>>,
    icons: Vec<IconData>,
    
    
    rigid_body_set: RigidBodySet,
    collider_set: ColliderSet,
    physics_pipeline: PhysicsPipeline,
    island_manager: IslandManager,
    broad_phase: BroadPhaseBvh,
    narrow_phase: NarrowPhase,
    impulse_joint_set: ImpulseJointSet,
    multibody_joint_set: MultibodyJointSet,
    ccd_solver: CCDSolver,
    
    // Map of Icon index to Rapier Handle
    icon_bodies: Vec<RigidBodyHandle>,
    integration_parameters: IntegrationParameters,
    
    grabbed_body: Option<RigidBodyHandle>,
    grab_offset: Vec2, // Add this!
    mouse_pos: Vec2,
    started: bool,
}
impl AppMain<'_> {
    fn new()-> Self {
        let broad_phase = Default::default();
        let narrow_phase = Default::default();
        Self {
            startup_time: Instant::now(),
            window: None,
            window_w: 0,
            window_h: 0,
            pixels: None,
            desktop_background: None,
            adjusted_background: None,
            icons: Vec::new(),
            
            rigid_body_set: RigidBodySet::new(),
            collider_set: ColliderSet::new(),
            physics_pipeline: PhysicsPipeline::new(),
            island_manager: IslandManager::new(),
            broad_phase,
            narrow_phase,
            impulse_joint_set: ImpulseJointSet::new(),
            multibody_joint_set: MultibodyJointSet::new(),
            ccd_solver: CCDSolver::new(),
            icon_bodies: Vec::new(),
            integration_parameters: IntegrationParameters::default(),
            
            grabbed_body: None,
            grab_offset: Default::default(),
            mouse_pos: Default::default(),
            started: false,
        }
    }
    fn handle_mouse_down(&mut self) {// 1. Update the query pipeline with the current state of the physics world
        let query_pipeline = self.broad_phase.as_query_pipeline(self.narrow_phase.query_dispatcher(), &self.rigid_body_set, &self.collider_set, QueryFilter::exclude_fixed());
        let point = Vector::new(self.mouse_pos.x, self.mouse_pos.y);
        
        let hit = query_pipeline.project_point(
            point,
            0.0,
            true,
        );
        
        if let Some((handle, projection)) = hit {
            if projection.is_inside {
                if let Some(collider) = self.collider_set.get(handle) {
                    if let Some(parent_handle) = collider.parent() {
                        self.grabbed_body = Some(parent_handle);
                        
                        // Calculate and store the offset
                        if let Some(body) = self.rigid_body_set.get(parent_handle) {
                            let body_pos = body.translation();
                            // Offset = Mouse position - Body center
                            self.grab_offset = self.mouse_pos - body_pos;
                        }
                    }
                }
            }
        }
    }
    
    fn handle_mouse_up(&mut self) {
        self.grabbed_body = None;
    }
    fn scan_desktop_icons(&mut self) {
        
        // Get original wallpaper path first
        let mut buffer = [0u16; 260];
        unsafe {
            let _ = SystemParametersInfoW(
                SPI_GETDESKWALLPAPER, buffer.len() as u32,
                Some(buffer.as_mut_ptr() as *mut _),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            );
        }
        let len = buffer.iter().position(|&i| i == 0).unwrap_or(buffer.len());
        let original_path = String::from_utf16_lossy(&buffer[..len]);
        
        // Single-pass icon capture with transparency
        self.icons = capture_all_icons(&original_path);
        println!("Captured {} icons with transparency", self.icons.len());
        
        // Taskbar slices unchanged
        let taskbar_elements = slice_taskbar(self.window_w as i32 / 100);
        println!("Found {} taskbar elements!", taskbar_elements.len());
        self.icons.extend(taskbar_elements);
    }
    fn resize_wallpaper(&mut self) {
        let resized = self.desktop_background.clone().unwrap().resize_exact(self.window_w,self.window_h,FilterType::Lanczos3);
        self.adjusted_background = Some(resized.into_rgba8().into_raw())
    }
    fn init_physics(&mut self) {
        let margin = 2.0; // The "buffer" zone in pixels
        let thickness = 20.0; // How thick the invisible walls are
        
        let w = self.window_w as f32;
        let h = self.window_h as f32;
        
        // FLOOR: Place it 'margin' pixels below the bottom edge
        let floor_collider = ColliderBuilder::cuboid(w / 2.0 + margin, thickness / 2.0)
            .translation(Vec2::new(w / 2.0, h + (thickness / 2.0) + margin))
            .restitution(0.6)
            .build();
        self.collider_set.insert(floor_collider);
        
        // CEILING: Place it 'margin' pixels above the top edge
        let ceiling_collider = ColliderBuilder::cuboid(w / 2.0 + margin, thickness / 2.0)
            .translation(Vec2::new(w / 2.0, -(thickness / 2.0) - margin))
            .build();
        self.collider_set.insert(ceiling_collider);
        
        // LEFT WALL: Place it 'margin' pixels to the left
        let wall_left_collider = ColliderBuilder::cuboid(thickness / 2.0, h / 2.0 + margin)
            .translation(Vec2::new(-(thickness / 2.0) - margin, h / 2.0))
            .build();
        self.collider_set.insert(wall_left_collider);
        
        // RIGHT WALL: Place it 'margin' pixels to the right
        let wall_right_collider = ColliderBuilder::cuboid(thickness / 2.0, h / 2.0 + margin)
            .translation(Vec2::new(w + (thickness / 2.0) + margin, h / 2.0))
            .build();
        self.collider_set.insert(wall_right_collider);
        
        for icon in &self.icons {
            let rigid_body = RigidBodyBuilder::dynamic()
                .translation(Vec2::new(icon.x as f32, icon.y as f32))
                .angular_damping(0.5)
                .linear_damping(0.3)
                .can_sleep(false) // Keep them moving!
                .build();
            
            let handle = self.rigid_body_set.insert(rigid_body);
            
            let collider = match icon.shape {
                CollisionShape::Circle => ColliderBuilder::ball(icon.width.min(icon.height) as f32 / 2.0),
                CollisionShape::Quad => ColliderBuilder::cuboid(icon.width as f32 / 2.0, icon.height as f32 / 2.0)
            }
                .restitution(0.7) // Make them bouncy
                .friction(0.3)
                .build();
            
            self.collider_set.insert_with_parent(collider, handle, &mut self.rigid_body_set);
            self.icon_bodies.push(handle);
        }
    }
    fn step_physics(&mut self) {
        let gravity = Vec2::new(0.0, 981.0); // High gravity for pixel units
        let physics_hooks = ();
        let event_handler = ();
        let max_velocity = Vector {x: 5000.0, y: 5000.0};
        if let Some(handle) = self.grabbed_body {
            if let Some(body) = self.rigid_body_set.get_mut(handle) {
                // 1. Where the center SHOULD be to keep your mouse at the grab_offset
                let target_center = self.mouse_pos - self.grab_offset;
                let current_center = body.translation();
                
                // 2. Calculate the displacement needed
                let displacement = target_center - current_center;
                
                // 3. Velocity = Distance / Time
                // Assuming 60fps, delta_time is 1.0/60.0. So we multiply by 60.
                let required_velocity = displacement * 60.0;
                
                // 4. Inject the velocity
                body.set_linvel(required_velocity, true);
                
                // 5. Stop it from spinning wildly while held
                body.set_angvel(body.angvel() * 0.9, true);
            }
        }
        self.physics_pipeline.step(
            gravity,
            &self.integration_parameters,
            &mut self.island_manager,
            &mut self.broad_phase,
            &mut self.narrow_phase,
            &mut self.rigid_body_set,
            &mut self.collider_set,
            &mut self.impulse_joint_set,
            &mut self.multibody_joint_set,
            &mut self.ccd_solver,
            &physics_hooks,
            &event_handler,
        );
        
        for (_handle, body) in self.rigid_body_set.iter_mut() {
            if body.is_dynamic() {
                let vel = body.linvel();
                body.set_linvel(vel.max(-max_velocity).min(max_velocity),true)
            }
        }
        let margin = 50.0; // How far offscreen they can go before snapping back
        
        for handle in &self.icon_bodies {
            if let Some(body) = self.rigid_body_set.get_mut(*handle) {
                let pos = body.translation();
                let mut new_pos = pos;
                let mut reset_needed = false;
                
                // Check Left/Right bounds
                if pos.x < -margin || pos.x > self.window_w as f32 + margin {
                    new_pos.x = self.window_w as f32 / 2.0;
                    reset_needed = true;
                }
                
                // Check Top/Bottom bounds
                if pos.y < -margin || pos.y > self.window_h as f32 + margin {
                    new_pos.y = self.window_h as f32 / 2.0;
                    reset_needed = true;
                }
                
                if reset_needed {
                    // Reset position to center and kill momentum so they don't fly off again
                    body.set_translation(new_pos, true);
                    body.set_linvel(Vec2 {x: 0.0,y: 0.0}, true);
                    body.set_angvel(0.0, true);
                }
            }
        }
        for (i, handle) in self.icon_bodies.iter().enumerate() {
            let body = &self.rigid_body_set[*handle];
            self.icons[i].x = body.translation().x as i32;
            self.icons[i].y = body.translation().y as i32;
            self.icons[i].rotation = body.rotation().angle();
        }
    }
}
impl ApplicationHandler for AppMain<'_> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_none() {
            let attributes = WindowAttributes::default()
                .with_title("OxyNewton")
                .with_decorations(false)
                .with_visible(false)
                .with_fullscreen(Some(Fullscreen::Borderless(event_loop.primary_monitor())));
            
            
            self.window = Some(Arc::new(event_loop.create_window(attributes).unwrap()));
            let window = self.window.as_ref().unwrap();
            self.window_w = window.inner_size().width;
            self.window_h = window.inner_size().height;
            let size = window.inner_size();
            let surface_texture = SurfaceTexture::new(size.width,size.height,window.clone());
            let pixels = Pixels::new(size.width,size.height,surface_texture).unwrap();
            
            self.pixels = Some(pixels);
            self.desktop_background = get_wallpaper_pixels();
            self.resize_wallpaper();
            self.scan_desktop_icons();
            self.init_physics();
            self.startup_time = Instant::now();
            self.window.as_ref().unwrap().set_visible(true);
            #[cfg(not(debug_assertions))]
            self.window.as_ref().unwrap().set_window_level(WindowLevel::AlwaysOnTop);
        }
    }
    
    fn window_event(&mut self, event_loop: &ActiveEventLoop, _window_id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                event_loop.exit()
            },
            WindowEvent::RedrawRequested => {
                if self.started {
                    self.step_physics()
                }
                if let Some(pixels) = &mut self.pixels {
                    if let Some(background) = self.adjusted_background.as_ref() {
                        let frame = pixels.frame_mut();
                        frame.copy_from_slice(&background);
                        for icon in &self.icons {
                            blit_rotated(frame,self.window_w,self.window_h,icon)
                        }
                    }
                    if pixels.render().is_err() {
                        event_loop.exit()
                    }
                }
                self.window.as_ref().unwrap().request_redraw()
            },
            WindowEvent::CursorMoved { position, .. } => {
                self.mouse_pos = Vec2::new(position.x as f32, position.y as f32);
            },
            WindowEvent::KeyboardInput { device_id: _device_id,event, is_synthetic: _is_synthetic } => {
                if event.state == winit::event::ElementState::Pressed && !self.started && self.startup_time.elapsed().as_millis() > 100{
                    self.started = true
                }
            },
            WindowEvent::MouseInput { state, button, .. } => {
                if button == winit::event::MouseButton::Left && self.started {
                    if state == winit::event::ElementState::Pressed {
                        self.handle_mouse_down();
                    } else {
                        self.handle_mouse_up();
                    }
                }
                if state == winit::event::ElementState::Pressed && !self.started {
                    self.started = true;
                }
            }
            _ => { }
        }
    }
}

fn blit_rotated(frame: &mut [u8], frame_w: u32, frame_h: u32, icon: &IconData) {
    let (img_w, img_h, pixels) = match &icon.image {
        Some(data) => data,
        None => return,
    };
    
    let cos_a = icon.rotation.cos();
    let sin_a = icon.rotation.sin();
    let hw = *img_w as f32 / 2.0;
    let hh = *img_h as f32 / 2.0;
    
    // Bounding box for optimization
    let radius = (hw * hw + hh * hh).sqrt().ceil() as i32;
    let min_x = (icon.x - radius).max(0);
    let max_x = (icon.x + radius).min(frame_w as i32 - 1);
    let min_y = (icon.y - radius).max(0);
    let max_y = (icon.y + radius).min(frame_h as i32 - 1);
    
    for py in min_y..=max_y {
        for px in min_x..=max_x {
            let dx = (px - icon.x) as f32;
            let dy = (py - icon.y) as f32;
            
            let src_x = dx * cos_a + dy * sin_a + hw;
            let src_y = -dx * sin_a + dy * cos_a + hh;
            
            // Check boundaries with a 1-pixel margin for interpolation
            if src_x >= 0.0 && src_x < (*img_w - 1) as f32 && src_y >= 0.0 && src_y < (*img_h - 1) as f32 {
                let x0 = src_x.floor() as u32;
                let y0 = src_y.floor() as u32;
                let x1 = x0 + 1;
                let y1 = y0 + 1;
                
                let tx = src_x - x0 as f32;
                let ty = src_y - y0 as f32;
                
                // Sample 4 neighboring pixels
                let p00 = get_pixel(pixels, *img_w, x0, y0);
                let p10 = get_pixel(pixels, *img_w, x1, y0);
                let p01 = get_pixel(pixels, *img_w, x0, y1);
                let p11 = get_pixel(pixels, *img_w, x1, y1);
                
                // Interpolate colors (RGBA)
                let mut lerped = [0u8; 4];
                for i in 0..4 {
                    let top = p00[i] as f32 * (1.0 - tx) + p10[i] as f32 * tx;
                    let bottom = p01[i] as f32 * (1.0 - tx) + p11[i] as f32 * tx;
                    lerped[i] = (top * (1.0 - ty) + bottom * ty) as u8;
                }
                
                let dst_idx = (py as u32 * frame_w + px as u32) as usize * 4;
                let alpha = lerped[3] as f32 / 255.0;
                
                if alpha > 0.0 {
                    // Standard Alpha Blending: dst = src * alpha + dst * (1 - alpha)
                    for i in 0..3 {
                        let src_c = lerped[i] as f32;
                        let dst_c = frame[dst_idx + i] as f32;
                        frame[dst_idx + i] = (src_c * alpha + dst_c * (1.0 - alpha)) as u8;
                    }
                    // Optional: You can choose to keep the background alpha or set to 255
                    frame[dst_idx + 3] = 255;
                }
            }
        }
    }
}

// Helper to get pixel as [R, G, B, A]
fn get_pixel(data: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    let i = (y * width + x) as usize * 4;
    [data[i], data[i+1], data[i+2], data[i+3]]
}