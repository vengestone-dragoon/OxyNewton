// Draws one textured, alpha-blended quad per draw call. `sprite` is a tiny
// per-sprite uniform rewritten every frame from the latest simulation
// snapshot; everything else (vertex positions, UVs) is generated from
// `vertex_index` so no vertex buffer is needed for a unit quad.
//
// Packed as two vec4s instead of individual vec2/f32 fields so the WGSL and
// Rust (`bytemuck`) layouts are unambiguous without reasoning about uniform
// struct alignment/padding rules.
struct SpriteUniform {
    // xy = center position, zw = size (both in world space).
    position_size: vec4<f32>,
    // x = rotation (radians), y = unused, zw = world size this sprite's
    // engine is rendering into (used to map world space to clip space).
    rotation_world: vec4<f32>,
}

@group(0) @binding(0) var<uniform> sprite: SpriteUniform;
@group(0) @binding(1) var sprite_texture: texture_2d<f32>;
@group(0) @binding(2) var sprite_sampler: sampler;

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    // A unit quad, centered on the origin, as two triangles.
    var local_positions = array<vec2<f32>, 6>(
        vec2<f32>(-0.5, -0.5),
        vec2<f32>(0.5, -0.5),
        vec2<f32>(0.5, 0.5),
        vec2<f32>(-0.5, -0.5),
        vec2<f32>(0.5, 0.5),
        vec2<f32>(-0.5, 0.5),
    );

    let local = local_positions[vertex_index];
    let position = sprite.position_size.xy;
    let size = sprite.position_size.zw;
    let rotation = sprite.rotation_world.x;
    let world_size = sprite.rotation_world.zw;

    let cos_r = cos(rotation);
    let sin_r = sin(rotation);
    let scaled = local * size;
    let rotated = vec2<f32>(
        scaled.x * cos_r - scaled.y * sin_r,
        scaled.x * sin_r + scaled.y * cos_r,
    );
    let world_pos = position + rotated;

    // World space is [0, world_size] with y pointing down (screen space);
    // clip space is [-1, 1] with y pointing up.
    let clip_x = (world_pos.x / world_size.x) * 2.0 - 1.0;
    let clip_y = 1.0 - (world_pos.y / world_size.y) * 2.0;

    var out: VertexOutput;
    out.clip_position = vec4<f32>(clip_x, clip_y, 0.0, 1.0);
    out.uv = local + vec2<f32>(0.5, 0.5);
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(sprite_texture, sprite_sampler, in.uv);
}
