//! Wire formats for the two data channels: small binary messages on the
//! unreliable/unordered `input` channel for high-frequency mouse/key
//! events, and JSON objects on the reliable `control` channel for
//! low-frequency settings changes and paste submissions.

use serde::{Deserialize, Serialize};

/// Parsed from an `input` data channel message. Loss-tolerant by design —
/// the next event supersedes a dropped one for mouse moves, and a dropped
/// key event is rare enough on a LAN not to design around further.
#[derive(Debug, Clone, PartialEq)]
pub enum InputEvent {
    /// A physical key's press/release state changed. `code` is a
    /// `KeyboardEvent.code` value (e.g. `"KeyA"`); translated via
    /// `ch9329::keymap`.
    KeyEvent { pressed: bool, code: String },
    /// Absolute cursor position as a fraction of the video frame.
    MouseAbsoluteMove { x_frac: f32, y_frac: f32 },
    /// A full relative-mode report: move plus button/wheel state.
    MouseRelativeMove { buttons: u8, dx: i8, dy: i8, wheel: i8 },
    /// Click/scroll state without moving the cursor — used for absolute
    /// mode, where the hardware only honors position, not buttons/wheel,
    /// in its own absolute report (see `ch9329::writer`).
    MouseButtons { buttons: u8, wheel: i8 },
}

const TAG_KEY_EVENT: u8 = 0x01;
const TAG_MOUSE_ABSOLUTE_MOVE: u8 = 0x02;
const TAG_MOUSE_RELATIVE_MOVE: u8 = 0x03;
const TAG_MOUSE_BUTTONS: u8 = 0x04;

impl InputEvent {
    pub fn parse(data: &[u8]) -> Option<Self> {
        let (&tag, rest) = data.split_first()?;
        match tag {
            TAG_KEY_EVENT if !rest.is_empty() => Some(InputEvent::KeyEvent {
                pressed: rest[0] != 0,
                code: String::from_utf8_lossy(&rest[1..]).into_owned(),
            }),
            TAG_MOUSE_ABSOLUTE_MOVE if rest.len() == 8 => Some(InputEvent::MouseAbsoluteMove {
                x_frac: f32::from_le_bytes(rest[0..4].try_into().ok()?),
                y_frac: f32::from_le_bytes(rest[4..8].try_into().ok()?),
            }),
            TAG_MOUSE_RELATIVE_MOVE if rest.len() == 4 => Some(InputEvent::MouseRelativeMove {
                buttons: rest[0],
                dx: rest[1] as i8,
                dy: rest[2] as i8,
                wheel: rest[3] as i8,
            }),
            TAG_MOUSE_BUTTONS if rest.len() == 2 => Some(InputEvent::MouseButtons { buttons: rest[0], wheel: rest[1] as i8 }),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CaptureSettingsWire {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlMessage {
    /// Applies and persists whichever of capture settings / mouse mode the
    /// page included, sent once when the page's Save button is clicked
    /// (see `assets/web/app.js`) — dropdowns no longer apply live on their
    /// own. Each half is only present if the corresponding hardware
    /// (capture card / CH9329) is actually connected — the page has
    /// nothing meaningful to save for a device that isn't there.
    UpdateSettings {
        #[serde(default)]
        capture: Option<CaptureSettingsWire>,
        #[serde(default)]
        mouse_mode: Option<MouseModeWire>,
    },
    Paste { text: String },
    /// The browser's SDP answer to a `ServerMessage::Offer` — the second
    /// half of a renegotiation round trip started by
    /// `rtc::Handler::on_negotiation_needed` (see `session::handle`).
    Answer { sdp: String },
}

/// Server-to-client messages, pushed down the `control` data channel
/// whenever server-side state changes that the page can't otherwise learn
/// about without a reload (see `session::handle`'s device-state and
/// settings arms).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    DeviceState(crate::config::DeviceState),
    /// Whether the CH9329 is plugged in right now — the HID counterpart of
    /// `DeviceState` (which only covers the capture card).
    HidState { available: bool },
    Settings { capture: crate::config::CaptureSettings, mouse_mode: crate::config::MouseMode },
    /// A fresh SDP offer starting a second (or later) round of
    /// negotiation, pushed whenever `rtc::Handler::on_negotiation_needed`
    /// fires after the initial connection is already up — e.g. the
    /// server just added or removed this session's video track. The
    /// browser applies it and replies with `ControlMessage::Answer` over
    /// this same channel.
    Offer { sdp: String },
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseModeWire {
    Absolute,
    Relative,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_key_event() {
        let mut data = vec![TAG_KEY_EVENT, 1];
        data.extend_from_slice(b"KeyA");
        assert_eq!(InputEvent::parse(&data), Some(InputEvent::KeyEvent { pressed: true, code: "KeyA".into() }));
    }

    #[test]
    fn parses_mouse_absolute_move() {
        let mut data = vec![TAG_MOUSE_ABSOLUTE_MOVE];
        data.extend_from_slice(&0.25f32.to_le_bytes());
        data.extend_from_slice(&0.75f32.to_le_bytes());
        assert_eq!(InputEvent::parse(&data), Some(InputEvent::MouseAbsoluteMove { x_frac: 0.25, y_frac: 0.75 }));
    }

    #[test]
    fn parses_mouse_relative_move_with_signed_deltas() {
        let data = vec![TAG_MOUSE_RELATIVE_MOVE, 0x01, (-5i8) as u8, 10, (-3i8) as u8];
        assert_eq!(InputEvent::parse(&data), Some(InputEvent::MouseRelativeMove { buttons: 1, dx: -5, dy: 10, wheel: -3 }));
    }

    #[test]
    fn parses_mouse_buttons() {
        let data = vec![TAG_MOUSE_BUTTONS, 0x02, (-1i8) as u8];
        assert_eq!(InputEvent::parse(&data), Some(InputEvent::MouseButtons { buttons: 2, wheel: -1 }));
    }

    #[test]
    fn rejects_malformed_or_unknown_datagrams() {
        assert_eq!(InputEvent::parse(&[]), None);
        assert_eq!(InputEvent::parse(&[0xFF, 1, 2, 3]), None);
        assert_eq!(InputEvent::parse(&[TAG_MOUSE_ABSOLUTE_MOVE, 1, 2, 3]), None);
    }

    #[test]
    fn control_message_deserializes_from_json() {
        let msg: ControlMessage =
            serde_json::from_str(r#"{"type":"update_settings","capture":{"width":1920,"height":1080,"fps":10},"mouse_mode":"relative"}"#).unwrap();
        let ControlMessage::UpdateSettings { capture, mouse_mode } = msg else { panic!("expected UpdateSettings") };
        let capture = capture.unwrap();
        assert_eq!((capture.width, capture.height, capture.fps), (1920, 1080, 10));
        assert!(matches!(mouse_mode, Some(MouseModeWire::Relative)));

        let msg: ControlMessage = serde_json::from_str(r#"{"type":"paste","text":"hi"}"#).unwrap();
        assert!(matches!(msg, ControlMessage::Paste { text } if text == "hi"));

        let msg: ControlMessage = serde_json::from_str(r#"{"type":"answer","sdp":"v=0..."}"#).unwrap();
        assert!(matches!(msg, ControlMessage::Answer { sdp } if sdp == "v=0..."));
    }

    #[test]
    fn server_message_offer_serializes_to_json() {
        let msg = ServerMessage::Offer { sdp: "v=0...".to_string() };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"type":"offer","sdp":"v=0..."}"#);
    }

    #[test]
    fn update_settings_allows_capture_and_mouse_mode_to_be_omitted_independently() {
        let msg: ControlMessage = serde_json::from_str(r#"{"type":"update_settings","mouse_mode":"absolute"}"#).unwrap();
        assert!(matches!(msg, ControlMessage::UpdateSettings { capture: None, mouse_mode: Some(MouseModeWire::Absolute) }));

        let msg: ControlMessage = serde_json::from_str(r#"{"type":"update_settings","capture":{"width":1280,"height":720,"fps":5}}"#).unwrap();
        assert!(matches!(msg, ControlMessage::UpdateSettings { capture: Some(_), mouse_mode: None }));

        let msg: ControlMessage = serde_json::from_str(r#"{"type":"update_settings"}"#).unwrap();
        assert!(matches!(msg, ControlMessage::UpdateSettings { capture: None, mouse_mode: None }));
    }
}
