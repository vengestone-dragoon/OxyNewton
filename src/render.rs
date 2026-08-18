//! wgpu plumbing. One shared [`Gpu`] (instance/adapter/device/queue, plus the
//! single sprite pipeline every window uses) and one [`WindowRenderer`] per
//! monitor window, each owning its own surface and the GPU textures for that
//! monitor's background/icon/taskbar sprites.
//!
//! Rendering is real instanced-per-sprite drawing, not a CPU composite: every
//! icon/taskbar slice/background is uploaded once as a GPU texture at setup
//! time, and each frame just rewrites a tiny per-sprite transform uniform
//! (position, size, rotation) from the latest [`EngineSnapshot`] and issues
//! one draw call per sprite.

use std::sync::{Arc, OnceLock};

use bytemuck::{Pod, Zeroable};
use image::RgbaImage;
use rapier2d::prelude::Vector;
use wgpu::util::{BufferInitDescriptor, DeviceExt, TextureDataOrder};
use winit::dpi::PhysicalSize;
use winit::window::Window;

use crate::engine::EngineSnapshot;

const SHADER_SOURCE: &str = include_str!("render.wgsl");

/// Matches `SpriteUniform` in `render.wgsl` field-for-field. Packed as two
/// `vec4`s so the WGSL and Rust layouts agree without reasoning about WGSL's
/// uniform alignment/padding rules for mixed `vec2`/`f32` fields.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct SpriteUniform {
    position_size: [f32; 4],
    rotation_world: [f32; 4],
}

/// Instance/adapter/device/queue shared by every monitor window, plus the
/// one sprite render pipeline they all draw with. The pipeline needs a
/// concrete surface texture format to be created, which only exists once the
/// first window's surface does — so it's built lazily on first use rather
/// than in `new()`, via `pipeline_for`.
pub struct Gpu {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    shader: wgpu::ShaderModule,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    pipeline: OnceLock<wgpu::RenderPipeline>,
}

impl Gpu {
    pub fn new() -> Self {
        let instance = wgpu::Instance::default();
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        }))
        .expect("no compatible GPU adapter found");

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("OxyNewton device"),
            ..Default::default()
        }))
        .expect("failed to open GPU device");

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("sprite shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER_SOURCE.into()),
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("sprite bind group layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("sprite sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        Self {
            instance,
            adapter,
            device,
            queue,
            shader,
            bind_group_layout,
            sampler,
            pipeline: OnceLock::new(),
        }
    }

    /// The shared sprite pipeline, built against `format` the first time
    /// this is called. Every window is expected to end up with the same
    /// surface format (they share one adapter), so later calls with a
    /// different format would silently reuse the first pipeline — not a
    /// concern on the single-GPU desktop setups this app targets.
    fn pipeline_for(&self, format: wgpu::TextureFormat) -> &wgpu::RenderPipeline {
        self.pipeline.get_or_init(|| self.build_pipeline(format))
    }

    fn build_pipeline(&self, format: wgpu::TextureFormat) -> wgpu::RenderPipeline {
        let pipeline_layout = self.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("sprite pipeline layout"),
            bind_group_layouts: &[Some(&self.bind_group_layout)],
            immediate_size: 0,
        });

        self.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("sprite pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &self.shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &self.shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        })
    }
}

/// One uploaded sprite: its GPU texture plus the tiny uniform buffer holding
/// its current transform (rewritten every frame from the simulation
/// snapshot).
struct SpriteTexture {
    bind_group: wgpu::BindGroup,
    transform_buffer: wgpu::Buffer,
}

impl SpriteTexture {
    fn new(gpu: &Gpu, image: &RgbaImage) -> Self {
        let size = wgpu::Extent3d {
            width: image.width().max(1),
            height: image.height().max(1),
            depth_or_array_layers: 1,
        };

        let texture = gpu.device.create_texture_with_data(
            &gpu.queue,
            &wgpu::TextureDescriptor {
                label: None,
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            },
            TextureDataOrder::LayerMajor,
            image.as_raw(),
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        let transform_buffer = gpu.device.create_buffer_init(&BufferInitDescriptor {
            label: None,
            contents: bytemuck::bytes_of(&SpriteUniform {
                position_size: [0.0; 4],
                rotation_world: [0.0; 4],
            }),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let bind_group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &gpu.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: transform_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&gpu.sampler),
                },
            ],
        });

        Self {
            bind_group,
            transform_buffer,
        }
    }

    fn write_transform(&self, queue: &wgpu::Queue, position: Vector, size: Vector, rotation: f32, world_size: Vector) {
        let uniform = SpriteUniform {
            position_size: [position.x, position.y, size.x, size.y],
            rotation_world: [rotation, 0.0, world_size.x, world_size.y],
        };
        queue.write_buffer(&self.transform_buffer, 0, bytemuck::bytes_of(&uniform));
    }
}

/// Everything needed to draw one monitor's window: its wgpu surface and the
/// GPU sprites for its background/icons/taskbar slices. Icon and taskbar
/// positions/rotations come from a fresh `EngineSnapshot` every `render()`
/// call; everything else here is set up once and reused every frame.
pub struct WindowRenderer {
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    world_size: Vector,
    background: SpriteTexture,
    icons: Vec<SpriteTexture>,
    icon_sizes: Vec<Vector>,
    taskbar: Vec<SpriteTexture>,
    taskbar_sizes: Vec<Vector>,
}

impl WindowRenderer {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        gpu: &Gpu,
        window: Arc<Window>,
        world_size: Vector,
        background: &RgbaImage,
        icons: &[RgbaImage],
        icon_sizes: &[Vector],
        taskbar: &[RgbaImage],
        taskbar_sizes: &[Vector],
    ) -> Self {
        let size = window.inner_size();
        let surface = gpu
            .instance
            .create_surface(window)
            .expect("failed to create window surface");
        let caps = surface.get_capabilities(&gpu.adapter);
        // Sprite textures are uploaded as raw (non-sRGB) bytes straight from
        // a screen capture, so the surface itself must not be sRGB either —
        // otherwise the GPU would apply an extra gamma encode on present and
        // wash out/darken everything relative to the real desktop.
        let format = caps.formats.iter().copied().find(|f| !f.is_srgb()).unwrap_or(caps.formats[0]);

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: size.width.max(1),
            height: size.height.max(1),
            desired_maximum_frame_latency: 2,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
        };
        surface.configure(&gpu.device, &config);
        gpu.pipeline_for(format);

        Self {
            surface,
            config,
            world_size,
            background: SpriteTexture::new(gpu, background),
            icons: icons.iter().map(|image| SpriteTexture::new(gpu, image)).collect(),
            icon_sizes: icon_sizes.to_vec(),
            taskbar: taskbar.iter().map(|image| SpriteTexture::new(gpu, image)).collect(),
            taskbar_sizes: taskbar_sizes.to_vec(),
        }
    }

    pub fn resize(&mut self, gpu: &Gpu, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(&gpu.device, &self.config);
    }

    /// Draws one frame from `snapshot` (background, then icons, then
    /// taskbar slices on top, matching the old CPU renderer's draw order).
    /// Silently skips the frame on any acquire failure other than "surface
    /// needs reconfiguring" — `Resized`/`ScaleFactorChanged` already drive
    /// `resize()`, so the next frame after either should succeed.
    pub fn render(&self, gpu: &Gpu, snapshot: &EngineSnapshot) {
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame) | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&gpu.device, &self.config);
                return;
            }
            _ => return,
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());

        self.background
            .write_transform(&gpu.queue, self.world_size / 2.0, self.world_size, 0.0, self.world_size);
        for ((sprite, &size), transform) in self.icons.iter().zip(&self.icon_sizes).zip(&snapshot.icons) {
            sprite.write_transform(&gpu.queue, transform.position, size, transform.rotation, self.world_size);
        }
        for ((sprite, &size), transform) in self.taskbar.iter().zip(&self.taskbar_sizes).zip(&snapshot.taskbar) {
            sprite.write_transform(&gpu.queue, transform.position, size, transform.rotation, self.world_size);
        }

        let mut encoder = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(gpu.pipeline_for(self.config.format));

            pass.set_bind_group(0, &self.background.bind_group, &[]);
            pass.draw(0..6, 0..1);
            for sprite in &self.icons {
                pass.set_bind_group(0, &sprite.bind_group, &[]);
                pass.draw(0..6, 0..1);
            }
            for sprite in &self.taskbar {
                pass.set_bind_group(0, &sprite.bind_group, &[]);
                pass.draw(0..6, 0..1);
            }
        }
        gpu.queue.submit(Some(encoder.finish()));
        gpu.queue.present(frame);
    }
}
