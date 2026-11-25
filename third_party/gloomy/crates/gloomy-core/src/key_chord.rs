use std::fmt;

use winit::keyboard::{Key, ModifiersState, NamedKey};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct KeyChord {
    pub key: String,
    pub control: bool,
    pub alt: bool,
    pub shift: bool,
    pub super_key: bool,
}

impl KeyChord {
    pub fn from_key(key: &Key, modifiers: ModifiersState) -> Option<Self> {
        Some(Self {
            key: key_name(key)?,
            control: modifiers.control_key(),
            alt: modifiers.alt_key(),
            shift: modifiers.shift_key(),
            super_key: modifiers.super_key(),
        })
    }
}

impl fmt::Display for KeyChord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (enabled, name) in [
            (self.super_key, "super"),
            (self.control, "ctrl"),
            (self.alt, "alt"),
            (self.shift, "shift"),
        ] {
            if enabled {
                write!(formatter, "{name}+")?;
            }
        }
        formatter.write_str(&self.key)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureResult {
    Ignored,
    Cancelled,
    Captured,
}

#[derive(Clone, Debug, Default)]
pub struct KeyCapture {
    waiting: bool,
}

impl KeyCapture {
    pub fn begin(&mut self) {
        self.waiting = true;
    }

    pub fn cancel(&mut self) {
        self.waiting = false;
    }

    pub fn is_waiting(&self) -> bool {
        self.waiting
    }

    pub fn capture(
        &mut self,
        key: &Key,
        modifiers: ModifiersState,
        pressed: bool,
    ) -> (CaptureResult, Option<KeyChord>) {
        if !self.waiting || !pressed {
            return (CaptureResult::Ignored, None);
        }
        if matches!(key, Key::Named(NamedKey::Escape)) {
            self.waiting = false;
            return (CaptureResult::Cancelled, None);
        }
        let Some(chord) = KeyChord::from_key(key, modifiers) else {
            return (CaptureResult::Ignored, None);
        };
        self.waiting = false;
        (CaptureResult::Captured, Some(chord))
    }
}

fn key_name(key: &Key) -> Option<String> {
    match key {
        Key::Character(value) => {
            if value.chars().any(char::is_control) || value.trim().is_empty() {
                None
            } else {
                Some(value.to_lowercase())
            }
        }
        Key::Named(key) => named_key_name(*key).map(str::to_owned),
        Key::Dead(_) | Key::Unidentified(_) => None,
    }
}

fn named_key_name(key: NamedKey) -> Option<&'static str> {
    Some(match key {
        NamedKey::Tab => "tab",
        NamedKey::Enter => "enter",
        NamedKey::Space => "space",
        NamedKey::Backspace => "backspace",
        NamedKey::Delete => "delete",
        NamedKey::Insert => "insert",
        NamedKey::ArrowLeft => "left",
        NamedKey::ArrowRight => "right",
        NamedKey::ArrowUp => "up",
        NamedKey::ArrowDown => "down",
        NamedKey::Home => "home",
        NamedKey::End => "end",
        NamedKey::PageUp => "page_up",
        NamedKey::PageDown => "page_down",
        NamedKey::F1 => "f1",
        NamedKey::F2 => "f2",
        NamedKey::F3 => "f3",
        NamedKey::F4 => "f4",
        NamedKey::F5 => "f5",
        NamedKey::F6 => "f6",
        NamedKey::F7 => "f7",
        NamedKey::F8 => "f8",
        NamedKey::F9 => "f9",
        NamedKey::F10 => "f10",
        NamedKey::F11 => "f11",
        NamedKey::F12 => "f12",
        NamedKey::F13 => "f13",
        NamedKey::F14 => "f14",
        NamedKey::F15 => "f15",
        NamedKey::F16 => "f16",
        NamedKey::F17 => "f17",
        NamedKey::F18 => "f18",
        NamedKey::F19 => "f19",
        NamedKey::F20 => "f20",
        NamedKey::F21 => "f21",
        NamedKey::F22 => "f22",
        NamedKey::F23 => "f23",
        NamedKey::F24 => "f24",
        NamedKey::F25 => "f25",
        NamedKey::F26 => "f26",
        NamedKey::F27 => "f27",
        NamedKey::F28 => "f28",
        NamedKey::F29 => "f29",
        NamedKey::F30 => "f30",
        NamedKey::F31 => "f31",
        NamedKey::F32 => "f32",
        NamedKey::F33 => "f33",
        NamedKey::F34 => "f34",
        NamedKey::F35 => "f35",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chord_names_round_trip_through_tilefire_syntax() {
        let chord = KeyChord::from_key(
            &Key::Character("H".into()),
            ModifiersState::CONTROL
                | ModifiersState::SHIFT
                | ModifiersState::SUPER,
        )
        .unwrap();
        assert_eq!(chord.to_string(), "super+ctrl+shift+h");
        assert_eq!(
            KeyChord::from_key(&Key::Named(NamedKey::F12), ModifiersState::ALT)
                .unwrap()
                .to_string(),
            "alt+f12"
        );
    }

    #[test]
    fn capture_takes_the_next_key_or_escape() {
        let mut capture = KeyCapture::default();
        assert_eq!(
            capture.capture(
                &Key::Character("h".into()),
                ModifiersState::empty(),
                true
            ),
            (CaptureResult::Ignored, None)
        );
        capture.begin();
        assert_eq!(
            capture.capture(
                &Key::Named(NamedKey::Escape),
                ModifiersState::empty(),
                true
            ),
            (CaptureResult::Cancelled, None)
        );
        assert!(!capture.is_waiting());
        capture.begin();
        let (result, chord) = capture.capture(
            &Key::Character("k".into()),
            ModifiersState::CONTROL,
            true,
        );
        assert_eq!(result, CaptureResult::Captured);
        assert_eq!(chord.unwrap().to_string(), "ctrl+k");
        assert!(!capture.is_waiting());
    }

    #[test]
    fn modifier_only_presses_keep_capture_waiting() {
        let mut capture = KeyCapture::default();
        capture.begin();
        assert_eq!(
            capture.capture(
                &Key::Named(NamedKey::Shift),
                ModifiersState::SHIFT,
                true,
            ),
            (CaptureResult::Ignored, None)
        );
        assert!(capture.is_waiting());
    }
}
