# benilla's `naga` fork — what differs from upstream, and how to check

Upstream: [`gfx-rs/wgpu`](https://github.com/gfx-rs/wgpu) (`naga`), MIT OR Apache-2.0, version
`27.0.3`, the version the workspace lock resolves to, copied from the cargo registry as published
and wired in through `[patch.crates-io]` in the workspace root. Every hunk is marked `iPad:`.

## The patch — `src/back/msl/{mod,writer}.rs`

`EntryPointResources::first_instance_buffer`: when wgpu-hal sets it (Metal GPU family < 3 only,
`third_party/wgpu-hal/BENILLA.md`), a vertex entry point that reads `@builtin(instance_index)`
takes `constant uint& _first_instance [[buffer(N)]]` and opens with
`instance_index += _first_instance;`, before the varyings are rebuilt from the arguments, so the
shader sees WGSL's `first_instance + i` while Metal draws from instance 0. Unset, the output is
byte-identical to upstream.

Check: `diff -r <registry>/naga-27.0.3 third_party/naga` shows only those hunks and this file.
