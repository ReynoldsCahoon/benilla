//! The tuned Bevy boot: the `DefaultPlugins` set every benilla binary stands on. The engine tuning
//! is shared; the `Window` is the caller's.

use bevy::app::{PluginGroupBuilder, TaskPoolOptions, TaskPoolPlugin};
use bevy::prelude::*;

use crate::thread_qos;

/// `DefaultPlugins` with benilla's engine tuning applied, around the caller's primary window.
/// The log level: info off iOS. On iOS, warnings: Bevy logs there through `tracing-oslog`, which
/// makes an `os_activity` and a unified-log entry for every span entered, and the interface enters
/// several `info_span!`s every frame (`ui_script: tick`, `resolve`, `measure`, `extract`): ~165
/// log lines a second on the iPad Air 2, and about half its malloc growth over a 25-minute quest
/// soak (9.5 -> 3.4 MB a minute with them off). `RUST_LOG` still overrides it.
#[cfg(not(target_os = "ios"))]
const LOG_LEVEL: bevy::log::Level = bevy::log::Level::INFO;
#[cfg(target_os = "ios")]
const LOG_LEVEL: bevy::log::Level = bevy::log::Level::WARN;

/// Per-target directives over [`LOG_LEVEL`]. On iOS, info stays on for what a session is read by:
/// the memory governor, the `WOW_IOS_PERF` meter, login progress and the probes.
#[cfg(not(target_os = "ios"))]
const LOG_FILTER: &str = "wgpu=error,naga=warn";
#[cfg(target_os = "ios")]
const LOG_FILTER: &str = "wgpu=error,naga=warn,benilla_world::memory_governor=info,\
benilla_world::ios_boot=info,\
benilla_app::glue_session::login=info,benilla_app::run_mode=info,benilla_app::probe_route=info,\
benilla_app::probe_chat=info,benilla_app::probe_crowd=info,benilla_app::probe_quest=info";

pub fn tuned_default_plugins(primary_window: Window) -> PluginGroupBuilder {
    // iOS: the whole screen. Bevy's default 1280x720 pt window overruns the iPad Air 2's 1024x768.
    #[cfg(target_os = "ios")]
    let primary_window = Window {
        mode: bevy::window::WindowMode::BorderlessFullscreen(
            bevy::window::MonitorSelection::Primary,
        ),
        prefers_status_bar_hidden: true,
        prefers_home_indicator_hidden: true,
        ..primary_window
    };
    let plugins = DefaultPlugins
        .set(WindowPlugin {
            primary_window: Some(primary_window),
            ..default()
        })
        // Deliberately no `AssetPlugin::file_path`: a baked source path resolves only on the build
        // machine. Every shader is embedded (`embedded://<crate>/shaders/…`), so no root is read.
        // Quiet wgpu/naga; the ring keeps the last stderr lines for the crash report (`log_ring`).
        .set(bevy::log::LogPlugin {
            level: LOG_LEVEL,
            filter: LOG_FILTER.into(),
            custom_layer: |_| Some(Box::new(crate::log_ring::LogRing)),
            ..default()
        })
        // Asset loads parse synchronously on the IO pool, and Bevy's default 4 threads saturate on
        // a dense teleport. Workers spawn at default QoS, behind any background build:
        // compute is user-interactive, IO and async compute user-initiated, and
        // `ThreadQosPlugin` promotes the render thread from inside.
        .set(TaskPoolPlugin {
            task_pool_options: TaskPoolOptions {
                io: bevy::app::TaskPoolThreadAssignmentPolicy {
                    min_threads: 2,
                    max_threads: 8,
                    percent: 0.5,
                    on_thread_spawn: Some(std::sync::Arc::new(|| {
                        thread_qos::promote_current_thread(thread_qos::QosClass::UserInitiated)
                    })),
                    on_thread_destroy: None,
                },
                async_compute: bevy::app::TaskPoolThreadAssignmentPolicy {
                    on_thread_spawn: Some(std::sync::Arc::new(|| {
                        thread_qos::promote_current_thread(thread_qos::QosClass::UserInitiated)
                    })),
                    ..TaskPoolOptions::default().async_compute
                },
                compute: bevy::app::TaskPoolThreadAssignmentPolicy {
                    on_thread_spawn: Some(std::sync::Arc::new(|| {
                        thread_qos::promote_current_thread(thread_qos::QosClass::UserInteractive)
                    })),
                    // `WOW_THREADS=1` serialises the frame's systems, a diagnostic: a defect that
                    // survives it is not a race between two systems.
                    max_threads: match std::env::var("WOW_THREADS").ok().as_deref() {
                        Some("1") => 1,
                        _ => TaskPoolOptions::default().compute.max_threads,
                    },
                    ..TaskPoolOptions::default().compute
                },
                ..default()
            },
        })
        // Sound is kira behind our own mixer; `bevy_audio` is off by feature (workspace
        // `Cargo.toml`). Kept though they look idle: gizmos (bowstring, fishing line), sprites (the
        // FrameXML quad pass), picking, TextPlugin (glue text), PostProcessPlugin (glow bloom) and
        // ScenePlugin (avian's collider backend reads `SceneSpawner`); the ForwardDecal family is
        // registered inside `PbrPlugin::build`, so it cannot be disabled on its own.
        //
        // M2/WMO/ADT load through our own `mpq://` loaders; there is no glTF.
        .disable::<bevy::gltf::GltfPlugin>()
        // No bevy AA: no Fxaa/TAA/SMAA/CAS component anywhere (MSAA is core render, unaffected).
        .disable::<bevy::anti_alias::AntiAliasPlugin>()
        // No gamepad input; 1.12's bindings are keyboard/mouse.
        .disable::<bevy::gilrs::GilrsPlugin>();
    // iOS: mesh instance data built on the CPU, and the rest of the A8X's settings
    // (`crate::ios_boot`).
    #[cfg(target_os = "ios")]
    let plugins = plugins
        .set(bevy::pbr::PbrPlugin {
            use_gpu_instance_buffer_builder: false,
            ..default()
        })
        .add(crate::ios_boot::IosBootPlugin);
    plugins
}
