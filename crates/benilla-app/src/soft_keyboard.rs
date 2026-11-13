//! The on-screen keyboard, on iOS, where the client runs with no hardware keyboard: it shows while
//! an EditBox holds keyboard focus and hides when none does, and its keystrokes are made into keys
//! the interface's walk takes.
//!
//! - Showing: a tap that ends on the EditBox holding focus asks for the keyboard; focus alone does
//!   not, so a box the interface focuses by itself (the login's account name, character create's
//!   name) waits for a tap, and taps elsewhere while it keeps focus (a race, a class) bring nothing
//!   up. The keyboard hides when no box has focus. The window's `ime_enabled` carries the request
//!   to winit, whose iOS view becomes (or resigns) first responder; the player can dismiss the
//!   keyboard with focus still in a box, which benilla cannot see, so a tap on the box while
//!   `ime_enabled` is already set asks winit directly.
//! - Keys: winit reports each character the keyboard types as a press and a release of an
//!   unidentified physical key, which the interface's walk drops (`bindings::chord::walk_name`)
//!   before the character reaches the box. [`soft_key`] gives such a key the code a US keyboard
//!   makes the character with, keeping its text, so the box inserts exactly what was typed; Return,
//!   which arrives as the character `"\n"`, becomes Enter. Backspace arrives as itself.

use bevy::input::keyboard::{Key, KeyCode, KeyboardInput, NativeKeyCode};
use bevy::prelude::*;

use crate::ui_script::{UiInput, UiKeyboardCapture};

/// The keyboard as a key the interface's walk takes, or `None` for a key that needs nothing: an
/// identified physical key, or a release or press that names no character.
pub(crate) fn soft_key(ev: &KeyboardInput) -> Option<KeyboardInput> {
    if ev.key_code != KeyCode::Unidentified(NativeKeyCode::Unidentified) {
        return None;
    }
    let Key::Character(chars) = &ev.logical_key else {
        return None;
    };
    let c = chars.chars().next()?;
    let mut key = ev.clone();
    if c == '\n' || c == '\r' {
        key.key_code = KeyCode::Enter;
        key.logical_key = Key::Enter;
        key.text = None;
    } else {
        key.key_code = us_key_for(c);
    }
    Some(key)
}

/// The US-layout key that types `c`. A character no US key types (an accent, another script) is
/// given `KeyA`: the box that has focus takes every key, and inserts the text the keyboard sent.
fn us_key_for(c: char) -> KeyCode {
    use KeyCode::*;
    const LETTERS: [KeyCode; 26] = [
        KeyA, KeyB, KeyC, KeyD, KeyE, KeyF, KeyG, KeyH, KeyI, KeyJ, KeyK, KeyL, KeyM, KeyN, KeyO,
        KeyP, KeyQ, KeyR, KeyS, KeyT, KeyU, KeyV, KeyW, KeyX, KeyY, KeyZ,
    ];
    const DIGITS: [KeyCode; 10] = [
        Digit0, Digit1, Digit2, Digit3, Digit4, Digit5, Digit6, Digit7, Digit8, Digit9,
    ];
    match c {
        'a'..='z' => LETTERS[c as usize - 'a' as usize],
        'A'..='Z' => LETTERS[c as usize - 'A' as usize],
        '0'..='9' => DIGITS[c as usize - '0' as usize],
        ')' => Digit0,
        '!' => Digit1,
        '@' => Digit2,
        '#' => Digit3,
        '$' => Digit4,
        '%' => Digit5,
        '^' => Digit6,
        '&' => Digit7,
        '*' => Digit8,
        '(' => Digit9,
        ' ' => Space,
        '-' | '_' => Minus,
        '=' | '+' => Equal,
        '[' | '{' => BracketLeft,
        ']' | '}' => BracketRight,
        '\\' | '|' => Backslash,
        ';' | ':' => Semicolon,
        '\'' | '"' => Quote,
        ',' | '<' => Comma,
        '.' | '>' => Period,
        '/' | '?' => Slash,
        '`' | '~' => Backquote,
        _ => KeyA,
    }
}

/// Shows the on-screen keyboard while an EditBox has focus; on iOS only.
pub(crate) struct SoftKeyboardPlugin;

impl Plugin for SoftKeyboardPlugin {
    fn build(&self, app: &mut App) {
        if cfg!(target_os = "ios") {
            app.add_systems(Update, follow_focus.after(UiInput));
        }
    }
}

/// Shows the keyboard for a tap on the focused box and hides it with no box focused; see the
/// module docs. After the interface's input pass, so a tap that focused a box has, and the hover
/// names the frame the tap ended on.
fn follow_focus(
    capture: Res<UiKeyboardCapture>,
    hw: Option<Res<crate::hw_keyboard::HwKeyboard>>,
    hover: Res<crate::ui_script::PlayerUiHover>,
    buttons: Res<ButtonInput<MouseButton>>,
    mut windows: Query<(Entity, &mut Window)>,
) {
    // A hardware keyboard types into the box itself ([`crate::hw_keyboard`]); the on-screen one
    // stays down, and winit's text path with it, so nothing arrives twice.
    let hardware = hw.is_some_and(|h| h.connected);
    let tapped_box = !hardware
        && buttons.just_released(MouseButton::Left)
        && capture.typing
        && hover.0.is_some()
        && hover.0 == capture.focused_box;
    for (entity, mut window) in &mut windows {
        if !capture.typing || hardware {
            if window.ime_enabled {
                window.ime_enabled = false;
            }
        } else if tapped_box {
            if window.ime_enabled {
                // Dismissed by the player with the box still focused: ask winit again.
                bevy::winit::WINIT_WINDOWS.with_borrow(|winit_windows| {
                    if let Some(winit_window) = winit_windows.get_window(entity) {
                        winit_window.set_ime_allowed(true);
                    }
                });
            } else {
                window.ime_enabled = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::input::ButtonState;

    fn typed(text: &str, state: ButtonState) -> KeyboardInput {
        KeyboardInput {
            key_code: KeyCode::Unidentified(NativeKeyCode::Unidentified),
            logical_key: Key::Character(text.into()),
            state,
            text: (state == ButtonState::Pressed).then(|| text.into()),
            repeat: false,
            window: Entity::PLACEHOLDER,
        }
    }

    #[test]
    fn a_typed_character_gets_its_us_key_and_keeps_its_text() {
        let key = soft_key(&typed("P", ButtonState::Pressed)).unwrap();
        assert_eq!(key.key_code, KeyCode::KeyP);
        assert_eq!(key.text.as_deref(), Some("P"));
        let up = soft_key(&typed("P", ButtonState::Released)).unwrap();
        assert_eq!(up.key_code, KeyCode::KeyP, "the release names the same key");
        assert_eq!(
            soft_key(&typed("?", ButtonState::Pressed))
                .unwrap()
                .key_code,
            KeyCode::Slash
        );
        let accent = soft_key(&typed("é", ButtonState::Pressed)).unwrap();
        assert_eq!(accent.key_code, KeyCode::KeyA);
        assert_eq!(
            accent.text.as_deref(),
            Some("é"),
            "the box still inserts what was typed"
        );
    }

    #[test]
    fn return_is_enter() {
        let key = soft_key(&typed("\n", ButtonState::Pressed)).unwrap();
        assert_eq!(key.key_code, KeyCode::Enter);
        assert_eq!(key.logical_key, Key::Enter);
        assert_eq!(key.text, None, "Enter inserts nothing");
    }

    #[test]
    fn an_identified_key_is_left_alone() {
        let mut backspace = typed("x", ButtonState::Pressed);
        backspace.key_code = KeyCode::Backspace;
        backspace.logical_key = Key::Backspace;
        assert!(soft_key(&backspace).is_none());
    }
}
