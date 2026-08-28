//! Maps browser `KeyboardEvent.code` values (physical key position, not
//! layout-dependent) to USB HID usage codes (Usage Page 0x07), which the
//! CH9329 accepts directly.

use super::protocol::modifier;

/// A looked-up key: either one of the 8 modifier bits (folded into the
/// keyboard report's modifier byte, not its key-slot array), or a regular
/// HID usage code (goes into a key slot).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCode {
    Modifier(u8),
    Usage(u8),
}

pub fn lookup(code: &str) -> Option<KeyCode> {
    use KeyCode::{Modifier, Usage};
    Some(match code {
        "KeyA" => Usage(0x04),
        "KeyB" => Usage(0x05),
        "KeyC" => Usage(0x06),
        "KeyD" => Usage(0x07),
        "KeyE" => Usage(0x08),
        "KeyF" => Usage(0x09),
        "KeyG" => Usage(0x0A),
        "KeyH" => Usage(0x0B),
        "KeyI" => Usage(0x0C),
        "KeyJ" => Usage(0x0D),
        "KeyK" => Usage(0x0E),
        "KeyL" => Usage(0x0F),
        "KeyM" => Usage(0x10),
        "KeyN" => Usage(0x11),
        "KeyO" => Usage(0x12),
        "KeyP" => Usage(0x13),
        "KeyQ" => Usage(0x14),
        "KeyR" => Usage(0x15),
        "KeyS" => Usage(0x16),
        "KeyT" => Usage(0x17),
        "KeyU" => Usage(0x18),
        "KeyV" => Usage(0x19),
        "KeyW" => Usage(0x1A),
        "KeyX" => Usage(0x1B),
        "KeyY" => Usage(0x1C),
        "KeyZ" => Usage(0x1D),

        "Digit1" => Usage(0x1E),
        "Digit2" => Usage(0x1F),
        "Digit3" => Usage(0x20),
        "Digit4" => Usage(0x21),
        "Digit5" => Usage(0x22),
        "Digit6" => Usage(0x23),
        "Digit7" => Usage(0x24),
        "Digit8" => Usage(0x25),
        "Digit9" => Usage(0x26),
        "Digit0" => Usage(0x27),

        "Enter" => Usage(0x28),
        "Escape" => Usage(0x29),
        "Backspace" => Usage(0x2A),
        "Tab" => Usage(0x2B),
        "Space" => Usage(0x2C),
        "Minus" => Usage(0x2D),
        "Equal" => Usage(0x2E),
        "BracketLeft" => Usage(0x2F),
        "BracketRight" => Usage(0x30),
        "Backslash" => Usage(0x31),
        "Semicolon" => Usage(0x33),
        "Quote" => Usage(0x34),
        "Backquote" => Usage(0x35),
        "Comma" => Usage(0x36),
        "Period" => Usage(0x37),
        "Slash" => Usage(0x38),
        "CapsLock" => Usage(0x39),

        "F1" => Usage(0x3A),
        "F2" => Usage(0x3B),
        "F3" => Usage(0x3C),
        "F4" => Usage(0x3D),
        "F5" => Usage(0x3E),
        "F6" => Usage(0x3F),
        "F7" => Usage(0x40),
        "F8" => Usage(0x41),
        "F9" => Usage(0x42),
        "F10" => Usage(0x43),
        "F11" => Usage(0x44),
        "F12" => Usage(0x45),

        "PrintScreen" => Usage(0x46),
        "ScrollLock" => Usage(0x47),
        "Pause" => Usage(0x48),
        "Insert" => Usage(0x49),
        "Home" => Usage(0x4A),
        "PageUp" => Usage(0x4B),
        "Delete" => Usage(0x4C),
        "End" => Usage(0x4D),
        "PageDown" => Usage(0x4E),
        "ArrowRight" => Usage(0x4F),
        "ArrowLeft" => Usage(0x50),
        "ArrowDown" => Usage(0x51),
        "ArrowUp" => Usage(0x52),

        "NumLock" => Usage(0x53),
        "NumpadDivide" => Usage(0x54),
        "NumpadMultiply" => Usage(0x55),
        "NumpadSubtract" => Usage(0x56),
        "NumpadAdd" => Usage(0x57),
        "NumpadEnter" => Usage(0x58),
        "Numpad1" => Usage(0x59),
        "Numpad2" => Usage(0x5A),
        "Numpad3" => Usage(0x5B),
        "Numpad4" => Usage(0x5C),
        "Numpad5" => Usage(0x5D),
        "Numpad6" => Usage(0x5E),
        "Numpad7" => Usage(0x5F),
        "Numpad8" => Usage(0x60),
        "Numpad9" => Usage(0x61),
        "Numpad0" => Usage(0x62),
        "NumpadDecimal" => Usage(0x63),

        "ControlLeft" => Modifier(modifier::LEFT_CTRL),
        "ShiftLeft" => Modifier(modifier::LEFT_SHIFT),
        "AltLeft" => Modifier(modifier::LEFT_ALT),
        "MetaLeft" => Modifier(modifier::LEFT_GUI),
        "ControlRight" => Modifier(modifier::RIGHT_CTRL),
        "ShiftRight" => Modifier(modifier::RIGHT_SHIFT),
        "AltRight" => Modifier(modifier::RIGHT_ALT),
        "MetaRight" => Modifier(modifier::RIGHT_GUI),

        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_map_to_sequential_usage_codes() {
        assert_eq!(lookup("KeyA"), Some(KeyCode::Usage(0x04)));
        assert_eq!(lookup("KeyZ"), Some(KeyCode::Usage(0x1D)));
    }

    #[test]
    fn digit_zero_comes_after_nine() {
        assert_eq!(lookup("Digit9"), Some(KeyCode::Usage(0x26)));
        assert_eq!(lookup("Digit0"), Some(KeyCode::Usage(0x27)));
    }

    #[test]
    fn modifiers_return_bitmask_not_usage() {
        assert_eq!(lookup("ShiftLeft"), Some(KeyCode::Modifier(modifier::LEFT_SHIFT)));
        assert_eq!(lookup("MetaRight"), Some(KeyCode::Modifier(modifier::RIGHT_GUI)));
    }

    #[test]
    fn unknown_code_returns_none() {
        assert_eq!(lookup("SomeMadeUpKey"), None);
    }

    #[test]
    fn all_eight_modifier_bits_are_distinct_and_reachable() {
        let codes = [
            "ControlLeft", "ShiftLeft", "AltLeft", "MetaLeft",
            "ControlRight", "ShiftRight", "AltRight", "MetaRight",
        ];
        let mut seen = 0u8;
        for code in codes {
            if let Some(KeyCode::Modifier(bit)) = lookup(code) {
                assert_eq!(seen & bit, 0, "modifier bit {bit:#04x} reused by {code}");
                seen |= bit;
            } else {
                panic!("{code} did not resolve to a modifier");
            }
        }
        assert_eq!(seen, 0xFF);
    }
}
