//! Converts plain text into a sequence of HID keystrokes for the paste-box
//! feature. US QWERTY layout only — documented limitation, not a general
//! keyboard-layout engine.

/// One keystroke to simulate: the HID usage code, and whether Shift must
/// be held while it's pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Keystroke {
    pub usage: u8,
    pub shift: bool,
}

fn lookup(c: char) -> Option<Keystroke> {
    let (usage, shift) = match c {
        'a'..='z' => (0x04 + (c as u8 - b'a'), false),
        'A'..='Z' => (0x04 + (c as u8 - b'A'), true),
        '1'..='9' => (0x1E + (c as u8 - b'1'), false),
        '0' => (0x27, false),
        '\n' => (0x28, false),
        '\t' => (0x2B, false),
        ' ' => (0x2C, false),
        '-' => (0x2D, false),
        '_' => (0x2D, true),
        '=' => (0x2E, false),
        '+' => (0x2E, true),
        '[' => (0x2F, false),
        '{' => (0x2F, true),
        ']' => (0x30, false),
        '}' => (0x30, true),
        '\\' => (0x31, false),
        '|' => (0x31, true),
        ';' => (0x33, false),
        ':' => (0x33, true),
        '\'' => (0x34, false),
        '"' => (0x34, true),
        '`' => (0x35, false),
        '~' => (0x35, true),
        ',' => (0x36, false),
        '<' => (0x36, true),
        '.' => (0x37, false),
        '>' => (0x37, true),
        '/' => (0x38, false),
        '?' => (0x38, true),
        '!' => (0x1E, true),
        '@' => (0x1F, true),
        '#' => (0x20, true),
        '$' => (0x21, true),
        '%' => (0x22, true),
        '^' => (0x23, true),
        '&' => (0x24, true),
        '*' => (0x25, true),
        '(' => (0x26, true),
        ')' => (0x27, true),
        _ => return None,
    };
    Some(Keystroke { usage, shift })
}

/// Encodes `text` into keystrokes, silently skipping characters with no
/// US-QWERTY mapping (e.g. non-ASCII).
pub fn encode(text: &str) -> Vec<Keystroke> {
    text.chars().filter_map(lookup).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowercase_letters_have_no_shift() {
        assert_eq!(lookup('a'), Some(Keystroke { usage: 0x04, shift: false }));
    }

    #[test]
    fn uppercase_letters_use_same_usage_with_shift() {
        assert_eq!(lookup('A'), Some(Keystroke { usage: 0x04, shift: true }));
    }

    #[test]
    fn shifted_digit_symbols_map_to_the_digit_usage() {
        assert_eq!(lookup('!'), Some(Keystroke { usage: 0x1E, shift: true }));
        assert_eq!(lookup('1'), Some(Keystroke { usage: 0x1E, shift: false }));
    }

    #[test]
    fn encode_round_trips_a_representative_string() {
        let keys = encode("Hi 1!");
        assert_eq!(
            keys,
            vec![
                Keystroke { usage: 0x0B, shift: true },  // H
                Keystroke { usage: 0x0C, shift: false }, // i
                Keystroke { usage: 0x2C, shift: false }, // space
                Keystroke { usage: 0x1E, shift: false }, // 1
                Keystroke { usage: 0x1E, shift: true },  // !
            ]
        );
    }

    #[test]
    fn unsupported_characters_are_skipped_not_erroring() {
        assert_eq!(encode("a€b"), vec![
            Keystroke { usage: 0x04, shift: false },
            Keystroke { usage: 0x05, shift: false },
        ]);
    }
}
