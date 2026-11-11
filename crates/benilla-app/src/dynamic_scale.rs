//! Dynamic render scale (`dynamicRenderScale`, benilla's own): holds a frame budget by moving
//! [`DynamicScale`], the factor on the player's `renderScale` that the 3D renders at, while the
//! interface stays native ([`crate::world_backdrop`]).
//!
//! The signal is the wall frame, averaged over a second. Under vsync a frame lands on whole
//! refresh intervals (16.7, 33.3, 50 ms at 60 Hz), so a frame that meets the budget says nothing of
//! how much it has to spare: stepping down follows a miss directly, but stepping up is a probe. After
//! [`PROBE_AFTER_S`] seconds on budget the scale climbs one step; a probe that misses steps back and
//! doubles the wait before the next, up to [`PROBE_MAX_S`]. Every change rebuilds the 3D's targets,
//! so a [`SETTLE_S`] settle follows it before frames count again.
//!
//! Built for the iPad Air 2, whose GPU cost at the login screen follows the 3D's pixel count (163
//! ms a frame at 2048x1536, 55 ms at a quarter of the pixels): on iOS it is on by default.

use bevy::prelude::*;

use crate::world_backdrop::DynamicScale;

/// The factors the scale steps through, from full down; the floor setting trims the end.
const LEVELS: [f32; 7] = [1.0, 0.85, 0.7, 0.6, 0.5, 0.42, 0.35];

/// A second's mean frame over the budget by more than this misses it.
const MISS: f32 = 1.15;

/// Seconds on budget before the first probe upward.
const PROBE_AFTER_S: f32 = 8.0;

/// The longest wait between probes, after repeated misses.
const PROBE_MAX_S: f32 = 120.0;

/// Seconds after a change before frames count: the targets rebuild and a frame or two runs long.
const SETTLE_S: f32 = 1.5;

/// The three CVars, as the controller reads them.
#[derive(Resource, Clone, Copy, PartialEq, Debug)]
struct DynamicScaleConfig {
    /// `dynamicRenderScale`.
    on: bool,
    /// `dynamicRenderScaleFPS`: the frame rate held.
    target_fps: f32,
    /// `dynamicRenderScaleMin`: the lowest factor it may reach.
    floor: f32,
}

impl Default for DynamicScaleConfig {
    /// The CVar table's defaults (`cvars::table`).
    fn default() -> Self {
        Self {
            on: cfg!(target_os = "ios"),
            target_fps: 30.0,
            floor: 0.5,
        }
    }
}

impl DynamicScaleConfig {
    /// The deepest index into [`LEVELS`] the floor allows.
    fn deepest(&self) -> usize {
        LEVELS
            .iter()
            .rposition(|&f| f >= self.floor - 1e-4)
            .unwrap_or(0)
    }

    fn budget_ms(&self) -> f32 {
        1000.0 / self.target_fps.max(1.0)
    }
}

/// The CVars' change callback.
fn on_cvar(ev: On<crate::cvars::CvarChanged>, mut config: ResMut<DynamicScaleConfig>) {
    if ev.is("dynamicRenderScale") {
        config.on = ev.flag();
    } else if ev.is("dynamicRenderScaleFPS") {
        config.target_fps = ev.num().clamp(10.0, 240.0);
    } else if ev.is("dynamicRenderScaleMin") {
        config.floor = ev.num().clamp(0.25, 1.0);
    }
}

/// The controller's running state.
#[derive(Default)]
struct Controller {
    /// Index into [`LEVELS`].
    level: usize,
    /// The window being averaged: seconds and frames.
    window_s: f32,
    window_frames: u32,
    /// Seconds left before frames count again.
    settle_s: f32,
    /// Seconds on budget since the last change or miss.
    on_budget_s: f32,
    /// The wait before the next probe upward; doubles on a failed probe.
    probe_wait_s: f32,
    /// The last change was a probe upward, not yet judged.
    probing: bool,
}

/// Holds the budget: see the module docs.
fn hold_budget(
    time: Res<Time<bevy::time::Real>>,
    config: Res<DynamicScaleConfig>,
    mut scale: ResMut<DynamicScale>,
    mut state: Local<Controller>,
) {
    if !config.on {
        if scale.0 != 1.0 {
            scale.0 = 1.0;
        }
        *state = Controller::default();
        return;
    }
    if state.probe_wait_s == 0.0 {
        state.probe_wait_s = PROBE_AFTER_S;
    }
    // A raised floor pulls the scale up to it at once.
    let deepest = config.deepest();
    if state.level > deepest {
        state.level = deepest;
    }

    let dt = time.delta_secs();
    if state.settle_s > 0.0 {
        state.settle_s -= dt;
        state.window_s = 0.0;
        state.window_frames = 0;
        apply(&mut scale, state.level);
        return;
    }
    state.window_s += dt;
    state.window_frames += 1;
    if state.window_s < 1.0 {
        apply(&mut scale, state.level);
        return;
    }
    let mean_ms = state.window_s * 1000.0 / state.window_frames as f32;
    let budget = config.budget_ms();
    let elapsed = state.window_s;
    state.window_s = 0.0;
    state.window_frames = 0;

    let from = LEVELS[state.level];
    if mean_ms > budget * MISS {
        if state.probing {
            state.probe_wait_s = (state.probe_wait_s * 2.0).min(PROBE_MAX_S);
        }
        state.probing = false;
        state.on_budget_s = 0.0;
        if state.level < deepest {
            state.level += 1;
            state.settle_s = SETTLE_S;
            info!(
                "dynamic render scale {from:.2} -> {:.2}: {mean_ms:.1} ms frames over a \
                 {budget:.1} ms budget",
                LEVELS[state.level]
            );
        }
    } else {
        if state.probing {
            // The probe held: the next one waits the base time.
            state.probing = false;
            state.probe_wait_s = PROBE_AFTER_S;
        }
        state.on_budget_s += elapsed;
        if state.level > 0 && state.on_budget_s >= state.probe_wait_s {
            state.level -= 1;
            state.on_budget_s = 0.0;
            state.probing = true;
            state.settle_s = SETTLE_S;
            info!(
                "dynamic render scale {from:.2} -> {:.2}: probing up, {mean_ms:.1} ms frames on a \
                 {budget:.1} ms budget",
                LEVELS[state.level]
            );
        }
    }
    apply(&mut scale, state.level);
}

/// Writes the level's factor, only when it moves: a write rebuilds nothing, but a moved factor
/// rebuilds the 3D's targets ([`crate::world_backdrop`]).
fn apply(scale: &mut DynamicScale, level: usize) {
    let factor = LEVELS[level];
    if scale.0 != factor {
        scale.0 = factor;
    }
}

/// Registers the controller and its CVars' callback.
pub(crate) struct DynamicScalePlugin;

impl Plugin for DynamicScalePlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(on_cvar)
            .init_resource::<DynamicScaleConfig>()
            .init_resource::<DynamicScale>()
            // After the frame's world stages, and before the render size reads the factor.
            .add_systems(
                Update,
                hold_budget
                    .after(benilla_world::schedule::WorldStage::Present)
                    .before(crate::world_backdrop::track_render_size),
            );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One controller second at a constant frame time, `frames` frames long.
    fn second(app: &mut App, frame_ms: f32) {
        let frames = (1000.0 / frame_ms).ceil() as u32;
        for _ in 0..frames {
            app.world_mut()
                .resource_mut::<Time<bevy::time::Real>>()
                .advance_by(std::time::Duration::from_secs_f32(frame_ms / 1000.0));
            app.update();
        }
    }

    fn app(on: bool) -> App {
        let mut app = App::new();
        app.init_resource::<Time<bevy::time::Real>>()
            .insert_resource(DynamicScaleConfig {
                on,
                target_fps: 30.0,
                floor: 0.5,
            })
            .init_resource::<DynamicScale>()
            .add_systems(Update, hold_budget);
        app
    }

    fn factor(app: &App) -> f32 {
        app.world().resource::<DynamicScale>().0
    }

    #[test]
    fn off_holds_full_scale_whatever_the_frames() {
        let mut app = app(false);
        for _ in 0..5 {
            second(&mut app, 160.0);
        }
        assert_eq!(factor(&app), 1.0);
    }

    #[test]
    fn missed_budgets_step_down_to_the_floor_and_stop() {
        let mut app = app(true);
        for _ in 0..30 {
            second(&mut app, 160.0);
        }
        assert_eq!(
            factor(&app),
            0.5,
            "0.5 is the floor; 0.42 and below are trimmed"
        );
    }

    /// Frames of `frame_ms` until the factor is `want`, up to `max_s` seconds; the seconds taken.
    fn until(app: &mut App, frame_ms: f32, want: f32, max_s: f32) -> Option<f32> {
        let mut t = 0.0;
        while t < max_s {
            app.world_mut()
                .resource_mut::<Time<bevy::time::Real>>()
                .advance_by(std::time::Duration::from_secs_f32(frame_ms / 1000.0));
            app.update();
            t += frame_ms / 1000.0;
            if factor(app) == want {
                return Some(t);
            }
        }
        None
    }

    #[test]
    fn frames_on_budget_probe_up_and_a_failed_probe_waits_twice_as_long() {
        let mut app = app(true);
        // A missed second steps down at once.
        assert!(until(&mut app, 50.0, 0.85, 1.1).is_some());
        // On budget, a probe up after the settle and PROBE_AFTER_S.
        let climbed = until(&mut app, 33.3, 1.0, 20.0).expect("the probe climbed");
        assert!(climbed >= PROBE_AFTER_S, "{climbed} s");
        // The probe misses its first judged second: back down.
        assert!(until(&mut app, 50.0, 0.85, SETTLE_S + 1.1).is_some());
        // The next probe waits twice as long.
        let again = until(&mut app, 33.3, 1.0, 40.0).expect("probed again");
        assert!(again >= 2.0 * PROBE_AFTER_S, "{again} s");
    }

    #[test]
    fn the_floor_keeps_only_the_levels_at_or_above_it() {
        let deepest = |floor| {
            LEVELS[DynamicScaleConfig {
                floor,
                ..DynamicScaleConfig::default()
            }
            .deepest()]
        };
        assert_eq!(deepest(0.5), 0.5);
        assert_eq!(deepest(0.35), 0.35);
        assert_eq!(deepest(0.25), 0.35);
        assert_eq!(deepest(1.0), 1.0);
    }
}
