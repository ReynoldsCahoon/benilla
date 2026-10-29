# benilla's `bevy_pbr` fork — what differs from upstream, and how to check

Upstream: [`bevyengine/bevy`](https://github.com/bevyengine/bevy) (`bevy_pbr`), MIT OR Apache-2.0,
version `0.18.1`, copied from the cargo registry as published and wired in through
`[patch.crates-io]` in the workspace root.

## The patch — cube-array fallback on all of iOS

Bevy binds point-light shadows as a cube-array texture in the mesh view bindings every mesh
pipeline shares, and falls back to a plain cube only on the iOS simulator (`target_abi = "sim"`,
decided at compile time). Metal GPU family < 4 (the iPad Air 2's A8X) has no cube arrays either, so
there every mesh pipeline fails to build. The fork keys the fallback on `target_os = "ios"`:
`bevy_pbr/src/render/{mesh_view_bindings,light}.rs` (the binding and the view) and
`bevy_render/src/render_resource/pipeline_cache.rs` (the `NO_CUBE_ARRAY_TEXTURES_SUPPORT`
shader def), which must agree. No other platform's build changes. A runtime decision from wgpu's
`DownlevelFlags::CUBE_ARRAY_TEXTURES` would be the upstreamable form.

## The patch — no comparison samples on iOS

Bevy's shadow lookups (`sample_shadow_map_hardware`, `sample_shadow_cubemap_hardware` in
`bevy_pbr/src/render/shadow_sampling.wgsl`) sample through comparison samplers bound as
arguments, and Metal GPU family 2 compiles a comparison sample only from a constant sampler
("sampler argument for sample_compare must be known at compile time"), so every stock-PBR fragment
(`StandardMaterial`, benilla's nameplates and raid marks among them) fails to build there. Under
the `NO_COMPARISON_SAMPLERS_SUPPORT` shader def, pushed on `target_os = "ios"` in
`bevy_render/src/render_resource/pipeline_cache.rs`, both return 1.0, unshadowed. benilla spawns no
Bevy light, so no shadow map is ever read and nothing drawn changes.

Check: `diff -r <registry>/bevy_pbr-0.18.1 third_party/bevy_pbr` shows only those lines, the shadow-sampling
lines above, and this file.
