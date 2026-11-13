# benilla's `wgpu-hal` fork — what differs from upstream, and how to check

Upstream: [`gfx-rs/wgpu`](https://github.com/gfx-rs/wgpu) (`wgpu-hal`), MIT OR Apache-2.0, version
`27.0.4`, the version the workspace lock resolves to. Wired in through `[patch.crates-io]` in the
workspace root. The crate is copied from the cargo registry as published (`src/`, `build.rs`,
`Cargo.toml`, `README.md`, the two licence files), except that the three `[[example]]` targets
are dropped from `Cargo.toml` and `examples/` is not copied. Every hunk is marked `BENILLA:`.

## Why a fork exists at all

On Vulkan, the default on Windows and Linux, `Surface::acquire_texture` waits up to wgpu-core's 1 s
frame timeout, and a presentation engine that stalls longer (a machine loaded by other work)
used to end the process:

- The post-acquire `vkWaitForFences` on the acquired image's fence returned `VK_TIMEOUT`, which
  went through `map_host_device_oom_and_lost_err` and became `DeviceError::Unexpected`. wgpu-core
  treats that as fatal and loses the device, and Bevy's `prepare_windows` panics with "Couldn't
  get swap chain texture … generic error".
- A `vkAcquireNextImageKHR` `VK_TIMEOUT` returned `Ok(None)`, which Bevy also panics on off Linux.

Upstream tracks it as [gfx-rs/wgpu#9029](https://github.com/gfx-rs/wgpu/issues/9029), still open
in wgpu 30. 1.12.1 has no such timeout: its frame pass presents when the swap chain is ready.
Found in `pkuzic/benilla-everwood_graphics@6fe464d896`.

## The patch — `src/vulkan/instance.rs`, `Surface::acquire_texture`

A `VK_TIMEOUT` is retried while the call has stalled for less than `STALL_BUDGET` (30 s, counted
from the start of `acquire_texture`) and the caller's timeout is not zero (a zero timeout is a
poll, which answers at once by design). Past the budget the result is what upstream returns.

- **`vkAcquireNextImageKHR` times out:** the acquire is issued again with the same semaphore and
  fence. The spec: when a call "does not successfully acquire an image, `semaphore` and `fence`
  are unaffected", so no signal operation is pending and the "unsignaled, no operations
  pending" valid usage still holds. `VK_TIMEOUT` itself is returned only for a timeout above zero
  and below `UINT64_MAX`.
- **`vkWaitForFences` on the acquired image times out:** the wait is issued again. The image is
  already acquired and its fence has a signal operation submitted, so waiting is the only
  correct move; the spec lets a wait be repeated and returns `VK_TIMEOUT` only when the fence is
  not yet signalled.
- **The wait on the previous submission, before the acquire, times out:** upstream drops
  `wait_for_fence`'s `Ok(false)` and acquires with a semaphore the GPU may still hold, which the
  spec forbids ("unsignaled, with no signal or wait operations pending"). The wait is issued
  again under the same budget; past it the acquire goes on as upstream's does.
- **Logging:** each retry logs at `error!` with the time stalled, so a stall shows in the log. A
  result that is not retried is logged raw (`{:?}` of the `vk::Result`) before it is mapped to
  `DeviceError::Unexpected` or `Ok(None)`; the ordinary resize results (`NOT_READY`,
  `ERROR_OUT_OF_DATE_KHR`, `ERROR_SURFACE_LOST_KHR`) are not logged.

What changes in play: a stall under 30 s only delays the frame. A caller that passes a timeout
now waits up to the budget instead of one timeout, so `Ok(None)` ("not ready, skip the frame")
comes only after 30 s.

## How to check

`diff -u <registry>/wgpu-hal-27.0.4/src/vulkan/instance.rs third_party/wgpu-hal/src/vulkan/instance.rs`
shows the patch and nothing else; `diff -r` of the two trees shows only that file, the dropped
`[[example]]` blocks in `Cargo.toml`, and the files not copied (`examples/`, `Cargo.lock`,
`Cargo.toml.orig`, `.cargo_vcs_info.json`, `.cargo-ok`).
`video::tests::wgpu_hal_resolves_to_the_vendored_stall_patch` checks the lock resolves wgpu-hal to
this path, at the lock's version, with the hunks present.

## When to drop it

When wgpu-hal handles the timeout itself ([gfx-rs/wgpu#9029](https://github.com/gfx-rs/wgpu/issues/9029)),
or when wgpu leaves 27: delete this directory and the `wgpu-hal` line under `[patch.crates-io]`.

## The iPad patch — emulated base instance, `src/metal/{mod,device,command}.rs`

Metal GPU family < 3 (the iPad Air 2's A8X) has no base-vertex/base-instance draws and throws an
Objective-C exception on one, which Bevy issues for every mesh batch after the first. On such a
device (`!private_caps.base_vertex_first_instance_drawing`) a pipeline layout reserves a
vertex-stage buffer slot after the sizes buffer, handed to naga as
`EntryPointResources::first_instance_buffer` (`third_party/naga/BENILLA.md`) and kept in
`PipelineStageInfo::first_instance_slot`; every draw writes its first instance there with
`setVertexBytes` and draws from instance 0. Hunks are marked `iPad:`. Devices with base-instance
draws, every Mac included, take no new path. Per-instance vertex buffers would still fetch from
instance 0; nothing in benilla uses one. Base vertex is emulated too (next section).

Measured on the iPad Air 2 (the `vanilla-ipad` spike, 1000 separate draws): 18.5 ms on the render
thread, against 22.4 ms for rebinding the per-object buffer at an offset per draw in Bevy.

## The iPad patch — emulated base vertex, `src/metal/{mod,command,adapter}.rs`

Without base-vertex draws Bevy's mesh allocator gave every mesh's vertex data its own buffer
(`DownlevelFlags::BASE_VERTEX` unset): ~6200 live buffers in Orgrimmar on the iPad, each at
least a 16 KB page. On a device without them, `draw_indexed` now shifts each bound vertex buffer
that the pipeline steps per vertex by `base_vertex × stride` (`setVertexBufferOffset`, which
every family has) and draws at base vertex 0 (`CommandEncoder::emulate_base_vertex`). This hal
always compiles naga's vertex pulling, which bounds-checks every vertex buffer against the size
in the sizes buffer, so a shifted buffer's size shrinks by the shift and the sizes are re-sent.
`CommandState::vertex_buffer_bindings` keeps each binding's offset, size and applied shift, so
an unchanged shift costs nothing; `draw` (non-indexed, whose `first_vertex` Metal takes natively)
and per-instance or constant buffers are kept unshifted. `DownlevelFlags::BASE_VERTEX` is then
reported on every Metal device, and Bevy packs vertex data into shared slabs. Hunks are marked
`iPad:`; a device with base-vertex draws takes no new path.

Measured on the iPad Air 2 (benilla `ipad` branch, the Orgrimmar route): live GPU buffers in The
Drag 6208 -> 1475, screenshots in the Valley of Strength and The Drag drawn whole.

## The iPad GPU meter — `src/metal/mod.rs`, `Queue::submit`

Apple GPUs before the A11 cannot take timestamps at pass boundaries, so bevy's render diagnostics
and benilla's `WOW_GPU_MS` read nothing on the iPad Air 2. Every Apple GPU does record a command
buffer's `GPUStartTime`/`GPUEndTime`. With `GPU_TIMING` set (benilla's iOS `WOW_IOS_PERF=1`), each
submitted command buffer gets a completed handler that adds the part of its span past the latest
end seen to `GPU_BUSY_NS` (the union, so buffers the GPU overlaps are not counted twice), by label
in `GPU_BUSY_BY_LABEL`, and counts itself in `GPU_BUFFERS`. Off, nothing is added to a submit.

## Staging buffers pooled — `src/metal/{staging_pool,device,mod}.rs`

wgpu-core makes a fresh `MAP_WRITE | COPY_SRC` buffer for every `write_buffer`,
`write_buffer_with` and `write_texture` (`StagingBuffer::new`), and destroys it once its submission
completes (`Queue::maintain` dropping the `FlushedStagingBuffer`). On iOS a Metal buffer's creation
and release are costly: on the iPad Air 2 in Stormwind, `queue.submit` took 14-16 ms of the render
thread's ~40 ms frame, and a Mac profile of the same scene put a third of `submit` in
`destroy_buffer` of staging buffers. A staging buffer reaches `destroy_buffer` only after the GPU is
done with it, so the Metal backend keeps it in `staging_pool`, by power-of-two size class (4 KiB to
4 MiB), and the next staging request of that class reuses it; `create_buffer` allocates a staging
buffer at its class size so it can go back. The pool holds at most 16 MiB; past that a returned
buffer is released as before, and a staging buffer over 4 MiB is never pooled. `Buffer::pooled`
marks the ones to return. `staging_pool::ENABLED` (on by default) is benilla's switch: its iOS
`WOW_STAGING_POOL=0` turns the pool off for a measurement.

Check: a pooled buffer is only ever handed out after `destroy_buffer` (the GPU finished with it),
and only to a `MAP_WRITE | COPY_SRC` request, which wgpu-core fills completely before copying from
it.

## Even frame pacing — `src/metal/mod.rs`, `Queue::present`

`PRESENT_MIN_DURATION` (seconds as `f64` bits, 0 by default: upstream's behaviour) presents through
`presentDrawable:afterMinimumDuration:` so each frame stays on the display at least that long.
benilla sets 1/30 s on iOS: the iPad Air 2 cannot hold 60 fps, and presenting as soon as possible
left a frame on for 2, 3 or 4 refreshes and presented ~8% of frames only to be replaced in the same
refresh (measured 2026-11-13: frames at an even 33 ms 54% -> 79%, never shown 159 -> 49 of ~1800,
shown fps unchanged).

