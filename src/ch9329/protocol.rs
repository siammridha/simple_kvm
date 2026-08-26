//! Pure CH9329 packet encoding — no I/O. Verified against two independent
//! open-source CH9329 implementations (py-ch9329, MiniKvm_public's CH9329.cs).

pub const CMD_SEND_KB_GENERAL_DATA: u8 = 0x02;
pub const CMD_SEND_MS_ABS_DATA: u8 = 0x04;
pub const CMD_SEND_MS_REL_DATA: u8 = 0x05;

const HEADER: [u8; 2] = [0x57, 0xAB];
const ADDR: u8 = 0x00;

pub mod modifier {
    pub const LEFT_CTRL: u8 = 1 << 0;
    pub const LEFT_SHIFT: u8 = 1 << 1;
    pub const LEFT_ALT: u8 = 1 << 2;
    pub const LEFT_GUI: u8 = 1 << 3;
    pub const RIGHT_CTRL: u8 = 1 << 4;
    pub const RIGHT_SHIFT: u8 = 1 << 5;
    pub const RIGHT_ALT: u8 = 1 << 6;
    pub const RIGHT_GUI: u8 = 1 << 7;
}

/// Frames a command: `0x57 0xAB | addr | cmd | len | data | checksum`.
/// Checksum is the sum of every preceding byte, mod 256.
fn assemble(cmd: u8, data: &[u8]) -> Vec<u8> {
    let mut packet = Vec::with_capacity(HEADER.len() + 3 + data.len() + 1);
    packet.extend_from_slice(&HEADER);
    packet.push(ADDR);
    packet.push(cmd);
    packet.push(data.len() as u8);
    packet.extend_from_slice(data);
    let checksum = packet.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
    packet.push(checksum);
    packet
}

/// Standard USB HID boot keyboard report: modifier bitmask, reserved byte,
/// up to 6 simultaneous HID usage codes (0x00 = empty slot).
pub fn keyboard_report(modifiers: u8, keys: [u8; 6]) -> Vec<u8> {
    let mut data = [0u8; 8];
    data[0] = modifiers;
    data[2..8].copy_from_slice(&keys);
    assemble(CMD_SEND_KB_GENERAL_DATA, &data)
}

/// Scales a 0.0..=1.0 fraction of the screen to the CH9329's 0..4096
/// absolute-axis range.
fn scale_axis(frac: f32) -> u16 {
    (frac.clamp(0.0, 1.0) * 4096.0).round() as u16
}

/// Absolute mouse report. `x_frac`/`y_frac` are the pointer position as a
/// fraction of the video frame (0.0 = left/top edge, 1.0 = right/bottom).
pub fn mouse_absolute(buttons: u8, x_frac: f32, y_frac: f32, wheel: i8) -> Vec<u8> {
    let x = scale_axis(x_frac);
    let y = scale_axis(y_frac);
    let data = [
        0x02, // constant absolute-mode marker byte
        buttons,
        (x & 0xFF) as u8,
        (x >> 8) as u8,
        (y & 0xFF) as u8,
        (y >> 8) as u8,
        wheel as u8,
    ];
    assemble(CMD_SEND_MS_ABS_DATA, &data)
}

/// Relative mouse report: button state plus signed pixel deltas.
pub fn mouse_relative(buttons: u8, dx: i8, dy: i8, wheel: i8) -> Vec<u8> {
    let data = [
        0x01, // constant relative-mode marker byte
        buttons,
        dx as u8,
        dy as u8,
        wheel as u8,
    ];
    assemble(CMD_SEND_MS_REL_DATA, &data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn naive_checksum(bytes: &[u8]) -> u8 {
        bytes.iter().fold(0u8, |acc, &b| acc.wrapping_add(b))
    }

    /// Golden vector from py-ch9329's own documented example: pressing the
    /// 'a' key (HID usage 0x04) with no modifiers.
    #[test]
    fn keyboard_report_matches_golden_vector() {
        let packet = keyboard_report(0x00, [0x04, 0x00, 0x00, 0x00, 0x00, 0x00]);
        assert_eq!(
            packet,
            vec![0x57, 0xAB, 0x00, 0x02, 0x08, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10]
        );
    }

    #[test]
    fn keyboard_report_all_zero_checksum() {
        let packet = keyboard_report(0x00, [0; 6]);
        assert_eq!(packet, vec![0x57, 0xAB, 0x00, 0x02, 0x08, 0, 0, 0, 0, 0, 0, 0, 0, 0x0C]);
    }

    #[test]
    fn keyboard_report_six_keys_and_all_modifiers() {
        let packet = keyboard_report(0xFF, [4, 5, 6, 7, 8, 9]);
        assert_eq!(packet.len(), 14);
        assert_eq!(&packet[0..5], &[0x57, 0xAB, 0x00, 0x02, 0x08]);
        assert_eq!(&packet[5..13], &[0xFF, 0x00, 4, 5, 6, 7, 8, 9]);
        assert_eq!(*packet.last().unwrap(), naive_checksum(&packet[..packet.len() - 1]));
    }

    #[test]
    fn mouse_absolute_scales_and_frames_correctly() {
        let packet = mouse_absolute(0x01, 0.5, 1.0, -1);
        assert_eq!(packet.len(), 13);
        assert_eq!(&packet[0..5], &[0x57, 0xAB, 0x00, 0x04, 0x07]);
        // x = 0.5 * 4096 = 2048 = 0x0800 (LE: 00 08); y = 1.0 * 4096 = 4096 = 0x1000 (LE: 00 10)
        assert_eq!(&packet[5..12], &[0x02, 0x01, 0x00, 0x08, 0x00, 0x10, 0xFF]);
        assert_eq!(*packet.last().unwrap(), naive_checksum(&packet[..packet.len() - 1]));
    }

    #[test]
    fn mouse_absolute_clamps_out_of_range_fractions() {
        let low = mouse_absolute(0, -0.5, -0.5, 0);
        assert_eq!(&low[7..11], &[0x00, 0x00, 0x00, 0x00]);
        let high = mouse_absolute(0, 1.5, 1.5, 0);
        assert_eq!(&high[7..11], &[0x00, 0x10, 0x00, 0x10]);
    }

    #[test]
    fn mouse_relative_frames_signed_deltas() {
        let packet = mouse_relative(0x02, -5, 10, 3);
        assert_eq!(packet.len(), 11);
        assert_eq!(&packet[0..5], &[0x57, 0xAB, 0x00, 0x05, 0x05]);
        assert_eq!(&packet[5..10], &[0x01, 0x02, 0xFB, 10, 3]);
        assert_eq!(*packet.last().unwrap(), naive_checksum(&packet[..packet.len() - 1]));
    }
}
