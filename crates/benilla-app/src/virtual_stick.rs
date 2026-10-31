//! The movement stick, on iOS: a finger [`crate::touch_mouse`] hands over moves the character as
//! the movement keys do, and a ring and knob show where it is.
//!
//! - The stick's centre is where the finger landed; the knob follows the finger out to
//!   [`RADIUS`]. Past [`DEAD`] of the radius, each axis beyond [`AXIS`] holds its direction:
//!   up forward, down backward, left and right strafe, so a diagonal holds two.
//! - The directions go in as the bindings' own calls (`MoveForwardStart`/`Stop` and the strafes'),
//!   so the mover, the wire and the camera's follow read them as they read W.
//! - How far the knob is out picks the gait: inside [`WALK_BELOW`] of the radius the character
//!   walks, past [`RUN_ABOVE`] it runs, between them it keeps its gait. The switch is the walk
//!   toggle, `ToggleRun()` (`0x513d50`), the only gait 1.12 has: walk speed or run speed, no
//!   speed between, so the server sees a player pressing the walk key. Letting go of the stick
//!   after a walk toggles back to running.
//!
//! Deviation: benilla's own control. The reference has no touch input; its movement comes from
//! keys and mice alone.

use benilla_ui::script::{BindingInput, FiredInput, HeldInput};
use bevy::prelude::*;

/// The stick's zone: a finger landing on the left of this fraction of the window's width.
pub(crate) const ZONE_WIDTH: f32 = 0.4;

/// Logical px from the centre to the knob's furthest reach.
pub(crate) const RADIUS: f32 = 60.0;

/// The fraction of [`RADIUS`] inside which the stick holds nothing.
const DEAD: f32 = 0.25;

/// The fraction of [`RADIUS`] an axis must pass to hold its direction: sin 22.5°, so the eight
/// sectors are equal.
const AXIS: f32 = 0.38;

/// The fraction of [`RADIUS`] under which a running stick walks.
const WALK_BELOW: f32 = 0.55;

/// The fraction of [`RADIUS`] over which a walking stick runs; the gap keeps a finger on the
/// boundary from toggling every frame.
const RUN_ABOVE: f32 = 0.7;

/// The stick this frame, `(centre, finger)` in logical px; `None` with no stick finger down.
#[derive(Resource, Default, Clone, Copy, PartialEq)]
pub(crate) struct VirtualStick(pub(crate) Option<(Vec2, Vec2)>);

/// The four directions in bindings order: forward, backward, strafe left, strafe right.
const DIRECTIONS: [HeldInput; 4] = [
    HeldInput::MoveForward,
    HeldInput::MoveBackward,
    HeldInput::StrafeLeft,
    HeldInput::StrafeRight,
];

/// The directions a finger at `at` holds on a stick centred at `center`, in [`DIRECTIONS`] order.
/// Window y grows downward, so up is forward.
fn held(center: Vec2, at: Vec2) -> [bool; 4] {
    let v = ((at - center) / RADIUS).clamp_length_max(1.0);
    if v.length() < DEAD {
        return [false; 4];
    }
    [v.y < -AXIS, v.y > AXIS, v.x < -AXIS, v.x > AXIS]
}

/// The gait the stick asks for: `Some(true)` to walk, `Some(false)` to run, from the knob's reach
/// (`None` with no stick) and the gait it asked for last. Inside the dead zone it keeps the last.
fn gait(reach: Option<f32>, last: Option<bool>) -> Option<bool> {
    let reach = reach?;
    if reach < DEAD {
        return last;
    }
    Some(match last {
        Some(true) => reach <= RUN_ABOVE,
        _ => reach < WALK_BELOW,
    })
}

/// Whether to press the walk toggle: the stick's gait changed, or it let go after a walk, and the
/// player's walk mode is not already what that asks for.
fn toggle_gait(want: Option<bool>, last: Option<bool>, walking: bool) -> bool {
    let target = match (want, last) {
        (Some(w), l) if l != Some(w) => w,
        (None, Some(true)) => false,
        _ => return false,
    };
    target != walking
}

/// The stick's directions into the bindings, after the frame's binding calls: a Start each frame a
/// direction is held, which is no new edge on a held bit and restores one the loading cover's bulk
/// clear dropped, and a Stop when it lets go; and the walk toggle when the gait changes.
pub(crate) fn hold_directions(
    stick: Option<Res<VirtualStick>>,
    player: Option<Res<crate::player::Player>>,
    mut binds: ResMut<crate::bindings::BindingsState>,
    mut last: Local<[bool; 4]>,
    mut last_gait: Local<Option<bool>>,
) {
    let stick = stick.and_then(|s| s.0);
    let now = stick.map_or([false; 4], |(center, at)| held(center, at));
    if now != *last {
        info!("virtual stick: forward/back/left/right {now:?}");
    }
    let reach = stick.map(|(center, at)| ((at - center) / RADIUS).length().min(1.0));
    let want = gait(reach, *last_gait);
    let mut calls = Vec::new();
    if let Some(player) = player {
        if toggle_gait(want, *last_gait, player.walking()) {
            info!("virtual stick: walk {}", want == Some(true));
            calls.push(BindingInput::Fire(FiredInput::ToggleRun));
        }
    }
    *last_gait = want;
    for (i, input) in DIRECTIONS.into_iter().enumerate() {
        if now[i] {
            calls.push(BindingInput::Start(input));
        } else if last[i] {
            calls.push(BindingInput::Stop(input));
        }
    }
    *last = now;
    if !calls.is_empty() {
        binds.apply_device_input(calls);
    }
}

/// The ring and the knob.
#[derive(Component)]
enum StickPart {
    Ring,
    Knob,
}

/// Above the interface, below the loading cover (1000).
const STICK_Z: i32 = 900;

/// The knob's diameter, as a fraction of the ring's.
const KNOB_SCALE: f32 = 0.45;

/// The ring and the knob, spawned once and parked hidden.
fn spawn_stick(mut commands: Commands) {
    for (part, diameter, color) in [
        (
            StickPart::Ring,
            2.0 * RADIUS,
            Color::srgba(1.0, 1.0, 1.0, 0.12),
        ),
        (
            StickPart::Knob,
            2.0 * RADIUS * KNOB_SCALE,
            Color::srgba(1.0, 1.0, 1.0, 0.35),
        ),
    ] {
        commands.spawn((
            part,
            Node {
                position_type: PositionType::Absolute,
                width: Val::Px(diameter),
                height: Val::Px(diameter),
                border: UiRect::all(Val::Px(2.0)),
                border_radius: BorderRadius::MAX,
                ..default()
            },
            BackgroundColor(color),
            BorderColor::all(Color::srgba(1.0, 1.0, 1.0, 0.4)),
            GlobalZIndex(STICK_Z),
            Visibility::Hidden,
        ));
    }
}

/// The ring at the stick's centre and the knob at the finger, clamped to the ring.
fn place_stick(
    stick: Res<VirtualStick>,
    mut parts: Query<(&StickPart, &mut Node, &mut Visibility)>,
) {
    if !stick.is_changed() {
        return;
    }
    for (part, mut node, mut vis) in &mut parts {
        let Some((center, at)) = stick.0 else {
            *vis = Visibility::Hidden;
            continue;
        };
        let (middle, diameter) = match part {
            StickPart::Ring => (center, 2.0 * RADIUS),
            StickPart::Knob => (
                center + (at - center).clamp_length_max(RADIUS),
                2.0 * RADIUS * KNOB_SCALE,
            ),
        };
        node.left = Val::Px(middle.x - diameter / 2.0);
        node.top = Val::Px(middle.y - diameter / 2.0);
        *vis = Visibility::Visible;
    }
}

/// Draws the stick on iOS; [`hold_directions`] runs in the bindings' tail on every platform and
/// holds nothing without a stick.
pub(crate) struct VirtualStickPlugin;

impl Plugin for VirtualStickPlugin {
    fn build(&self, app: &mut App) {
        if cfg!(target_os = "ios") {
            app.add_systems(Startup, spawn_stick)
                .add_systems(PostUpdate, place_stick);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const C: Vec2 = Vec2::new(200.0, 500.0);

    #[test]
    fn a_short_push_walks_and_a_long_push_runs() {
        assert_eq!(gait(Some(0.4), None), Some(true));
        assert_eq!(gait(Some(0.9), None), Some(false));
        assert_eq!(gait(None, Some(true)), None);
    }

    #[test]
    fn between_the_thresholds_the_gait_holds() {
        // Walking: on past the walk edge, still walking until RUN_ABOVE.
        assert_eq!(gait(Some(0.65), Some(true)), Some(true));
        assert_eq!(gait(Some(0.75), Some(true)), Some(false));
        // Running: back inside RUN_ABOVE, still running until WALK_BELOW.
        assert_eq!(gait(Some(0.65), Some(false)), Some(false));
        assert_eq!(gait(Some(0.5), Some(false)), Some(true));
        // The dead zone keeps whatever the stick last asked for.
        assert_eq!(gait(Some(0.1), Some(true)), Some(true));
        assert_eq!(gait(Some(0.1), None), None);
    }

    #[test]
    fn the_toggle_fires_on_a_change_of_gait_only_when_the_mode_differs() {
        assert!(
            toggle_gait(Some(true), None, false),
            "a first walk from a run"
        );
        assert!(
            !toggle_gait(Some(true), Some(true), false),
            "no edge, no press"
        );
        assert!(!toggle_gait(Some(true), None, true), "already walking");
        assert!(toggle_gait(Some(false), Some(true), true), "out to a run");
        assert!(
            toggle_gait(None, Some(true), true),
            "let go after a walk: back to running"
        );
        assert!(
            !toggle_gait(None, Some(false), false),
            "let go after a run: nothing"
        );
        assert!(
            !toggle_gait(None, None, true),
            "no stick, a walk set elsewhere stays"
        );
    }

    #[test]
    fn inside_the_dead_zone_nothing_is_held() {
        assert_eq!(held(C, C + Vec2::new(10.0, -10.0)), [false; 4]);
    }

    #[test]
    fn up_is_forward_and_down_is_backward() {
        assert_eq!(
            held(C, C + Vec2::new(0.0, -50.0)),
            [true, false, false, false]
        );
        assert_eq!(
            held(C, C + Vec2::new(0.0, 50.0)),
            [false, true, false, false]
        );
    }

    #[test]
    fn sideways_strafes_and_a_diagonal_holds_two() {
        assert_eq!(
            held(C, C + Vec2::new(-50.0, 0.0)),
            [false, false, true, false]
        );
        assert_eq!(
            held(C, C + Vec2::new(40.0, -40.0)),
            [true, false, false, true]
        );
    }

    #[test]
    fn a_finger_past_the_ring_reads_as_at_its_edge() {
        assert_eq!(
            held(C, C + Vec2::new(0.0, -500.0)),
            [true, false, false, false]
        );
        // Far right and a little up: the clamped vector's y stays under the axis threshold.
        assert_eq!(
            held(C, C + Vec2::new(500.0, -60.0)),
            [false, false, false, true]
        );
    }
}
