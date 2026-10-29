//! The iOS boot: the engine settings the iPad Air 2's A8X (Metal GPU family 2) needs and Bevy does
//! not work out for itself, each found and measured on the device by the `vanilla-ipad` spike.
//!
//! - CPU batching: Bevy grades the A8X "preprocessing only" because it has compute shaders, but
//!   that mode draws through indirect buffers, which family 2 lacks (wgpu: no `INDIRECT_EXECUTION`),
//!   and its meshes silently vanish. Paired with `PbrPlugin::use_gpu_instance_buffer_builder` off
//!   in [`crate::boot::tuned_default_plugins`].
//! - One buffer per mesh, so every mesh draw starts at vertex 0: family 2 has no base-vertex draws.
//!   (Base instance is emulated in the vendored wgpu-hal and naga.)
//! - The window counts as focused: winit never reports focus on iOS, and benilla mutes audio and
//!   gates input on an unfocused window. Written without marking the window changed (bevy_winit
//!   would ask winit to focus it, which iOS has no call for), and announced once (`WindowFocused`).
//! - Shadow maps of one texel: Bevy allocates its point and directional shadow maps at full size
//!   (1024² by six faces, 2048²; 40 MB on the device) whether or not a light casts, and benilla
//!   spawns no Bevy light.
//! - A frame whenever the last is done, paced by vsync. Bevy's default runs iOS as
//!   `ControlFlow::Wait` plus a redraw request UIKit never answers for a Metal-backed view, so frames
//!   only come when a touch wakes the loop; reactive with no wait runs as `ControlFlow::Poll`.
//!
//! Inserted at `build`, after `DefaultPlugins` have built and before their `finish`, where Bevy's
//! own `init_resource` keeps a value already present.
//!
//! Two measurement levers, read at launch:
//! - `WOW_IOS_VIEWPORT=<f>`: every 3D camera draws into the top-left `f` of the window on each
//!   axis, the same geometry on `f²` of the pixels: whether a pass's cost follows pixels.
//! - `WOW_IOS_PERF=1`: once a second, an `ios-perf` log line of the mean wall frame, the mean
//!   time the render thread spent in `render_system` (recording, submitting and presenting the
//!   frame, not the vsync wait, which is in acquiring the next swapchain texture), and the mean GPU
//!   busy time per frame from Metal's command-buffer GPU times (`wgpu_hal::metal::GPU_TIMING`).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bevy::prelude::*;
use bevy::render::batching::gpu_preprocessing::{GpuPreprocessingMode, GpuPreprocessingSupport};
use bevy::render::mesh::allocator::MeshAllocatorSettings;
use bevy::render::renderer::render_system;
use bevy::render::{Render, RenderApp, RenderSystems};
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
        app.add_systems(PreUpdate, hold_focus);
        app.insert_resource(bevy::light::PointLightShadowMap { size: 1 })
            .insert_resource(bevy::light::DirectionalLightShadowMap { size: 1 });
        app.sub_app_mut(RenderApp)
            .insert_resource(GpuPreprocessingSupport {
                max_supported_mode: GpuPreprocessingMode::None,
            })
            .insert_resource(MeshAllocatorSettings {
                large_threshold: 0,
                ..default()
            });
        if let Some(fraction) = std::env::var("WOW_IOS_VIEWPORT")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
        {
            app.add_systems(
                PostUpdate,
                move |windows: Query<&Window>, mut cameras: Query<&mut Camera, With<Camera3d>>| {
                    shrink_viewports(fraction, &windows, &mut cameras);
                },
            );
        }
        if std::env::var("WOW_IOS_PERF").as_deref() == Ok("1") {
            wgpu_hal::metal::GPU_TIMING.store(true, Ordering::Relaxed);
            app.add_systems(Last, log_perf);
            app.sub_app_mut(RenderApp)
                .insert_resource(RenderClock(Instant::now()))
                .add_systems(
                    Render,
                    (
                        (|mut clock: ResMut<RenderClock>| clock.0 = Instant::now())
                            .before(render_system),
                        (|clock: Res<RenderClock>| {
                            let ns = clock.0.elapsed().as_nanos() as u64;
                            RENDER_NS_SUM.fetch_add(ns, Ordering::Relaxed);
                            RENDER_FRAMES.fetch_add(1, Ordering::Relaxed);
                        })
                        .after(render_system),
                    )
                        .in_set(RenderSystems::Render),
                );
        }
    }
}

/// Keeps the window focused; see the module docs.
fn hold_focus(
    mut windows: Query<(Entity, &mut Window)>,
    mut focused: MessageWriter<bevy::window::WindowFocused>,
) {
    for (window, mut w) in &mut windows {
        if !w.focused {
            w.bypass_change_detection().focused = true;
            focused.write(bevy::window::WindowFocused {
                window,
                focused: true,
            });
        }
    }
}

/// `WOW_IOS_VIEWPORT`: each 3D camera's viewport, the top-left `fraction` of the window per axis.
fn shrink_viewports(
    fraction: f32,
    windows: &Query<&Window>,
    cameras: &mut Query<&mut Camera, With<Camera3d>>,
) {
    let Some(window) = windows.iter().next() else {
        return;
    };
    let size = (window.physical_size().as_vec2() * fraction)
        .as_uvec2()
        .max(UVec2::ONE);
    for mut camera in cameras.iter_mut() {
        if camera.viewport.as_ref().map(|v| v.physical_size) != Some(size) {
            camera.viewport = Some(bevy::camera::Viewport {
                physical_position: UVec2::ZERO,
                physical_size: size,
                ..default()
            });
        }
    }
}

/// The render thread's `render_system` time and frame count since the last `ios-perf` line.
static RENDER_NS_SUM: AtomicU64 = AtomicU64::new(0);
static RENDER_FRAMES: AtomicU64 = AtomicU64::new(0);

/// When this frame's `render_system` began, on the render thread.
#[derive(Resource)]
struct RenderClock(Instant);

/// The main thread's half of `WOW_IOS_PERF`: frames and wall time since the last line.
#[derive(Default)]
struct PerfWindow {
    since: Option<Instant>,
    frames: u32,
}

fn log_perf(mut window: Local<PerfWindow>) {
    let now = Instant::now();
    let since = *window.since.get_or_insert(now);
    window.frames += 1;
    let elapsed = now - since;
    if elapsed < Duration::from_secs(1) {
        return;
    }
    let render_ns = RENDER_NS_SUM.swap(0, Ordering::Relaxed);
    let render_frames = RENDER_FRAMES.swap(0, Ordering::Relaxed).max(1);
    let gpu_ns = wgpu_hal::metal::GPU_BUSY_NS.swap(0, Ordering::Relaxed);
    let gpu_buffers = wgpu_hal::metal::GPU_BUFFERS.swap(0, Ordering::Relaxed);
    let frames = f64::from(window.frames.max(1));
    let frame_ms = elapsed.as_secs_f64() * 1000.0 / frames;
    info!(
        "ios-perf frame_ms={frame_ms:.2} fps={:.1} render_ms={:.2} gpu_ms={:.2} cmd_bufs={:.1}",
        1000.0 / frame_ms,
        render_ns as f64 / render_frames as f64 / 1.0e6,
        gpu_ns as f64 / frames / 1.0e6,
        gpu_buffers as f64 / frames,
    );
    // The heaviest command buffers by label, each as GPU ms per frame.
    if let Some(by_label) = wgpu_hal::metal::GPU_BUSY_BY_LABEL.lock().take() {
        let mut top: Vec<_> = by_label.into_iter().collect();
        top.sort_by_key(|&(_, ns)| std::cmp::Reverse(ns));
        let top: Vec<String> = top
            .iter()
            .take(8)
            .map(|(label, ns)| format!("{label}={:.2}", *ns as f64 / frames / 1.0e6))
            .collect();
        info!("ios-perf-gpu {}", top.join(" "));
    }
    *window = PerfWindow {
        since: Some(now),
        frames: 0,
    };
}
