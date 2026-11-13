//! An iPad's hardware keyboard (a Magic Keyboard, any Bluetooth or Smart Connector one) as the
//! desktop's keyboard: every key down and up, whatever has focus.
//!
//! winit 0.30's iOS view takes keys only through `UIKeyInput` while it is first responder, which
//! benilla asks for only while an EditBox holds focus and the player taps it
//! ([`crate::soft_keyboard`]): typed text, and no key up or down at all, so no movement, no
//! bindings, no Enter or Escape. Game Controller's `GCKeyboard` (iOS 14+) reports each key by its
//! HID usage, pressed or released, with or without a first responder. [`pump_hw_keys`] turns those
//! into the [`KeyboardInput`] and [`WindowEvent::KeyboardInput`] messages `bevy_winit` writes on the
//! desktop, before bevy's input pass, so the bindings walk, `ButtonInput<KeyCode>` and the EditBoxes
//! all take them as they take a desktop key. Text follows the US layout with Shift and Caps Lock;
//! a held key repeats as a desktop's does ([`REPEAT_DELAY_S`], [`REPEAT_EVERY_S`]).
//!
//! While a hardware keyboard is connected the on-screen keyboard is never asked for, so winit's
//! `UIKeyInput` path stays shut and no character arrives twice.
//!
//! Deviation: benilla's own; the reference has no iPad.

use bevy::input::keyboard::{Key, KeyCode, KeyboardInput};
use bevy::input::ButtonState;
use bevy::prelude::*;
use bevy::window::{PrimaryWindow, WindowEvent};

/// Seconds a key is held before it repeats, and between repeats: macOS's defaults.
pub(crate) const REPEAT_DELAY_S: f64 = 0.5;
pub(crate) const REPEAT_EVERY_S: f64 = 1.0 / 30.0;

/// Whether a hardware keyboard is connected now; the on-screen keyboard stays down while one is.
#[derive(Resource, Default, Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HwKeyboard {
    pub(crate) connected: bool,
}

/// The key a HID keyboard-page usage (`GCKeyCode`) names; `None` for one the desktop has no key
/// for.
pub(crate) fn key_for_usage(usage: isize) -> Option<KeyCode> {
    use KeyCode::*;
    const LETTERS: [KeyCode; 26] = [
        KeyA, KeyB, KeyC, KeyD, KeyE, KeyF, KeyG, KeyH, KeyI, KeyJ, KeyK, KeyL, KeyM, KeyN, KeyO,
        KeyP, KeyQ, KeyR, KeyS, KeyT, KeyU, KeyV, KeyW, KeyX, KeyY, KeyZ,
    ];
    const DIGITS: [KeyCode; 10] = [
        Digit1, Digit2, Digit3, Digit4, Digit5, Digit6, Digit7, Digit8, Digit9, Digit0,
    ];
    const FKEYS: [KeyCode; 12] = [F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12];
    const PAD: [KeyCode; 10] = [
        Numpad1, Numpad2, Numpad3, Numpad4, Numpad5, Numpad6, Numpad7, Numpad8, Numpad9, Numpad0,
    ];
    Some(match usage {
        0x04..=0x1D => LETTERS[(usage - 0x04) as usize],
        0x1E..=0x27 => DIGITS[(usage - 0x1E) as usize],
        0x28 => Enter,
        0x29 => Escape,
        0x2A => Backspace,
        0x2B => Tab,
        0x2C => Space,
        0x2D => Minus,
        0x2E => Equal,
        0x2F => BracketLeft,
        0x30 => BracketRight,
        0x31 | 0x32 => Backslash,
        0x33 => Semicolon,
        0x34 => Quote,
        0x35 => Backquote,
        0x36 => Comma,
        0x37 => Period,
        0x38 => Slash,
        0x39 => CapsLock,
        0x3A..=0x45 => FKEYS[(usage - 0x3A) as usize],
        0x46 => PrintScreen,
        0x47 => ScrollLock,
        0x48 => Pause,
        0x49 => Insert,
        0x4A => Home,
        0x4B => PageUp,
        0x4C => Delete,
        0x4D => End,
        0x4E => PageDown,
        0x4F => ArrowRight,
        0x50 => ArrowLeft,
        0x51 => ArrowDown,
        0x52 => ArrowUp,
        0x53 => NumLock,
        0x54 => NumpadDivide,
        0x55 => NumpadMultiply,
        0x56 => NumpadSubtract,
        0x57 => NumpadAdd,
        0x58 => NumpadEnter,
        0x59..=0x62 => PAD[(usage - 0x59) as usize],
        0x63 => NumpadDecimal,
        0x64 => IntlBackslash,
        0xE0 => ControlLeft,
        0xE1 => ShiftLeft,
        0xE2 => AltLeft,
        0xE3 => SuperLeft,
        0xE4 => ControlRight,
        0xE5 => ShiftRight,
        0xE6 => AltRight,
        0xE7 => SuperRight,
        _ => return None,
    })
}

/// The modifiers held, as the text of a key depends on them.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Mods {
    pub(crate) shift: bool,
    pub(crate) caps: bool,
    /// Control or Command: a chord, which types nothing.
    pub(crate) chord: bool,
}

/// The US-layout character `code` types, unshifted and shifted.
fn us_chars(code: KeyCode) -> Option<(char, char)> {
    use KeyCode::*;
    Some(match code {
        KeyA => ('a', 'A'),
        KeyB => ('b', 'B'),
        KeyC => ('c', 'C'),
        KeyD => ('d', 'D'),
        KeyE => ('e', 'E'),
        KeyF => ('f', 'F'),
        KeyG => ('g', 'G'),
        KeyH => ('h', 'H'),
        KeyI => ('i', 'I'),
        KeyJ => ('j', 'J'),
        KeyK => ('k', 'K'),
        KeyL => ('l', 'L'),
        KeyM => ('m', 'M'),
        KeyN => ('n', 'N'),
        KeyO => ('o', 'O'),
        KeyP => ('p', 'P'),
        KeyQ => ('q', 'Q'),
        KeyR => ('r', 'R'),
        KeyS => ('s', 'S'),
        KeyT => ('t', 'T'),
        KeyU => ('u', 'U'),
        KeyV => ('v', 'V'),
        KeyW => ('w', 'W'),
        KeyX => ('x', 'X'),
        KeyY => ('y', 'Y'),
        KeyZ => ('z', 'Z'),
        Digit1 => ('1', '!'),
        Digit2 => ('2', '@'),
        Digit3 => ('3', '#'),
        Digit4 => ('4', '$'),
        Digit5 => ('5', '%'),
        Digit6 => ('6', '^'),
        Digit7 => ('7', '&'),
        Digit8 => ('8', '*'),
        Digit9 => ('9', '('),
        Digit0 => ('0', ')'),
        Minus => ('-', '_'),
        Equal => ('=', '+'),
        BracketLeft => ('[', '{'),
        BracketRight => (']', '}'),
        Backslash | IntlBackslash => ('\\', '|'),
        Semicolon => (';', ':'),
        Quote => ('\'', '"'),
        Backquote => ('`', '~'),
        Comma => (',', '<'),
        Period => ('.', '>'),
        Slash => ('/', '?'),
        Numpad0 => ('0', '0'),
        Numpad1 => ('1', '1'),
        Numpad2 => ('2', '2'),
        Numpad3 => ('3', '3'),
        Numpad4 => ('4', '4'),
        Numpad5 => ('5', '5'),
        Numpad6 => ('6', '6'),
        Numpad7 => ('7', '7'),
        Numpad8 => ('8', '8'),
        Numpad9 => ('9', '9'),
        NumpadDivide => ('/', '/'),
        NumpadMultiply => ('*', '*'),
        NumpadSubtract => ('-', '-'),
        NumpadAdd => ('+', '+'),
        NumpadDecimal => ('.', '.'),
        _ => return None,
    })
}

/// The logical key and the text a press of `code` carries, as winit reports them on the desktop:
/// a character key carries its character (none under a chord), Space carries " ", and the named
/// keys carry none.
pub(crate) fn logical_and_text(code: KeyCode, mods: Mods) -> (Key, Option<String>) {
    use KeyCode as K;
    let named = match code {
        K::Enter | K::NumpadEnter => Some(Key::Enter),
        K::Escape => Some(Key::Escape),
        K::Backspace => Some(Key::Backspace),
        K::Tab => Some(Key::Tab),
        K::Delete => Some(Key::Delete),
        K::Insert => Some(Key::Insert),
        K::Home => Some(Key::Home),
        K::End => Some(Key::End),
        K::PageUp => Some(Key::PageUp),
        K::PageDown => Some(Key::PageDown),
        K::ArrowUp => Some(Key::ArrowUp),
        K::ArrowDown => Some(Key::ArrowDown),
        K::ArrowLeft => Some(Key::ArrowLeft),
        K::ArrowRight => Some(Key::ArrowRight),
        K::CapsLock => Some(Key::CapsLock),
        K::NumLock => Some(Key::NumLock),
        K::ScrollLock => Some(Key::ScrollLock),
        K::Pause => Some(Key::Pause),
        K::PrintScreen => Some(Key::PrintScreen),
        K::ShiftLeft | K::ShiftRight => Some(Key::Shift),
        K::ControlLeft | K::ControlRight => Some(Key::Control),
        K::AltLeft | K::AltRight => Some(Key::Alt),
        K::SuperLeft | K::SuperRight => Some(Key::Super),
        K::F1 => Some(Key::F1),
        K::F2 => Some(Key::F2),
        K::F3 => Some(Key::F3),
        K::F4 => Some(Key::F4),
        K::F5 => Some(Key::F5),
        K::F6 => Some(Key::F6),
        K::F7 => Some(Key::F7),
        K::F8 => Some(Key::F8),
        K::F9 => Some(Key::F9),
        K::F10 => Some(Key::F10),
        K::F11 => Some(Key::F11),
        K::F12 => Some(Key::F12),
        _ => None,
    };
    if let Some(key) = named {
        return (key, None);
    }
    if code == K::Space {
        return (Key::Space, (!mods.chord).then(|| " ".to_string()));
    }
    let Some((lower, upper)) = us_chars(code) else {
        return (
            Key::Unidentified(bevy::input::keyboard::NativeKey::Unidentified),
            None,
        );
    };
    // Caps Lock shifts letters only, as a desktop's does.
    let shifted = if lower.is_ascii_alphabetic() {
        mods.shift != mods.caps
    } else {
        mods.shift
    };
    let c = if shifted { upper } else { lower };
    (
        Key::Character(c.to_string().into()),
        (!mods.chord).then(|| c.to_string()),
    )
}

/// The keys held and the repeat clock: what the pump carries from frame to frame.
#[derive(Default)]
pub(crate) struct KeyState {
    held: Vec<KeyCode>,
    caps: bool,
    /// The key that repeats (the last non-modifier pressed and still held) and when it next does.
    repeat: Option<(KeyCode, f64)>,
}

/// One key event the pump writes: the key, its state, and whether it is a repeat.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct KeyEvent {
    pub(crate) code: KeyCode,
    pub(crate) state: ButtonState,
    pub(crate) logical: Key,
    pub(crate) text: Option<String>,
    pub(crate) repeat: bool,
}

const MODIFIERS: [KeyCode; 9] = [
    KeyCode::ShiftLeft,
    KeyCode::ShiftRight,
    KeyCode::ControlLeft,
    KeyCode::ControlRight,
    KeyCode::AltLeft,
    KeyCode::AltRight,
    KeyCode::SuperLeft,
    KeyCode::SuperRight,
    KeyCode::CapsLock,
];

impl KeyState {
    fn mods(&self) -> Mods {
        let held = |a, b| self.held.contains(&a) || self.held.contains(&b);
        Mods {
            shift: held(KeyCode::ShiftLeft, KeyCode::ShiftRight),
            caps: self.caps,
            chord: held(KeyCode::ControlLeft, KeyCode::ControlRight)
                || held(KeyCode::SuperLeft, KeyCode::SuperRight),
        }
    }

    fn event(&self, code: KeyCode, state: ButtonState, repeat: bool) -> KeyEvent {
        let (logical, text) = logical_and_text(code, self.mods());
        KeyEvent {
            code,
            state,
            logical,
            text: (state == ButtonState::Pressed).then_some(text).flatten(),
            repeat,
        }
    }

    /// A key went down or up at `now`. A second down of a held key and an up of one not held are
    /// dropped, so the stream stays paired whatever the source reports.
    pub(crate) fn key(&mut self, code: KeyCode, pressed: bool, now: f64) -> Option<KeyEvent> {
        let held = self.held.contains(&code);
        if pressed == held {
            return None;
        }
        if pressed {
            if code == KeyCode::CapsLock {
                self.caps = !self.caps;
            }
            self.held.push(code);
            if !MODIFIERS.contains(&code) {
                self.repeat = Some((code, now + REPEAT_DELAY_S));
            }
            Some(self.event(code, ButtonState::Pressed, false))
        } else {
            self.held.retain(|&k| k != code);
            if self.repeat.is_some_and(|(k, _)| k == code) {
                self.repeat = None;
            }
            Some(self.event(code, ButtonState::Released, false))
        }
    }

    /// The repeats due by `now`.
    pub(crate) fn repeats(&mut self, now: f64) -> Vec<KeyEvent> {
        let mut out = vec![];
        while let Some((code, due)) = self.repeat {
            if due > now {
                break;
            }
            out.push(self.event(code, ButtonState::Pressed, true));
            self.repeat = Some((code, due + REPEAT_EVERY_S));
            // A long stall repeats at most a few times, not a burst.
            if out.len() >= 4 {
                self.repeat = Some((code, now + REPEAT_EVERY_S));
                break;
            }
        }
        out
    }

    /// Every held key released: the keyboard went away.
    pub(crate) fn release_all(&mut self) -> Vec<KeyEvent> {
        let held = std::mem::take(&mut self.held);
        self.repeat = None;
        held.into_iter()
            .map(|code| self.event(code, ButtonState::Released, false))
            .collect()
    }
}

/// Writes the frame's hardware keys as `bevy_winit` writes a desktop's; see the module docs.
#[cfg_attr(not(target_os = "ios"), allow(dead_code))]
fn pump_hw_keys(
    time: Res<Time<bevy::time::Real>>,
    window: Query<Entity, With<PrimaryWindow>>,
    mut connected: ResMut<HwKeyboard>,
    mut state: Local<KeyState>,
    mut keys: MessageWriter<KeyboardInput>,
    mut window_events: MessageWriter<WindowEvent>,
) {
    let now = time.elapsed_secs_f64();
    let is_connected = ios::hook();
    if connected.connected != is_connected {
        connected.connected = is_connected;
        info!(
            "hardware keyboard {}",
            if is_connected {
                "connected"
            } else {
                "disconnected"
            }
        );
    }
    let mut events = vec![];
    for (usage, pressed) in ios::drain() {
        if let Some(code) = key_for_usage(usage) {
            events.extend(state.key(code, pressed, now));
        }
    }
    events.extend(state.repeats(now));
    if !is_connected {
        events.extend(state.release_all());
    }
    let Ok(window) = window.single() else {
        return;
    };
    for ev in events {
        let input = KeyboardInput {
            key_code: ev.code,
            logical_key: ev.logical,
            state: ev.state,
            text: ev.text.map(Into::into),
            repeat: ev.repeat,
            window,
        };
        window_events.write(WindowEvent::KeyboardInput(input.clone()));
        keys.write(input);
    }
}

/// `GCKeyboard`: the keyboard's key-changed handler queues `(usage, pressed)` for the pump. The
/// app links GameController (`ios/project.yml`): a static library carries no framework link.
// objc 0.2's `msg_send!` tests `cfg(feature = "cargo-clippy")` inside the caller's crate.
#[cfg(target_os = "ios")]
#[allow(unexpected_cfgs)]
mod ios {
    use std::sync::Mutex;

    use block::ConcreteBlock;
    use objc::runtime::{Class, Object, BOOL, NO};
    use objc::{msg_send, sel, sel_impl};

    static QUEUE: Mutex<Vec<(isize, bool)>> = Mutex::new(Vec::new());
    /// The `GCKeyboardInput` the handler is on, as an address: a new one (a reconnect) is hooked
    /// again.
    static HOOKED: Mutex<usize> = Mutex::new(0);

    /// Hooks the connected keyboard's input, if not yet; whether one is connected.
    pub(super) fn hook() -> bool {
        let Some(class) = Class::get("GCKeyboard") else {
            return false;
        };
        unsafe {
            let keyboard: *mut Object = msg_send![class, coalescedKeyboard];
            if keyboard.is_null() {
                *HOOKED.lock().unwrap_or_else(|e| e.into_inner()) = 0;
                return false;
            }
            let input: *mut Object = msg_send![keyboard, keyboardInput];
            if input.is_null() {
                return false;
            }
            let mut hooked = HOOKED.lock().unwrap_or_else(|e| e.into_inner());
            if *hooked != input as usize {
                let handler = ConcreteBlock::new(
                    |_kb: *mut Object, _key: *mut Object, usage: isize, pressed: BOOL| {
                        QUEUE
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push((usage, pressed != NO));
                    },
                )
                .copy();
                let _: () = msg_send![input, setKeyChangedHandler: &*handler];
                // The keyboard input holds its own copy; this one lives as long as the process,
                // one per connection.
                std::mem::forget(handler);
                *hooked = input as usize;
            }
            true
        }
    }

    pub(super) fn drain() -> Vec<(isize, bool)> {
        std::mem::take(&mut *QUEUE.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

/// Elsewhere there is no `GCKeyboard`: the desktop's keyboard already arrives through winit.
#[cfg(not(target_os = "ios"))]
#[allow(dead_code)]
mod ios {
    pub(super) fn hook() -> bool {
        false
    }
    pub(super) fn drain() -> Vec<(isize, bool)> {
        vec![]
    }
}

/// The pump, on iOS; the connection flag everywhere (the soft keyboard reads it).
pub(crate) struct HwKeyboardPlugin;

impl Plugin for HwKeyboardPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<HwKeyboard>();
        #[cfg(target_os = "ios")]
        app.add_systems(PreUpdate, pump_hw_keys.before(bevy::input::InputSystems));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_usages_name_the_desktop_keys() {
        assert_eq!(key_for_usage(0x04), Some(KeyCode::KeyA));
        assert_eq!(key_for_usage(0x1D), Some(KeyCode::KeyZ));
        assert_eq!(key_for_usage(0x1E), Some(KeyCode::Digit1));
        assert_eq!(key_for_usage(0x27), Some(KeyCode::Digit0));
        assert_eq!(key_for_usage(0x28), Some(KeyCode::Enter));
        assert_eq!(key_for_usage(0x29), Some(KeyCode::Escape));
        assert_eq!(key_for_usage(0x2B), Some(KeyCode::Tab));
        assert_eq!(key_for_usage(0x3A), Some(KeyCode::F1));
        assert_eq!(key_for_usage(0x45), Some(KeyCode::F12));
        assert_eq!(key_for_usage(0x52), Some(KeyCode::ArrowUp));
        assert_eq!(key_for_usage(0x62), Some(KeyCode::Numpad0));
        assert_eq!(key_for_usage(0xE1), Some(KeyCode::ShiftLeft));
        assert_eq!(key_for_usage(0xE3), Some(KeyCode::SuperLeft));
        assert_eq!(key_for_usage(0x00), None);
        assert_eq!(key_for_usage(0x80), None);
    }

    #[test]
    fn text_follows_shift_and_caps_lock_on_the_us_layout() {
        let m = |shift, caps| Mods {
            shift,
            caps,
            chord: false,
        };
        let text = |code, mods| logical_and_text(code, mods).1;
        assert_eq!(text(KeyCode::KeyA, m(false, false)).as_deref(), Some("a"));
        assert_eq!(text(KeyCode::KeyA, m(true, false)).as_deref(), Some("A"));
        assert_eq!(text(KeyCode::KeyA, m(false, true)).as_deref(), Some("A"));
        assert_eq!(text(KeyCode::KeyA, m(true, true)).as_deref(), Some("a"));
        assert_eq!(text(KeyCode::Digit1, m(false, true)).as_deref(), Some("1"));
        assert_eq!(text(KeyCode::Digit1, m(true, false)).as_deref(), Some("!"));
        assert_eq!(text(KeyCode::Slash, m(true, false)).as_deref(), Some("?"));
        assert_eq!(text(KeyCode::Space, m(false, false)).as_deref(), Some(" "));
        assert_eq!(text(KeyCode::Enter, m(false, false)), None);
        assert_eq!(
            logical_and_text(KeyCode::Escape, m(false, false)).0,
            Key::Escape
        );
        let chord = Mods {
            chord: true,
            ..m(false, false)
        };
        assert_eq!(text(KeyCode::KeyC, chord), None, "a chord types nothing");
    }

    #[test]
    fn presses_pair_with_releases_and_a_held_key_repeats() {
        let mut s = KeyState::default();
        let w = s.key(KeyCode::KeyW, true, 0.0).unwrap();
        assert_eq!(
            (w.state, w.text.as_deref(), w.repeat),
            (ButtonState::Pressed, Some("w"), false)
        );
        assert!(
            s.key(KeyCode::KeyW, true, 0.1).is_none(),
            "a second down is dropped"
        );
        assert!(s.repeats(0.4).is_empty(), "no repeat inside the delay");
        let r = s.repeats(REPEAT_DELAY_S + 0.001);
        assert_eq!(r.len(), 1);
        assert!(r[0].repeat && r[0].state == ButtonState::Pressed);
        let up = s.key(KeyCode::KeyW, false, 1.0).unwrap();
        assert_eq!((up.state, up.text), (ButtonState::Released, None));
        assert!(s.repeats(5.0).is_empty(), "released keys stop repeating");
        assert!(
            s.key(KeyCode::KeyW, false, 1.1).is_none(),
            "an unpaired up is dropped"
        );
    }

    #[test]
    fn shift_shapes_the_text_and_modifiers_do_not_repeat() {
        let mut s = KeyState::default();
        s.key(KeyCode::ShiftLeft, true, 0.0);
        assert!(s.repeats(2.0).is_empty(), "a held modifier never repeats");
        let a = s.key(KeyCode::KeyA, true, 2.0).unwrap();
        assert_eq!(a.text.as_deref(), Some("A"));
        s.key(KeyCode::CapsLock, true, 2.1);
        s.key(KeyCode::CapsLock, false, 2.2);
        s.key(KeyCode::ShiftLeft, false, 2.3);
        s.key(KeyCode::KeyA, false, 2.3);
        let b = s.key(KeyCode::KeyB, true, 2.4).unwrap();
        assert_eq!(b.text.as_deref(), Some("B"), "Caps Lock is on");
    }

    #[test]
    fn a_disconnect_releases_every_held_key() {
        let mut s = KeyState::default();
        s.key(KeyCode::KeyW, true, 0.0);
        s.key(KeyCode::ShiftLeft, true, 0.0);
        let ups = s.release_all();
        assert_eq!(ups.len(), 2);
        assert!(ups.iter().all(|e| e.state == ButtonState::Released));
        assert!(s.repeats(9.0).is_empty());
    }
}
