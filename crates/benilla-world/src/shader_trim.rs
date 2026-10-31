//! Emptying the shader composer once the pipelines are warm: naga_oil keeps every shader import it
//! parsed, once per shader-def set, for the session (~26 MB on the iPad Air 2), though only a new
//! pipeline reads them. After [`IDLE_SECS`] with nothing queued or compiling, the composer is
//! emptied (`PipelineCache::trim_shader_composer`, the vendored `bevy_render`); a later pipeline
//! re-parses the imports it needs, a slower first compile for that one. Nothing drawn changes.
//! `$WOW_SHADER_TRIM=0` keeps the composer whole (a before/after measurement).

use std::time::{Duration, Instant};

use bevy::prelude::*;
use bevy::render::render_resource::PipelineCache;
use bevy::render::{Render, RenderApp, RenderSystems};

/// How long the pipeline cache must stay idle before the composer is emptied: longer than the
/// gaps between a load's pipeline bursts, so a world entry trims once, after it settles.
const IDLE_SECS: u64 = 20;

/// When the cache last compiled, and whether the composer has been emptied since.
struct Idle {
    since: Instant,
    trimmed: bool,
}

impl Default for Idle {
    fn default() -> Self {
        Self {
            since: Instant::now(),
            trimmed: false,
        }
    }
}

fn trim_when_idle(cache: Res<PipelineCache>, mut idle: Local<Idle>) {
    if cache.compiling() {
        *idle = Idle::default();
        return;
    }
    if idle.trimmed || idle.since.elapsed() < Duration::from_secs(IDLE_SECS) {
        return;
    }
    idle.trimmed = true;
    let modules = cache.trim_shader_composer();
    if modules > 0 {
        info!("shader trim: emptied the composer, {modules} parsed imports dropped");
    }
}

pub(crate) fn plugin(app: &mut App) {
    if std::env::var("WOW_SHADER_TRIM").is_ok_and(|v| v == "0") {
        return;
    }
    if let Some(render) = app.get_sub_app_mut(RenderApp) {
        render.add_systems(Render, trim_when_idle.in_set(RenderSystems::Cleanup));
    }
}
