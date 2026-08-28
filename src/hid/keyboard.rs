//! Held-key tracking and USB HID boot-keyboard report assembly.
//!
//! One `Keyboard` exists per CH9329, owned by the writer: the chip presents
//! a single physical keyboard to the target machine, so what is currently
//! held is a property of that keyboard, not of whichever browser session
//! happened to press the key.

use std::collections::HashSet;

use super::keymap::{self, KeyCode};

/// Slots in the boot-keyboard report for simultaneously-held non-modifier
/// keys. Modifiers don't use these - they have their own bitmask byte.
const KEY_SLOTS: usize = 6;

#[derive(Default)]
pub struct Keyboard {
    held: HashSet<String>,
}

impl Keyboard {
    /// Records one browser `KeyboardEvent.code` going down or up and
    /// returns the report to send: the modifier bitmask plus the usage
    /// codes of everything still held. `None` for a code with no HID
    /// mapping — that key is neither tracked nor reported.
    ///
    /// Six-key rollover: while more than six non-modifier keys are held,
    /// the ones that don't fit are simply left out of the report (which
    /// six is unspecified) but stay tracked, so they appear again as soon
    /// as a slot frees up. Modifiers are never dropped this way.
    pub fn apply(&mut self, code: &str, pressed: bool) -> Option<(u8, [u8; KEY_SLOTS])> {
        keymap::lookup(code)?;
        if pressed {
            self.held.insert(code.to_string());
        } else {
            self.held.remove(code);
        }

        let mut modifiers = 0u8;
        let mut keys = [0u8; KEY_SLOTS];
        let mut slot = 0;
        for held_code in &self.held {
            match keymap::lookup(held_code) {
                Some(KeyCode::Modifier(bit)) => modifiers |= bit,
                Some(KeyCode::Usage(usage)) if slot < keys.len() => {
                    keys[slot] = usage;
                    slot += 1;
                }
                _ => {}
            }
        }
        Some((modifiers, keys))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::protocol::modifier;

    fn filled(keys: [u8; KEY_SLOTS]) -> Vec<u8> {
        let mut filled: Vec<u8> = keys.into_iter().filter(|&k| k != 0).collect();
        filled.sort_unstable();
        filled
    }

    #[test]
    fn a_pressed_key_lands_in_a_slot_and_leaves_on_release() {
        let mut keyboard = Keyboard::default();

        assert_eq!(keyboard.apply("KeyA", true), Some((0, [0x04, 0, 0, 0, 0, 0])));
        assert_eq!(keyboard.apply("KeyA", false), Some((0, [0; 6])));
    }

    #[test]
    fn modifiers_go_in_the_bitmask_not_a_slot() {
        let mut keyboard = Keyboard::default();

        assert_eq!(keyboard.apply("ShiftLeft", true), Some((modifier::LEFT_SHIFT, [0; 6])));
        assert_eq!(keyboard.apply("KeyA", true), Some((modifier::LEFT_SHIFT, [0x04, 0, 0, 0, 0, 0])));
    }

    #[test]
    fn an_unknown_code_is_neither_reported_nor_tracked() {
        let mut keyboard = Keyboard::default();

        assert_eq!(keyboard.apply("SomeMadeUpKey", true), None);
        assert_eq!(keyboard.apply("KeyA", true), Some((0, [0x04, 0, 0, 0, 0, 0])));
    }

    /// The report only has six slots. A seventh held key is left out of it
    /// but stays tracked, so releasing one of the six brings it back.
    #[test]
    fn a_seventh_held_key_is_dropped_from_the_report_but_not_forgotten() {
        let mut keyboard = Keyboard::default();
        let codes = ["KeyA", "KeyB", "KeyC", "KeyD", "KeyE", "KeyF", "KeyG"];
        let usages = [0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A];

        let mut report = None;
        for code in codes {
            report = keyboard.apply(code, true);
        }
        let (modifiers, keys) = report.expect("every code here is a known key");
        assert_eq!(modifiers, 0);
        let reported = filled(keys);
        assert_eq!(reported.len(), KEY_SLOTS, "exactly six of the seven held keys fit");
        assert!(reported.iter().all(|usage| usages.contains(usage)), "reported keys must all be held ones");

        // The one that missed out is still held: releasing a key that did
        // make the report frees its slot for it.
        let dropped = *usages.iter().find(|usage| !reported.contains(usage)).expect("one of the seven must have missed out");
        let released = codes[usages.iter().position(|&u| u == reported[0]).expect("reported keys come from the held set")];
        let (_, keys) = keyboard.apply(released, false).expect("releasing a known key reports");
        let reported = filled(keys);
        assert_eq!(reported.len(), KEY_SLOTS, "six keys are still held after releasing one of seven");
        assert!(reported.contains(&dropped), "the key that missed out must take the freed slot");
    }

    #[test]
    fn modifiers_are_never_dropped_by_rollover() {
        let mut keyboard = Keyboard::default();
        for code in ["KeyA", "KeyB", "KeyC", "KeyD", "KeyE", "KeyF", "KeyG"] {
            keyboard.apply(code, true);
        }

        let (modifiers, keys) = keyboard.apply("ControlLeft", true).expect("ControlLeft is a known key");

        assert_eq!(modifiers, modifier::LEFT_CTRL);
        assert_eq!(filled(keys).len(), KEY_SLOTS);
    }
}
