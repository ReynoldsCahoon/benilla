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
//!
//! The target is `dynamicRenderScaleFPS`, held to the display's own rate. Its "0", the iOS
//! default, is the device's ([`auto_fps`]): 30 on the A8-class GPUs (the iPad Air 2, the iPad mini
//! 4), which cannot hold 60, and 60 on anything newer. On iOS the same target paces the
//! presentation ([`benilla_world::frame_pace::hold_frames_for`]), with or without the controller.

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
pub(crate) struct DynamicScaleConfig {
    /// `dynamicRenderScale`.
    on: bool,
    /// The frame rate held: [`Self::asked_fps`] resolved by [`resolve_target`].
    target_fps: f32,
    /// `dynamicRenderScaleFPS` as set; [`AUTO`] is the device's own rate.
    asked_fps: f32,
    /// `dynamicRenderScaleMin`: the lowest factor it may reach.
    floor: f32,
}

impl Default for DynamicScaleConfig {
    /// The CVar table's defaults (`cvars::table`).
    fn default() -> Self {
        Self {
            on: cfg!(target_os = "ios"),
            target_fps: 30.0,
            asked_fps: if cfg!(target_os = "ios") { AUTO } else { 30.0 },
            floor: 0.5,
        }
    }
}

/// `dynamicRenderScaleFPS`'s "the device's own rate".
const AUTO: f32 = 0.0;

/// The device's own rate: 30 on an Apple A8-class GPU (the adapter names "Apple A8 GPU" and
/// "Apple A8X GPU"), 60 otherwise; 30 until the adapter is known.
fn auto_fps(adapter_name: Option<&str>) -> f32 {
    match adapter_name {
        None => 30.0,
        Some(name) if name.contains(" A8") => 30.0,
        Some(_) => 60.0,
    }
}

/// The rate held: the asked one or the device's, never past the fastest display's refresh
/// (`None` refresh rates read as 60 Hz, as [`crate::video::refresh_period`] does).
fn target_fps(
    asked: f32,
    adapter_name: Option<&str>,
    refresh_millihertz: impl IntoIterator<Item = Option<u32>>,
) -> f32 {
    let display = 1.0 / crate::video::refresh_period(refresh_millihertz).as_secs_f32();
    let fps = if asked == AUTO {
        auto_fps(adapter_name)
    } else {
        asked
    };
    fps.min(display.round())
}

/// Resolves the target as the setting, the adapter or the displays change, and paces the
/// presentation to it on iOS.
fn resolve_target(
    mut config: ResMut<DynamicScaleConfig>,
    adapter: Option<Res<bevy::render::renderer::RenderAdapterInfo>>,
    monitors: Query<&bevy::window::Monitor>,
    mut held: Local<Option<f32>>,
) {
    let fps = target_fps(
        config.asked_fps,
        adapter.as_deref().map(|a| a.name.as_str()),
        monitors.iter().map(|m| m.refresh_rate_millihertz),
    );
    if *held == Some(fps) {
        return;
    }
    *held = Some(fps);
    config.target_fps = fps;
    benilla_world::frame_pace::hold_frames_for(fps);
    info!(
        "frame-rate target {fps} fps (dynamicRenderScaleFPS {})",
        config.asked_fps
    );
}

impl DynamicScaleConfig {
    /// `dynamicRenderScale`, `dynamicRenderScaleFPS` as set, and `dynamicRenderScaleMin`.
    #[cfg(test)]
    pub(crate) fn settings(&self) -> (bool, f32, f32) {
        (self.on, self.asked_fps, self.floor)
    }

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

/// The CVars' change callback, run by [`crate::cvars::callbacks::CvarCallbacks`] for every write:
/// a Lua verb's (the options rows, `/console`), the boot load's. (An observer of
/// [`crate::cvars::CvarChanged`] alone saw only the boot load, so a write took effect at the next
/// launch.)
pub(crate) fn on_cvar(ev: &crate::cvars::CvarChanged, mut config: Mut<'_, DynamicScaleConfig>) {
    if ev.is("dynamicRenderScale") {
        config.on = ev.flag();
    } else if ev.is("dynamicRenderScaleFPS") {
        let asked = ev.num();
        config.asked_fps = if asked <= 0.0 {
            AUTO
        } else {
            asked.clamp(10.0, 240.0)
        };
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

impl Controller {
    /// One frame of `dt` seconds against a `budget_ms` frame: steps down a level (to `deepest` at
    /// most) after a missed second, when `may_descend`, and probes up one after
    /// [`PROBE_AFTER_S`] on budget; see the module docs. Whether the level moved.
    fn tick(
        &mut self,
        dt: f32,
        budget_ms: f32,
        deepest: usize,
        may_descend: bool,
        levels: &[f32],
        what: &str,
    ) -> bool {
        if self.probe_wait_s == 0.0 {
            self.probe_wait_s = PROBE_AFTER_S;
        }
        // A raised floor pulls the scale up to it at once.
        if self.level > deepest {
            self.level = deepest;
            return true;
        }
        if self.settle_s > 0.0 {
            self.settle_s -= dt;
            self.window_s = 0.0;
            self.window_frames = 0;
            return false;
        }
        self.window_s += dt;
        self.window_frames += 1;
        if self.window_s < 1.0 {
            return false;
        }
        let mean_ms = self.window_s * 1000.0 / self.window_frames as f32;
        let elapsed = self.window_s;
        self.window_s = 0.0;
        self.window_frames = 0;

        let from = levels[self.level];
        if mean_ms > budget_ms * MISS {
            if self.probing {
                self.probe_wait_s = (self.probe_wait_s * 2.0).min(PROBE_MAX_S);
            }
            self.probing = false;
            self.on_budget_s = 0.0;
            if may_descend && self.level < deepest {
                self.level += 1;
                self.settle_s = SETTLE_S;
                info!(
                    "{what} {from:.2} -> {:.2}: {mean_ms:.1} ms frames over a {budget_ms:.1} ms budget",
                    levels[self.level]
                );
                return true;
            }
        } else {
            if self.probing {
                // The probe held: the next one waits the base time.
                self.probing = false;
                self.probe_wait_s = PROBE_AFTER_S;
            }
            self.on_budget_s += elapsed;
            if self.level > 0 && self.on_budget_s >= self.probe_wait_s {
                self.level -= 1;
                self.on_budget_s = 0.0;
                self.probing = true;
                self.settle_s = SETTLE_S;
                info!(
                    "{what} {from:.2} -> {:.2}: probing up, {mean_ms:.1} ms frames on a \
                     {budget_ms:.1} ms budget",
                    levels[self.level]
                );
                return true;
            }
        }
        false
    }
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
    state.tick(
        time.delta_secs(),
        config.budget_ms(),
        config.deepest(),
        true,
        &LEVELS,
        "dynamic render scale",
    );
    apply(&mut scale, state.level);
}

/// The interface's render scales, `uiRenderScale`'s Dynamic steps through, from native down.
const UI_LEVELS: [f32; 3] = [1.0, 0.75, 0.5];

/// `uiRenderScale`: a fixed fraction of the window the interface renders at, or Dynamic.
#[derive(Resource, Clone, Copy, PartialEq, Debug)]
pub(crate) enum UiScaleChoice {
    Fixed(f32),
    Dynamic,
}

impl Default for UiScaleChoice {
    /// Native, or `$WOW_UI_RENDER_SCALE` for the session (`0` is Dynamic).
    fn default() -> Self {
        std::env::var("WOW_UI_RENDER_SCALE")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
            .map_or(Self::Fixed(1.0), Self::from_value)
    }
}

impl UiScaleChoice {
    /// A CVar value: `0` (or below) is Dynamic, anything else a fixed scale in 0.5-1.0.
    pub(crate) fn from_value(v: f32) -> Self {
        if v <= 0.0 {
            Self::Dynamic
        } else {
            Self::Fixed(v.clamp(0.5, 1.0))
        }
    }
}

/// The `uiRenderScale` callback, beside [`on_cvar`]; `$WOW_UI_RENDER_SCALE` owns the session.
pub(crate) fn on_ui_scale_cvar(ev: &crate::cvars::CvarChanged, mut choice: Mut<'_, UiScaleChoice>) {
    if ev.is("uiRenderScale") && std::env::var_os("WOW_UI_RENDER_SCALE").is_none() {
        let want = UiScaleChoice::from_value(ev.num());
        if *choice != want {
            *choice = want;
        }
    }
}

/// Whether the 3D's dynamic scale has nothing left to give: off, or at its floor.
fn world_at_floor(config: &DynamicScaleConfig, world_scale: &DynamicScale) -> bool {
    !config.on || world_scale.0 <= LEVELS[config.deepest()] + 1e-4
}

/// Animation Detail's Dynamic ([`crate::creature_anim::AnimDetail`]): Quality until missed
/// seconds, then Performance ([`crate::creature_anim::AnimDetailStepped`]), with the 3D's stepping
/// against the same target; it steps down only once the 3D's scale is off or at its floor, and
/// climbs back on its own probes.
fn hold_anim_budget(
    time: Res<Time<bevy::time::Real>>,
    config: Res<DynamicScaleConfig>,
    detail: Option<Res<crate::creature_anim::AnimDetail>>,
    world_scale: Res<DynamicScale>,
    stepped: Option<ResMut<crate::creature_anim::AnimDetailStepped>>,
    mut state: Local<Controller>,
) {
    let Some(mut stepped) = stepped else {
        return;
    };
    let want = if detail.is_some_and(|d| *d == crate::creature_anim::AnimDetail::Dynamic) {
        state.tick(
            time.delta_secs(),
            config.budget_ms(),
            ANIM_LEVELS.len() - 1,
            // Not in the frame the world itself stepped: one missed second, one step.
            world_at_floor(&config, &world_scale) && !world_scale.is_changed(),
            &ANIM_LEVELS,
            "dynamic animation detail",
        );
        state.level > 0
    } else {
        *state = Controller::default();
        false
    };
    if stepped.0 != want {
        stepped.0 = want;
    }
}

/// Animation Detail's two steps for the controller's log: Quality (1) and Performance (0.5).
const ANIM_LEVELS: [f32; 2] = [1.0, 0.5];

/// The interface's scale ([`crate::ui_pass::UiRenderScale`]): the fixed choice, or under Dynamic
/// the same stepping as the 3D's against the same target, 100%, 75%, 50%. Dynamic steps down only
/// once the 3D's dynamic scale is off or at its floor and a Dynamic Animation Detail has stepped
/// to Performance, so a missed second lowers one of them, the world first, then animation; it
/// steps back up on its own probes.
#[allow(clippy::too_many_arguments)] // one Bevy system's inputs
fn hold_ui_budget(
    time: Res<Time<bevy::time::Real>>,
    config: Res<DynamicScaleConfig>,
    choice: Res<UiScaleChoice>,
    world_scale: Res<DynamicScale>,
    anim: Option<Res<crate::creature_anim::AnimDetail>>,
    anim_stepped: Option<Res<crate::creature_anim::AnimDetailStepped>>,
    mut ui: ResMut<crate::ui_pass::UiRenderScale>,
    mut state: Local<Controller>,
) {
    // Animation has stepped, and not in this frame: one missed second, one step.
    let anim_done = !anim.is_some_and(|d| *d == crate::creature_anim::AnimDetail::Dynamic)
        || anim_stepped.is_some_and(|s| s.0 && !s.is_changed());
    let want = match *choice {
        UiScaleChoice::Fixed(s) => {
            *state = Controller::default();
            s
        }
        UiScaleChoice::Dynamic => {
            state.tick(
                time.delta_secs(),
                config.budget_ms(),
                UI_LEVELS.len() - 1,
                world_at_floor(&config, &world_scale) && !world_scale.is_changed() && anim_done,
                &UI_LEVELS,
                "dynamic interface scale",
            );
            UI_LEVELS[state.level]
        }
    };
    if ui.0 != want {
        ui.0 = want;
    }
}

/// Writes the level's factor, only when it moves: a write rebuilds nothing, but a moved factor
/// rebuilds the 3D's targets ([`crate::world_backdrop`]). Through the `ResMut`, so an unmoved
/// factor leaves the resource unchanged (the later scalers read its change as "the world just
/// stepped").
fn apply(scale: &mut ResMut<DynamicScale>, level: usize) {
    let factor = LEVELS[level];
    if scale.0 != factor {
        scale.0 = factor;
    }
}

/// Registers the controller and its CVars' callback.
pub(crate) struct DynamicScalePlugin;

impl Plugin for DynamicScalePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<DynamicScaleConfig>()
            .init_resource::<DynamicScale>()
            .init_resource::<UiScaleChoice>()
            .init_resource::<crate::ui_pass::UiRenderScale>()
            // After the frame's world stages, and before the render size reads the factor.
            .add_systems(
                Update,
                (
                    resolve_target,
                    hold_budget,
                    hold_anim_budget,
                    hold_ui_budget,
                )
                    .chain()
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
                asked_fps: 30.0,
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

    #[test]
    fn the_target_is_the_device_s_own_rate_held_to_the_display() {
        let hz = |hz: u32| [Some(hz * 1000)];
        // Automatic: the A8-class GPUs hold 30, anything newer 60, and 30 before the adapter.
        assert_eq!(target_fps(AUTO, Some("Apple A8X GPU"), hz(60)), 30.0);
        assert_eq!(target_fps(AUTO, Some("Apple A8 GPU"), hz(60)), 30.0);
        assert_eq!(target_fps(AUTO, Some("Apple A12Z GPU"), hz(120)), 60.0);
        assert_eq!(target_fps(AUTO, Some("Apple M2 GPU"), hz(120)), 60.0);
        assert_eq!(target_fps(AUTO, None, hz(60)), 30.0);
        // Display Maximum ("120") is the screen's rate: 120 on ProMotion, 60 elsewhere.
        assert_eq!(target_fps(120.0, Some("Apple M2 GPU"), hz(120)), 120.0);
        assert_eq!(target_fps(120.0, Some("Apple A8X GPU"), hz(60)), 60.0);
        // An unknown refresh reads as 60 Hz; a set rate under it stands.
        assert_eq!(target_fps(120.0, None, [None]), 60.0);
        assert_eq!(target_fps(30.0, Some("Apple M2 GPU"), hz(120)), 30.0);
    }

    fn ui_app(choice: UiScaleChoice) -> App {
        let mut app = App::new();
        app.init_resource::<Time<bevy::time::Real>>()
            .insert_resource(DynamicScaleConfig {
                on: true,
                target_fps: 30.0,
                asked_fps: 30.0,
                floor: 0.5,
            })
            .init_resource::<DynamicScale>()
            .insert_resource(choice)
            .init_resource::<crate::ui_pass::UiRenderScale>()
            .add_systems(Update, (hold_budget, hold_ui_budget).chain());
        app
    }

    fn ui(app: &App) -> f32 {
        app.world().resource::<crate::ui_pass::UiRenderScale>().0
    }

    #[test]
    fn dynamic_interface_steps_down_after_the_world_and_back_up_on_budget() {
        let mut app = ui_app(UiScaleChoice::Dynamic);
        // Missed seconds: the world steps first, the interface holds at 100% until it floors.
        let mut world_floored_at = None;
        for s in 0..40 {
            second(&mut app, 50.0);
            if factor(&app) > 0.5 + 1e-4 {
                assert_eq!(ui(&app), 1.0, "second {s}: the world still has steps");
            } else if world_floored_at.is_none() {
                world_floored_at = Some(s);
            }
        }
        assert!(world_floored_at.is_some(), "the world reached its floor");
        assert_eq!(ui(&app), 0.5, "then the interface stepped 75% and 50%");
        // On budget: the interface climbs back to native.
        for _ in 0..120 {
            second(&mut app, 33.3);
        }
        assert_eq!(ui(&app), 1.0);
    }

    #[test]
    fn a_fixed_interface_scale_never_moves() {
        let mut app = ui_app(UiScaleChoice::Fixed(0.75));
        for _ in 0..30 {
            second(&mut app, 50.0);
        }
        assert_eq!(ui(&app), 0.75);
        assert_eq!(UiScaleChoice::from_value(0.0), UiScaleChoice::Dynamic);
        assert_eq!(UiScaleChoice::from_value(0.3), UiScaleChoice::Fixed(0.5));
    }

    #[test]
    fn dynamic_animation_steps_after_the_world_and_before_the_interface() {
        use crate::creature_anim::{AnimDetail, AnimDetailStepped};
        let mut app = App::new();
        app.init_resource::<Time<bevy::time::Real>>()
            .insert_resource(DynamicScaleConfig {
                on: true,
                target_fps: 30.0,
                asked_fps: 30.0,
                floor: 0.5,
            })
            .init_resource::<DynamicScale>()
            .insert_resource(UiScaleChoice::Dynamic)
            .init_resource::<crate::ui_pass::UiRenderScale>()
            .insert_resource(AnimDetail::Dynamic)
            .init_resource::<AnimDetailStepped>()
            .add_systems(
                Update,
                (hold_budget, hold_anim_budget, hold_ui_budget).chain(),
            );
        let stepped = |app: &App| app.world().resource::<AnimDetailStepped>().0;
        let (mut anim_at, mut ui_at) = (None, None);
        for s in 0..60 {
            second(&mut app, 50.0);
            if stepped(&app) && anim_at.is_none() {
                assert!(
                    factor(&app) <= 0.5 + 1e-4,
                    "animation waits for the world's floor"
                );
                anim_at = Some(s);
            }
            if ui(&app) < 1.0 && ui_at.is_none() {
                assert!(stepped(&app), "the interface waits for animation");
                ui_at = Some(s);
            }
        }
        assert!(
            anim_at.is_some() && ui_at.is_some(),
            "{anim_at:?} {ui_at:?}"
        );
        assert!(anim_at < ui_at, "animation first: {anim_at:?} {ui_at:?}");
        for _ in 0..200 {
            second(&mut app, 33.3);
        }
        assert!(!stepped(&app), "back to Quality on budget");
        assert_eq!(ui(&app), 1.0);
    }
}
