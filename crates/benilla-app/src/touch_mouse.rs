//! Touch as the mouse, on iOS, where the client runs with no keyboard or mouse attached: the
//! touches become the mouse benilla already reads (the window's cursor, the button state, motion
//! and the wheel), written before Bevy's input pass each frame.
//!
//! - One finger moves the cursor. A tap is a left click; a finger that moves past [`SLOP_PX`] or
//!   stays down [`DWELL_S`] holds the left button from where it landed, so it drags.
//! - Two fingers put the cursor at their midpoint and hold the right button: a two-finger drag is
//!   a right-drag (the camera), a two-finger tap a right-click.
//! - Two fingers pinching turn the wheel (camera zoom), one line per [`PINCH_PX_PER_LINE`].
//! - In the world, a finger still pending a frame after it landed on the left of the screen with
//!   no interface frame under it becomes the movement stick ([`crate::virtual_stick`]) and leaves
//!   the mouse; while it is held another finger's drag is a right-drag, mouselook, so the
//!   character turns with the view. A stick finger that never moved is a tap after all.
//!
//! Each action is written twice, as bevy_winit writes a real mouse's: as its typed message (the
//! button plane, motion and wheel the game reads) and into the window's ordered event stream
//! ([`WindowEvent`]), which the interface reads its clicks, moves and wheel turns from
//! ([`crate::ui_script::collect_input_records`]).
//!
//! A click's release lands a frame after its press, so the interface sees the button down for a
//! frame. The cursor is written without marking the window changed: bevy_winit would otherwise try
//! to warp the system cursor, which iOS refuses, and log the refusal on every move.
//!
//! [`Gesture`] is the platform-free state machine; [`TouchMousePlugin`] runs it on iOS only.

use bevy::input::mouse::{MouseButtonInput, MouseMotion, MouseScrollUnit, MouseWheel};
use bevy::input::touch::{TouchInput, TouchPhase};
use bevy::input::{ButtonState, InputSystems};
use bevy::platform::collections::HashMap;
use bevy::prelude::*;
use bevy::window::{CursorMoved, WindowEvent};

/// Logical px a held finger may wander before it is a drag, not a tap.
const SLOP_PX: f32 = 10.0;

/// Seconds a finger may stay down before it holds the left button (a press-and-hold drag).
const DWELL_S: f32 = 0.25;

/// Logical px of pinch per wheel line.
const PINCH_PX_PER_LINE: f32 = 40.0;

/// One mouse action for the frame, in the order the gesture produced it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum MouseOut {
    /// The cursor to this logical position.
    Cursor(Vec2),
    /// Mouse motion by this logical delta.
    Motion(Vec2),
    Press(MouseButton),
    Release(MouseButton),
    /// Wheel lines, positive up (zoom in).
    Wheel(f32),
}

#[derive(Clone, Copy, Debug, PartialEq, Default)]
enum Mode {
    /// No finger down.
    #[default]
    Idle,
    /// One finger down, not yet a press: a tap, a drag or a hold still to be told apart. `hold`:
    /// it landed in the stick's zone, so it presses nothing until the stick is claimed or declined.
    Pending { start: Vec2, since: f32, hold: bool },
    /// One finger holding this button: left, or right while the stick is held.
    Drag(MouseButton),
    /// Two fingers: the right button, and the pair's midpoint and spread last frame.
    Two { mid: Vec2, spread: f32 },
    /// A gesture has ended with fingers still down; nothing until they all lift.
    Draining,
}

/// The touch-to-mouse state machine. Feed it each touch ([`Self::touch`]) and once a frame
/// ([`Self::frame`]); both return the mouse actions to apply.
#[derive(Default)]
pub(crate) struct Gesture {
    fingers: HashMap<u64, Vec2>,
    mode: Mode,
    /// The finger driving the movement stick, outside `fingers` and the mouse.
    stick: Option<StickFinger>,
    /// The stick's zone, `x` below this logical px; `None` outside the world.
    pub(crate) stick_zone: Option<f32>,
    /// Actions held for the next frame: a click's release.
    next_frame: Vec<MouseOut>,
}

/// The stick's finger: where it landed (the stick's centre), where it is, and whether it has moved
/// past [`SLOP_PX`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct StickFinger {
    id: u64,
    pub(crate) center: Vec2,
    pub(crate) at: Vec2,
    moved: bool,
}

impl Gesture {
    /// Where a finger has been pending since before `now`: the candidate a frame later, once the
    /// interface's hover has seen the cursor there.
    pub(crate) fn pending_start(&self, now: f32) -> Option<Vec2> {
        match self.mode {
            Mode::Pending {
                start,
                since,
                hold: true,
            } if since < now && self.fingers.len() == 1 => Some(start),
            _ => None,
        }
    }

    /// Hand the pending finger to the stick; it pressed nothing, so the mouse is left as it was.
    pub(crate) fn claim_stick(&mut self) {
        let Mode::Pending { start, .. } = self.mode else {
            return;
        };
        let Some((&id, &at)) = self.fingers.iter().next() else {
            return;
        };
        self.fingers.remove(&id);
        self.mode = Mode::Idle;
        self.stick = Some(StickFinger {
            id,
            center: start,
            at,
            moved: start.distance(at) > SLOP_PX,
        });
    }

    /// Keep the pending finger for the mouse: a UI frame was under it. A finger already past the
    /// slop presses now, as it would have when it crossed.
    pub(crate) fn decline_stick(&mut self) -> Vec<MouseOut> {
        let Mode::Pending { start, since, .. } = self.mode else {
            return Vec::new();
        };
        self.mode = Mode::Pending {
            start,
            since,
            hold: false,
        };
        let Some(&at) = self.fingers.values().next() else {
            return Vec::new();
        };
        if start.distance(at) <= SLOP_PX {
            return Vec::new();
        }
        let button = self.drag_button();
        self.mode = Mode::Drag(button);
        self.next_frame.push(MouseOut::Cursor(at));
        self.next_frame.push(MouseOut::Motion(at - start));
        vec![MouseOut::Cursor(start), MouseOut::Press(button)]
    }

    /// The stick's finger, while one is down.
    pub(crate) fn stick(&self) -> Option<StickFinger> {
        self.stick
    }

    /// The button a one-finger drag holds: the right, mouselook, while the stick is held.
    fn drag_button(&self) -> MouseButton {
        if self.stick.is_some() {
            MouseButton::Right
        } else {
            MouseButton::Left
        }
    }

    /// A touch of the stick's finger: it moves the stick, and its lift ends it, a tap if it never
    /// moved.
    fn stick_touch(&mut self, phase: TouchPhase, at: Vec2) -> Vec<MouseOut> {
        let Some(stick) = self.stick.as_mut() else {
            return Vec::new();
        };
        match phase {
            TouchPhase::Started | TouchPhase::Moved => {
                stick.at = at;
                stick.moved |= stick.center.distance(at) > SLOP_PX;
                Vec::new()
            }
            TouchPhase::Ended | TouchPhase::Canceled => {
                let tap = !stick.moved && phase == TouchPhase::Ended;
                let center = stick.center;
                self.stick = None;
                if tap && self.mode == Mode::Idle {
                    self.next_frame.push(MouseOut::Release(MouseButton::Left));
                    vec![MouseOut::Cursor(center), MouseOut::Press(MouseButton::Left)]
                } else {
                    Vec::new()
                }
            }
        }
    }

    /// The midpoint and spread of the first two fingers, by id.
    fn pair(&self) -> Option<(Vec2, f32)> {
        let mut ids: Vec<_> = self.fingers.keys().copied().collect();
        ids.sort_unstable();
        let (a, b) = (
            self.fingers.get(ids.first()?)?,
            self.fingers.get(ids.get(1)?)?,
        );
        Some(((*a + *b) / 2.0, a.distance(*b)))
    }

    /// The start of a frame: what the last frame held back.
    pub(crate) fn begin_frame(&mut self) -> Vec<MouseOut> {
        std::mem::take(&mut self.next_frame)
    }

    /// One touch event at `now` seconds.
    pub(crate) fn touch(
        &mut self,
        id: u64,
        phase: TouchPhase,
        at: Vec2,
        now: f32,
    ) -> Vec<MouseOut> {
        if self.stick.is_some_and(|s| s.id == id) {
            return self.stick_touch(phase, at);
        }
        let mut out = Vec::new();
        match phase {
            TouchPhase::Started => {
                self.fingers.insert(id, at);
                match self.mode {
                    Mode::Idle => {
                        self.mode = Mode::Pending {
                            start: at,
                            since: now,
                            hold: self.stick.is_none() && self.stick_zone.is_some_and(|x| at.x < x),
                        };
                        out.push(MouseOut::Cursor(at));
                    }
                    Mode::Pending { .. } => {
                        // A second finger before the first pressed: the right button, at the
                        // pair's midpoint, from the next frame so the cursor is there first.
                        if let Some((mid, spread)) = self.pair() {
                            self.mode = Mode::Two { mid, spread };
                            out.push(MouseOut::Cursor(mid));
                            self.next_frame.push(MouseOut::Press(MouseButton::Right));
                        }
                    }
                    // A finger added to a left drag or a pair changes nothing.
                    Mode::Drag(_) | Mode::Two { .. } | Mode::Draining => {}
                }
            }
            TouchPhase::Moved => {
                let Some(last) = self.fingers.insert(id, at) else {
                    return out;
                };
                match self.mode {
                    Mode::Pending { start, hold, .. } => {
                        out.push(MouseOut::Cursor(at));
                        if !hold && start.distance(at) > SLOP_PX {
                            // The press lands where the finger did; the cursor follows next frame.
                            let button = self.drag_button();
                            out.clear();
                            out.push(MouseOut::Press(button));
                            self.next_frame.push(MouseOut::Cursor(at));
                            self.next_frame.push(MouseOut::Motion(at - start));
                            self.mode = Mode::Drag(button);
                        }
                    }
                    Mode::Drag(_) => {
                        out.push(MouseOut::Cursor(at));
                        out.push(MouseOut::Motion(at - last));
                    }
                    // Judged once a frame ([`Self::frame`]): each finger moves in its own event,
                    // and one finger of a pan alone reads as a pinch.
                    Mode::Two { .. } | Mode::Idle | Mode::Draining => {}
                }
            }
            TouchPhase::Ended | TouchPhase::Canceled => {
                self.fingers.remove(&id);
                match self.mode {
                    Mode::Pending { .. } => {
                        // A tap: the click, its release a frame later.
                        out.push(MouseOut::Cursor(at));
                        out.push(MouseOut::Press(MouseButton::Left));
                        self.next_frame.push(MouseOut::Release(MouseButton::Left));
                        self.mode = Mode::Idle;
                    }
                    Mode::Drag(button) => {
                        out.push(MouseOut::Release(button));
                        self.mode = self.idle_or_draining();
                    }
                    Mode::Two { .. } => {
                        // The press may still be held back for this frame: then it goes now and
                        // the release follows, a two-finger tap's right-click.
                        let held = self
                            .next_frame
                            .iter()
                            .position(|o| *o == MouseOut::Press(MouseButton::Right));
                        if let Some(i) = held {
                            self.next_frame.remove(i);
                            out.push(MouseOut::Press(MouseButton::Right));
                            self.next_frame.push(MouseOut::Release(MouseButton::Right));
                        } else {
                            out.push(MouseOut::Release(MouseButton::Right));
                        }
                        self.mode = self.idle_or_draining();
                    }
                    Mode::Draining => self.mode = self.idle_or_draining(),
                    Mode::Idle => {}
                }
            }
        }
        out
    }

    fn idle_or_draining(&self) -> Mode {
        if self.fingers.is_empty() {
            Mode::Idle
        } else {
            Mode::Draining
        }
    }

    /// The end of a frame at `now` seconds: a pending finger held past [`DWELL_S`] presses, and a
    /// pair's movement over the frame is a pan (cursor and motion) or a pinch (wheel), whichever
    /// is larger.
    pub(crate) fn frame(&mut self, now: f32) -> Vec<MouseOut> {
        match self.mode {
            Mode::Pending {
                since, hold: false, ..
            } if now - since >= DWELL_S => {
                let button = self.drag_button();
                self.mode = Mode::Drag(button);
                vec![MouseOut::Press(button)]
            }
            Mode::Two { mid, spread } => {
                let Some((new_mid, new_spread)) = self.pair() else {
                    return Vec::new();
                };
                self.mode = Mode::Two {
                    mid: new_mid,
                    spread: new_spread,
                };
                let pinch = new_spread - spread;
                let pan = new_mid - mid;
                if pinch == 0.0 && pan == Vec2::ZERO {
                    Vec::new()
                } else if pinch.abs() > pan.length() {
                    vec![MouseOut::Wheel(pinch / PINCH_PX_PER_LINE)]
                } else {
                    vec![MouseOut::Cursor(new_mid), MouseOut::Motion(pan)]
                }
            }
            _ => Vec::new(),
        }
    }
}

/// Runs [`Gesture`] on the touches, on iOS only; elsewhere the OS gives a real mouse.
pub(crate) struct TouchMousePlugin;

impl Plugin for TouchMousePlugin {
    fn build(&self, app: &mut App) {
        if cfg!(target_os = "ios") {
            app.init_resource::<crate::virtual_stick::VirtualStick>()
                .add_systems(PreUpdate, touch_to_mouse.before(InputSystems));
        }
    }
}

/// What decides a stick finger and where the stick goes: the interface's hover, the client
/// state, and the published stick.
#[derive(bevy::ecs::system::SystemParam)]
struct StickGate<'w> {
    hover: Option<Res<'w, crate::ui_script::PlayerUiHover>>,
    client_state: Option<Res<'w, State<crate::client_state::ClientState>>>,
    stick: ResMut<'w, crate::virtual_stick::VirtualStick>,
}

/// The touches, as mouse input for this frame.
fn touch_to_mouse(
    time: Res<Time<bevy::time::Real>>,
    mut touches: MessageReader<TouchInput>,
    mut windows: Query<(Entity, &mut Window)>,
    mut typed: (
        MessageWriter<MouseButtonInput>,
        MessageWriter<MouseMotion>,
        MessageWriter<MouseWheel>,
        MessageWriter<CursorMoved>,
    ),
    mut stream: MessageWriter<WindowEvent>,
    mut gesture: Local<Gesture>,
    stick_gate: StickGate,
) {
    let StickGate {
        hover,
        client_state,
        stick: mut stick_out,
    } = stick_gate;
    let now = time.elapsed_secs();
    let Some((window_entity, mut window)) = windows.iter_mut().next() else {
        return;
    };
    let in_world =
        client_state.is_some_and(|s| *s.get() == crate::client_state::ClientState::InWorld);
    gesture.stick_zone = in_world.then(|| window.width() * crate::virtual_stick::ZONE_WIDTH);
    let mut out = gesture.begin_frame();
    // Last frame's hover saw the cursor where the pending finger landed: the world under it makes
    // it the stick, a frame keeps it for the mouse.
    if gesture.pending_start(now).is_some() {
        if hover.is_none_or(|h| h.0.is_none()) {
            gesture.claim_stick();
        } else {
            out.extend(gesture.decline_stick());
        }
    }
    for touch in touches.read() {
        out.extend(gesture.touch(touch.id, touch.phase, touch.position, now));
    }
    out.extend(gesture.frame(now));
    let stick = if in_world { gesture.stick() } else { None };
    let published = stick.map(|s| (s.center, s.at));
    if stick_out.0 != published {
        stick_out.0 = published;
    }
    let (buttons, motion, wheel, moved) = &mut typed;
    for action in out {
        match action {
            MouseOut::Cursor(at) => {
                // Not marked changed: see the module docs.
                window
                    .bypass_change_detection()
                    .set_cursor_position(Some(at));
                let ev = CursorMoved {
                    window: window_entity,
                    position: at,
                    delta: None,
                };
                moved.write(ev.clone());
                stream.write(WindowEvent::CursorMoved(ev));
            }
            MouseOut::Motion(delta) => {
                motion.write(MouseMotion { delta });
            }
            MouseOut::Press(button) | MouseOut::Release(button) => {
                let state = if matches!(action, MouseOut::Press(_)) {
                    ButtonState::Pressed
                } else {
                    ButtonState::Released
                };
                let ev = MouseButtonInput {
                    button,
                    state,
                    window: window_entity,
                };
                buttons.write(ev);
                stream.write(WindowEvent::MouseButtonInput(ev));
            }
            MouseOut::Wheel(lines) => {
                let ev = MouseWheel {
                    unit: MouseScrollUnit::Line,
                    x: 0.0,
                    y: lines,
                    window: window_entity,
                };
                wheel.write(ev);
                stream.write(WindowEvent::MouseWheel(ev));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use MouseOut::*;

    const L: MouseButton = MouseButton::Left;
    const R: MouseButton = MouseButton::Right;

    /// A gesture with the stick's zone `x < 300`.
    fn in_world() -> Gesture {
        Gesture {
            stick_zone: Some(300.0),
            ..Gesture::default()
        }
    }

    #[test]
    fn a_zone_finger_presses_nothing_until_claimed_then_drives_the_stick() {
        let mut g = in_world();
        let start = Vec2::new(100.0, 500.0);
        assert_eq!(
            g.touch(1, TouchPhase::Started, start, 0.0),
            vec![Cursor(start)]
        );
        // Past the slop in the first frame: held, no press.
        let far = start + Vec2::new(40.0, 0.0);
        assert_eq!(g.touch(1, TouchPhase::Moved, far, 0.01), vec![Cursor(far)]);
        assert_eq!(g.pending_start(0.0), None, "not before a frame has passed");
        assert_eq!(g.pending_start(0.04), Some(start));
        g.claim_stick();
        let stick = g.stick().expect("claimed");
        assert_eq!((stick.center, stick.at), (start, far));
        let further = start + Vec2::new(0.0, -50.0);
        assert_eq!(g.touch(1, TouchPhase::Moved, further, 0.05), vec![]);
        assert_eq!(g.stick().unwrap().at, further);
        assert_eq!(g.touch(1, TouchPhase::Ended, further, 0.3), vec![]);
        assert_eq!(g.stick(), None);
        assert_eq!(g.begin_frame(), vec![], "a moved stick is no click");
    }

    #[test]
    fn a_still_stick_finger_is_a_tap() {
        let mut g = in_world();
        let at = Vec2::new(100.0, 500.0);
        g.touch(1, TouchPhase::Started, at, 0.0);
        g.claim_stick();
        assert_eq!(
            g.touch(1, TouchPhase::Ended, at, 0.1),
            vec![Cursor(at), Press(L)]
        );
        assert_eq!(g.begin_frame(), vec![Release(L)]);
    }

    #[test]
    fn a_declined_zone_finger_past_the_slop_presses_then() {
        let mut g = in_world();
        let start = Vec2::new(100.0, 500.0);
        g.touch(1, TouchPhase::Started, start, 0.0);
        let far = start + Vec2::new(40.0, 0.0);
        g.touch(1, TouchPhase::Moved, far, 0.01);
        assert_eq!(g.decline_stick(), vec![Cursor(start), Press(L)]);
        assert_eq!(g.begin_frame(), vec![Cursor(far), Motion(far - start)]);
        assert_eq!(g.stick(), None);
    }

    #[test]
    fn with_the_stick_held_another_finger_drags_the_right_button() {
        let mut g = in_world();
        g.touch(1, TouchPhase::Started, Vec2::new(100.0, 500.0), 0.0);
        g.claim_stick();
        let look = Vec2::new(800.0, 400.0);
        assert_eq!(
            g.touch(2, TouchPhase::Started, look, 0.1),
            vec![Cursor(look)]
        );
        let moved = look + Vec2::new(30.0, 0.0);
        assert_eq!(g.touch(2, TouchPhase::Moved, moved, 0.12), vec![Press(R)]);
        assert_eq!(g.touch(2, TouchPhase::Ended, moved, 0.2), vec![Release(R)]);
        assert!(g.stick().is_some(), "the stick is still held");
    }

    #[test]
    fn outside_the_zone_or_the_world_a_finger_is_the_mouse_at_once() {
        for mut g in [in_world(), Gesture::default()] {
            let start = Vec2::new(500.0, 500.0);
            let start = if g.stick_zone.is_some() {
                start
            } else {
                Vec2::new(100.0, 500.0)
            };
            g.touch(1, TouchPhase::Started, start, 0.0);
            let far = start + Vec2::new(40.0, 0.0);
            assert_eq!(g.touch(1, TouchPhase::Moved, far, 0.01), vec![Press(L)]);
            assert_eq!(g.pending_start(0.04), None);
        }
    }

    #[test]
    fn a_tap_moves_the_cursor_then_clicks_and_releases_a_frame_later() {
        let mut g = Gesture::default();
        let at = Vec2::new(100.0, 200.0);
        assert_eq!(g.touch(1, TouchPhase::Started, at, 0.0), vec![Cursor(at)]);
        assert_eq!(g.frame(0.03), vec![]);
        assert_eq!(
            g.touch(1, TouchPhase::Ended, at, 0.08),
            vec![Cursor(at), Press(L)]
        );
        assert_eq!(g.begin_frame(), vec![Release(L)]);
        assert_eq!(g.begin_frame(), vec![]);
    }

    #[test]
    fn a_drag_presses_where_the_finger_landed_and_the_cursor_follows() {
        let mut g = Gesture::default();
        let start = Vec2::new(100.0, 100.0);
        g.touch(1, TouchPhase::Started, start, 0.0);
        let near = start + Vec2::new(4.0, 0.0);
        assert_eq!(
            g.touch(1, TouchPhase::Moved, near, 0.02),
            vec![Cursor(near)]
        );
        let far = start + Vec2::new(30.0, 0.0);
        assert_eq!(g.touch(1, TouchPhase::Moved, far, 0.04), vec![Press(L)]);
        assert_eq!(g.begin_frame(), vec![Cursor(far), Motion(far - start)]);
        let further = far + Vec2::new(5.0, 5.0);
        assert_eq!(
            g.touch(1, TouchPhase::Moved, further, 0.06),
            vec![Cursor(further), Motion(Vec2::new(5.0, 5.0))]
        );
        assert_eq!(
            g.touch(1, TouchPhase::Ended, further, 0.08),
            vec![Release(L)]
        );
    }

    #[test]
    fn a_held_finger_presses_after_the_dwell() {
        let mut g = Gesture::default();
        g.touch(1, TouchPhase::Started, Vec2::ZERO, 0.0);
        assert_eq!(g.frame(0.1), vec![]);
        assert_eq!(g.frame(DWELL_S), vec![Press(L)]);
        assert_eq!(
            g.touch(1, TouchPhase::Ended, Vec2::ZERO, 0.5),
            vec![Release(L)]
        );
    }

    #[test]
    fn a_two_finger_tap_right_clicks_at_the_midpoint() {
        let mut g = Gesture::default();
        g.touch(1, TouchPhase::Started, Vec2::new(0.0, 0.0), 0.0);
        let out = g.touch(2, TouchPhase::Started, Vec2::new(100.0, 0.0), 0.02);
        assert_eq!(out, vec![Cursor(Vec2::new(50.0, 0.0))]);
        // Both lift before the held-back press went out: press now, release next frame.
        assert_eq!(
            g.touch(1, TouchPhase::Ended, Vec2::ZERO, 0.03),
            vec![Press(R)]
        );
        assert_eq!(g.begin_frame(), vec![Release(R)]);
        // The second finger lifting ends the drain, with no left click.
        assert_eq!(
            g.touch(2, TouchPhase::Ended, Vec2::new(100.0, 0.0), 0.04),
            vec![]
        );
        assert_eq!(g.frame(1.0), vec![]);
    }

    #[test]
    fn a_two_finger_drag_is_a_right_drag_and_a_pinch_turns_the_wheel() {
        let mut g = Gesture::default();
        g.touch(1, TouchPhase::Started, Vec2::new(0.0, 0.0), 0.0);
        g.touch(2, TouchPhase::Started, Vec2::new(100.0, 0.0), 0.01);
        assert_eq!(g.begin_frame(), vec![Press(R)]);
        // Both move right together, each in its own event: one pan for the frame.
        assert_eq!(
            g.touch(1, TouchPhase::Moved, Vec2::new(10.0, 0.0), 0.05),
            vec![]
        );
        assert_eq!(
            g.touch(2, TouchPhase::Moved, Vec2::new(110.0, 0.0), 0.05),
            vec![]
        );
        assert_eq!(
            g.frame(0.05),
            vec![Cursor(Vec2::new(60.0, 0.0)), Motion(Vec2::new(10.0, 0.0))]
        );
        assert_eq!(g.frame(0.06), vec![], "no movement, nothing");
        // Apart by 80 px: two lines of wheel, zooming in.
        g.touch(2, TouchPhase::Moved, Vec2::new(190.0, 0.0), 0.1);
        assert_eq!(g.frame(0.1), vec![Wheel(2.0)]);
        assert_eq!(
            g.touch(2, TouchPhase::Ended, Vec2::new(190.0, 0.0), 0.2),
            vec![Release(R)]
        );
    }
}
