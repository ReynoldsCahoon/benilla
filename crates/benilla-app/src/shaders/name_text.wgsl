// Overhead names in the player-UI pass, at the window's resolution ([`crate::name_pass`]). The
// glyph block is the world name's mesh, seated on screen at the anchor; its per-instance `MeshTag`
// holds the anchor's reverse-Z depth, and a fragment the world's resolved depth puts behind
// geometry is dropped, the reference's depth-tested name batch (`0x6c7470` calls `0x5c1d60(1,1)`).
// Output follows `ui_quad.wgsl`: gamma bytes, premultiplied, for `(One, OneMinusSrcAlpha)`.

#import bevy_sprite::mesh2d_functions as mesh_functions
#import bevy_sprite::mesh2d_view_bindings::view

struct Vertex {
    @builtin(instance_index) instance_index: u32,
    @location(0) position: vec3<f32>,
    @location(2) uv: vec2<f32>,
}

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    // The anchor's reverse-Z NDC depth: larger is nearer, 0 the far plane at infinity.
    @location(1) @interpolate(flat) depth: f32,
}

@group(2) @binding(0) var<uniform> color: vec4<f32>;
@group(2) @binding(1) var glyphs: texture_2d<f32>;
@group(2) @binding(2) var glyph_sampler: sampler;
// The world camera's depth at its own render size ([`crate::name_pass`]'s resolve).
@group(2) @binding(3) var world_depth: texture_2d<f32>;

// A name fragment survives a world depth up to this fraction nearer than its anchor: the block
// stands upright at the anchor's depth, and a unit's own head sits just under it.
const DEPTH_SLACK: f32 = 1.01;

@vertex
fn vertex(vertex: Vertex) -> VertexOutput {
    var out: VertexOutput;
    let world_from_local = mesh_functions::get_world_from_local(vertex.instance_index);
    let world_position = mesh_functions::mesh2d_position_local_to_world(
        world_from_local,
        vec4<f32>(vertex.position, 1.0),
    );
    out.position = mesh_functions::mesh2d_position_world_to_clip(world_position);
    out.uv = vertex.uv;
    out.depth = bitcast<f32>(mesh_functions::get_tag(vertex.instance_index));
    return out;
}

fn linear_to_srgb(c: vec3<f32>) -> vec3<f32> {
    let higher = 1.055 * pow(c, vec3<f32>(1.0 / 2.4)) - 0.055;
    let lower = c * 12.92;
    return select(higher, lower, c <= vec3<f32>(0.0031308));
}

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    // The UI target's pixel to the world target's: both span the window, at their own sizes.
    let dims = vec2<f32>(textureDimensions(world_depth));
    let texel = clamp(
        vec2<i32>(in.position.xy * dims / view.viewport.zw),
        vec2<i32>(0),
        vec2<i32>(dims) - vec2<i32>(1),
    );
    let world = textureLoad(world_depth, texel, 0).r;
    if world > in.depth * DEPTH_SLACK {
        discard;
    }
    let t = textureSample(glyphs, glyph_sampler, in.uv);
    // The paint is a byte value, as the world pass wrote it raw into its gamma target.
    let rgb = linear_to_srgb(t.rgb) * color.rgb;
    let a = t.a * color.a;
    return vec4<f32>(rgb * a, a);
}
