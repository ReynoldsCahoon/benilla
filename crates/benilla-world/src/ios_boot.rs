//! The iOS boot: the engine settings the iPad Air 2's A8X (Metal GPU family 2) needs and Bevy does
//! not work out for itself, each found and measured on the device by the `vanilla-ipad` spike.
//!
//! - CPU batching: Bevy grades the A8X "preprocessing only" because it has compute shaders, but
//!   that mode draws through indirect buffers, which family 2 lacks (wgpu: no `INDIRECT_EXECUTION`),
//!   and its meshes silently vanish. Paired with `PbrPlugin::use_gpu_instance_buffer_builder` off
//!   in [`crate::boot::tuned_default_plugins`].
//! - One buffer per mesh, so every mesh draw starts at vertex 0: family 2 has no base-vertex draws.
//!   (Base instance is emulated in the vendored wgpu-hal and naga.)
//! - A frame whenever the last is done, paced by vsync. Bevy's default runs iOS as
//!   `ControlFlow::Wait` plus a redraw request UIKit never answers for a Metal-backed view, so frames
//!   only come when a touch wakes the loop; reactive with no wait runs as `ControlFlow::Poll`.
//!
//! Inserted at `build`, after `DefaultPlugins` have built and before their `finish`, where Bevy's
//! own `init_resource` keeps a value already present.

use std::time::Duration;

use bevy::prelude::*;
use bevy::render::batching::gpu_preprocessing::{GpuPreprocessingMode, GpuPreprocessingSupport};
use bevy::render::mesh::allocator::MeshAllocatorSettings;
use bevy::render::RenderApp;
use bevy::winit::{UpdateMode, WinitSettings};

/// See the module docs; added last by [`crate::boot::tuned_default_plugins`] on iOS.
pub struct IosBootPlugin;

impl Plugin for IosBootPlugin {
    fn build(&self, app: &mut App) {
        let every_frame = UpdateMode::Reactive {
            wait: Duration::ZERO,
            react_to_device_events: true,
            react_to_user_events: true,
            react_to_window_events: true,
        };
        app.insert_resource(WinitSettings {
            focused_mode: every_frame,
            unfocused_mode: every_frame,
        });
        app.sub_app_mut(RenderApp)
            .insert_resource(GpuPreprocessingSupport {
                max_supported_mode: GpuPreprocessingMode::None,
            })
            .insert_resource(MeshAllocatorSettings {
                large_threshold: 0,
                ..default()
            });
    }
}
