// The world camera's depth into a float colour image the overhead names sample in the player-UI
// pass ([`crate::name_pass`]). A multisampled depth gives its first sample.

#import bevy_core_pipeline::fullscreen_vertex_shader::FullscreenVertexOutput

#ifdef MULTISAMPLED
@group(0) @binding(0) var depth: texture_depth_multisampled_2d;
#else
@group(0) @binding(0) var depth: texture_depth_2d;
#endif

@fragment
fn fragment(in: FullscreenVertexOutput) -> @location(0) vec4<f32> {
    let d = textureLoad(depth, vec2<i32>(in.position.xy), 0);
    return vec4<f32>(d, 0.0, 0.0, 1.0);
}
