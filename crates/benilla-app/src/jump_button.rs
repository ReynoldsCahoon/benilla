//! The jump button, on iOS: a ring under the minimap that a finger [`crate::touch_mouse`] hands
//! over presses as the `JUMP` binding does, `Jump()` (`0x513bd0`) once per touch.
//!
//! - The ring sits centred under the `<Minimap>` widget's rect, so it follows the interface's
//!   scale and moves with the minimap; with no minimap shown it sits in the top-right corner.
//! - An interface frame under the finger keeps it for the mouse, as the stick does, so an open bag
//!   or a frame dragged over the ring takes its taps first.
//!
//! Deviation: benilla's own control. The reference has no touch input; it jumps from the `JUMP`
//! binding's key alone.

use benilla_ui::script::{BindingInput, FiredInput};
use bevy::prelude::*;

/// Logical px from the ring's centre to its edge, which is also where a finger counts as on it
/// ([`crate::touch_mouse`]'s jump zone).
pub(crate) const RADIUS: f32 = 30.0;

/// Logical px between the minimap's bottom edge and the ring's, past the zoom buttons that hang
/// below the disc.
const GAP: f32 = 24.0;

/// Logical px from the window's top and right edges to the ring with no minimap shown.
const CORNER: f32 = 48.0;

/// The button this frame: where its centre is (`None` outside the world) and whether a finger
/// pressed it since the bindings last read it.
#[derive(Resource, Default, Clone, Copy, PartialEq)]
pub(crate) struct JumpButton {
    pub(crate) center: Option<Vec2>,
    pub(crate) pressed: bool,
}

/// The ring's centre for a minimap at `minimap` (y-down logical px) in a `width`-wide window.
fn center_for(minimap: Option<Rect>, width: f32) -> Vec2 {
    match minimap {
        Some(r) => Vec2::new(r.center().x, r.max.y + GAP + RADIUS),
        None => Vec2::new(width - CORNER - RADIUS, CORNER + RADIUS),
    }
}

/// Places the button for this frame, in the world only; runs before the touches are read.
pub(crate) fn place_button(
    mut button: ResMut<JumpButton>,
    minimap: Option<Res<crate::minimap::MinimapWidget>>,
    windows: Query<&Window>,
    client_state: Option<Res<State<crate::client_state::ClientState>>>,
) {
    let in_world = client_state
        .as_ref()
        .is_some_and(|s| *s.get() == crate::client_state::ClientState::InWorld);
    let center = windows.single().ok().filter(|_| in_world).map(|w| {
        let rect = minimap.as_ref().and_then(|m| m.0.as_ref()).map(|s| s.rect);
        center_for(rect, w.width())
    });
    if button.center != center {
        button.center = center;
    }
}

/// A press into the bindings, after the frame's binding calls, as the stick's directions go.
pub(crate) fn fire_jump(
    button: Option<ResMut<JumpButton>>,
    mut binds: ResMut<crate::bindings::BindingsState>,
) {
    let Some(mut button) = button else { return };
    if button.pressed {
        button.pressed = false;
        info!("jump button: Jump()");
        binds.apply_device_input(vec![BindingInput::Fire(FiredInput::Jump)]);
    }
}

/// The ring and its chevron at the button's centre, a quad in the player UI's pass
/// ([`crate::touch_art`]); nothing outside the world.
fn draw_button(
    button: Res<JumpButton>,
    art: Option<Res<crate::touch_art::TouchArt>>,
    mut quads: ResMut<crate::ui_pass::UiQuads>,
) {
    let (Some(center), Some(art)) = (button.center, art) else {
        return;
    };
    crate::touch_art::push(&mut quads, &art.jump, center, 2.0 * RADIUS);
}

/// Places and draws the button on iOS; [`fire_jump`] runs in the bindings' tail on every platform
/// and fires nothing without a press.
pub(crate) struct JumpButtonPlugin;

impl Plugin for JumpButtonPlugin {
    fn build(&self, app: &mut App) {
        if cfg!(target_os = "ios") {
            app.init_resource::<JumpButton>()
                .add_systems(
                    PreUpdate,
                    // Before the touches read it: `touch_to_mouse` orders itself after.
                    place_button.before(bevy::input::InputSystems),
                )
                .add_systems(Update, draw_button.in_set(crate::ui_pass::UiQuadAppend));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_sits_centred_under_the_minimap() {
        let minimap = Rect::new(870.0, 20.0, 1010.0, 160.0);
        let c = center_for(Some(minimap), 1024.0);
        assert_eq!(c, Vec2::new(940.0, 160.0 + GAP + RADIUS));
    }

    #[test]
    fn with_no_minimap_it_sits_in_the_top_right_corner() {
        let c = center_for(None, 1024.0);
        assert_eq!(c, Vec2::new(1024.0 - CORNER - RADIUS, CORNER + RADIUS));
    }
}
