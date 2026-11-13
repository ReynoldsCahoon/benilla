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

## The patch — pending writes submitted every n uploads

wgpu records every `Queue::write_buffer` / `write_texture` of a frame into one "PendingWrites"
command buffer, submitted ahead of the frame's own. A teleport landing prepares hundreds of
textures, meshes and materials in one frame, thousands of copies in that one command buffer, and
the Metal driver of the iPad Air 2 (A8X, iOS 15) keeps the memory behind it and never gives it back
(measured: the same uploads split into ~12 command buffers leave it flat). `src/render_asset.rs`
adds `set_pending_write_flush(n)` and `count_upload_for_flush`, called after each prepared asset
in `prepare_assets` (and `prepare_erased_assets`, `src/erased_render_asset.rs`, which take the
`RenderQueue` for it) and after each mesh copied into its slab (`src/mesh/allocator.rs`); every
n-th call submits the pending writes (`queue.submit` with no command buffers), counted across
asset types. Order is kept: writes after the submit go into the next pending batch. Off (0) by
default, upstream's behaviour; benilla sets 4 on iOS (`benilla_world::world_plugins`).

## The patch — no swapchain clear under a camera

`CameraDriverNode` clears every window "with no camera", testing `camera_windows.contains(id) &&
render_context.has_commands()`; the camera sub graphs it starts run only after the node returns, so
`has_commands()` is false there and every window a camera draws took a full clear first, which the
camera's own output then overwrote (`no_camera_clear_pass`, ~0.8 ms of the iPad Air 2's GPU a
frame). `src/render_graph/camera_driver_node.rs` skips the clear for any window a camera whose graph
runs targets. A window with no such camera is still cleared.

## The patch — identical rewrites skipped

Bevy rewrites `StorageBuffer`, `RawBufferVec` and `BufferVec` contents every frame whether or not
they changed. On the iPad Air 2 each `write_buffer` is a blit in wgpu's PendingWrites command buffer
costing ~0.3 ms of GPU whatever its size (the wow-optimize FINDINGS, 2026-11-13), so
`src/render_resource/buffer_vec.rs` adds `UploadMemo` (the buffer and bytes last uploaded), and the
three `write_buffer`s skip an upload identical to the last one into the same buffer. Never for an
`INDIRECT` buffer, which GPU preprocessing writes between uploads. `DynamicUniformBuffer::get_writer`
(`src/render_resource/uniform_buffer.rs`) writes into memory instead of a staging view and on drop
copies the written span only if it differs, where it used to copy the buffer's whole capacity.

## The patch — a count of budget deferrals

`RenderAssetBytesPerFrame` (benilla sets 4 MB on iOS) pushes an asset past the frame's budget to
the next frame, and nothing outside the render world can tell. `src/render_asset.rs` adds
`BUDGET_DEFERRALS` / `budget_deferrals()`, a count of every such push, incremented in the two
`bpf.exhausted()` branches of `prepare_assets` and of `prepare_erased_assets`
(`src/erased_render_asset.rs`). benilla's portrait booths read it so a still is not taken while
its meshes or textures are still queued (`benilla_app::portrait`). No behaviour change.

Check: `diff -r <registry>/bevy_render-0.18.1 third_party/bevy_render` shows only those lines, the shadow-sampling
lines above, the slab node table, the single main texture, the pending-write flush, the deferral count, the camera-window clear, the upload memo, and this file.
