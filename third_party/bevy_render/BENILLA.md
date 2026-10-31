# benilla's `bevy_render` fork — what differs from upstream, and how to check

Upstream: [`bevyengine/bevy`](https://github.com/bevyengine/bevy) (`bevy_render`), MIT OR Apache-2.0,
version `0.18.1`, copied from the cargo registry as published and wired in through
`[patch.crates-io]` in the workspace root.

## The patch — the shader composer can be emptied

`PipelineCache::compiling` and `PipelineCache::trim_shader_composer` (`src/render_resource/
pipeline_cache.rs`): naga_oil's composer keeps every import it parsed, once per shader-def set,
for the session (~26 MB on the iPad Air 2 once the pipelines are warm). benilla empties it after
the cache has been idle a while (`benilla_world::shader_trim`); `bevy_shader`'s cache re-adds an
import from its kept source when a later pipeline needs it. Additive: nothing calls them upstream.

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

## The patch — a smaller node table per mesh slab

Each general mesh slab owns an `offset_allocator::Allocator`, and `Allocator::new` sizes its node
table for 128K allocations, written in full up front: 4.5 MB of heap per slab, ~40 MB over the ~9
slabs benilla's vertex layouts make. `src/mesh/allocator.rs` builds each slab's allocator with
`SLAB_MAX_ALLOCS` (16K) nodes instead; the busiest slab peaks below 4096 live meshes in Orgrimmar at
farclip 177. A slab out of nodes refuses the allocation and the mesh takes the next slab or a new
one, the path a full slab already takes. All platforms.

## The patch — one main texture for a view that never ping-pongs

`prepare_view_targets` (`src/view/mod.rs`) gives every view two full-size main textures, `a` and
`b`, the second only for `ViewTarget::post_process_write`. benilla's player-UI and world cameras run
`CameraOutputMode::Skip` and their final passes read the main texture once
(`benilla_world::final_pass`), so `b` is never written: on the iPad Air 2 that is 12.6 MB for the
full-window UI view and the world view's share at its render scale. The fork adds a
`SingleMainTexture` view component, extracted like `Hdr`; on such a view `b` is a clone of `a`.
A post-process write there reads and writes one texture, which wgpu's validation rejects, so a
wrong marker fails loudly. All platforms; only views carrying the marker change.

Check: `diff -r <registry>/bevy_render-0.18.1 third_party/bevy_render` shows only those lines, the shadow-sampling
lines above, the slab node table, the single main texture, and this file.
