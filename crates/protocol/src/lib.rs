// SPDX-License-Identifier: Apache-2.0

//! Wire format between the host daemon and the Android app.
//!
//! One stream header at connect, then a frame header + Annex-B payload per
//! access unit, then a stream of 9-byte reverse messages:
//!
//! ```text
//! StreamHeader (16 bytes)
//!   0..4   magic  "MRLD"
//!   4..6   version          u16
//!   6..8   width            u16
//!   8..10  height           u16
//!  10..12  framerate        u16
//!     12   codec            u8
//!  13..16  reserved
//!
//! FrameHeader (16 bytes), repeated
//!   0..4   payload length   u32
//!   4..12  pts, nanoseconds u64
//!     12   flags            u8   bit 0 = keyframe, bit 1 = control
//!  13..16  reserved
//!
//! ReverseMessage (9 bytes), repeated, device -> host
//!      0   type             u8   MSG_ACK | MSG_TOUCH
//!   1..9   payload          8 bytes, type-dependent
//! ```
//!
//! Every multi-byte field is **big-endian**. That is deliberate: Kotlin's
//! `DataInputStream` reads big-endian natively, so the device side needs no
//! byte-swapping code at all.
//!
//! The reverse direction uses a leading type byte rather than a full
//! `FrameHeader`. Both message types are fixed-size and small; a length
//! field would be larger than the payload it describes, and the ack path
//! fires once per frame.

use anyhow::{bail, Result};

pub const MAGIC: [u8; 4] = *b"MRLD";
/// Bumped to 3 by the touchscreen change.
///
/// v2 -> v3 is a wire-format break: the device -> host direction changes from
/// a bare 8-byte ack to a 9-byte `[type][payload]` message. The host rejects
/// v2 with a clear error, so a mismatched pair fails loudly on the stream
/// header rather than corrupting the reverse channel.
pub const VERSION: u16 = 3;
pub const STREAM_HEADER_LEN: usize = 16;
pub const FRAME_HEADER_LEN: usize = 16;

/// Length of every device -> host message. One type byte plus eight bytes of
/// payload. Independent of the message type: both are fixed-size.
pub const REVERSE_MSG_LEN: usize = 9;

/// Reverse-message type discriminators.
pub const MSG_ACK: u8 = 0x01;
pub const MSG_TOUCH: u8 = 0x02;

/// Base of the host-side port range the daemon claims for `adb forward`.
pub const BASE_PORT: u16 = 27183;

/// Alias for [`BASE_PORT`].
pub const DEFAULT_PORT: u16 = BASE_PORT;

/// Number of consecutive host-side ports the daemon will claim.
pub const PORT_RANGE: u16 = 100;
/// Abstract Unix socket the device app listens on.
pub const SOCKET_NAME: &str = "moreland";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Codec {
    H264 = 0,
    H265 = 1,
}

impl Codec {
    pub fn from_u8(value: u8) -> Result<Self> {
        Ok(match value {
            0 => Codec::H264,
            1 => Codec::H265,
            other => bail!("unknown codec id {other}"),
        })
    }

    /// MIME type MediaCodec expects.
    pub fn mime(self) -> &'static str {
        match self {
            Codec::H264 => "video/avc",
            Codec::H265 => "video/hevc",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct StreamHeader {
    pub width: u16,
    pub height: u16,
    pub framerate: u16,
    pub codec: Codec,
}

impl StreamHeader {
    pub fn encode(&self) -> [u8; STREAM_HEADER_LEN] {
        let mut buf = [0u8; STREAM_HEADER_LEN];
        buf[0..4].copy_from_slice(&MAGIC);
        buf[4..6].copy_from_slice(&VERSION.to_be_bytes());
        buf[6..8].copy_from_slice(&self.width.to_be_bytes());
        buf[8..10].copy_from_slice(&self.height.to_be_bytes());
        buf[10..12].copy_from_slice(&self.framerate.to_be_bytes());
        buf[12] = self.codec as u8;
        buf
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() < STREAM_HEADER_LEN {
            bail!("stream header too short: {} bytes", buf.len());
        }
        if buf[0..4] != MAGIC {
            bail!("bad magic: {:02x?}", &buf[0..4]);
        }
        let version = u16::from_be_bytes([buf[4], buf[5]]);
        if version != VERSION {
            bail!("unsupported protocol version {version} (expected {VERSION})");
        }
        Ok(Self {
            width: u16::from_be_bytes([buf[6], buf[7]]),
            height: u16::from_be_bytes([buf[8], buf[9]]),
            framerate: u16::from_be_bytes([buf[10], buf[11]]),
            codec: Codec::from_u8(buf[12])?,
        })
    }
}

pub const FLAG_KEYFRAME: u8 = 1 << 0;
/// Frame payload is a [`ControlMessage`], not video. Host -> device only.
pub const FLAG_CONTROL: u8 = 1 << 1;

#[derive(Debug, Clone, Copy)]
pub struct FrameHeader {
    pub length: u32,
    pub pts_ns: u64,
    pub keyframe: bool,
    pub control: bool,
}

impl FrameHeader {
    pub fn encode(&self) -> [u8; FRAME_HEADER_LEN] {
        let mut buf = [0u8; FRAME_HEADER_LEN];
        buf[0..4].copy_from_slice(&self.length.to_be_bytes());
        buf[4..12].copy_from_slice(&self.pts_ns.to_be_bytes());
        let mut flags = 0u8;
        if self.keyframe {
            flags |= FLAG_KEYFRAME;
        }
        if self.control {
            flags |= FLAG_CONTROL;
        }
        buf[12] = flags;
        buf
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() < FRAME_HEADER_LEN {
            bail!("frame header too short: {} bytes", buf.len());
        }
        let flags = buf[12];
        Ok(Self {
            length: u32::from_be_bytes(buf[0..4].try_into().unwrap()),
            pts_ns: u64::from_be_bytes(buf[4..12].try_into().unwrap()),
            keyframe: flags & FLAG_KEYFRAME != 0,
            control: flags & FLAG_CONTROL != 0,
        })
    }
}

// ---------------------------------------------------------------- controls ---

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ControlKind {
    Brightness = 1,
    Rotation = 2,
}

impl ControlKind {
    pub fn from_u8(value: u8) -> Result<Self> {
        Ok(match value {
            1 => ControlKind::Brightness,
            2 => ControlKind::Rotation,
            other => bail!("unknown control kind {other}"),
        })
    }
}

/// A host -> device control message carried in a frame-shaped envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlMessage {
    pub kind: ControlKind,
    pub value: u8,
}

impl ControlMessage {
    pub const LEN: usize = 2;

    /// Brightness is a percentage of full scale. Values above 100 clamp.
    pub fn brightness(percent: u8) -> Self {
        Self { kind: ControlKind::Brightness, value: percent.min(100) }
    }

    /// `degrees` is one of 0, 90, 180, 270.
    pub fn rotation(degrees: u16) -> Result<Self> {
        let value = match degrees {
            0 => 0u8,
            90 => 1,
            180 => 2,
            270 => 3,
            other => bail!("rotation must be 0, 90, 180, or 270 (got {other})"),
        };
        Ok(Self { kind: ControlKind::Rotation, value })
    }

    pub fn rotation_degrees(&self) -> u16 {
        match self.value {
            0 => 0,
            1 => 90,
            2 => 180,
            3 => 270,
            _ => 0,
        }
    }

    pub fn encode(&self) -> [u8; Self::LEN] {
        [self.kind as u8, self.value]
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() < Self::LEN {
            bail!("control message too short: {} bytes", buf.len());
        }
        Ok(Self {
            kind: ControlKind::from_u8(buf[0])?,
            value: buf[1],
        })
    }
}

// ---------------------------------------------------------- reverse channel ---

/// Single-contact touch action. The four codes are exhaustive for v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TouchAction {
    Down = 0,
    Move = 1,
    Up = 2,
    Cancel = 3,
}

impl TouchAction {
    pub fn from_u8(value: u8) -> Result<Self> {
        Ok(match value {
            0 => TouchAction::Down,
            1 => TouchAction::Move,
            2 => TouchAction::Up,
            3 => TouchAction::Cancel,
            other => bail!("unknown touch action {other}"),
        })
    }
}

/// One device -> host touch event. Single contact point.
///
/// `x` and `y` are fixed-point normalized to `[0..65535]`, representing
/// `[0.0 .. 1.0]` across the tablet's surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TouchMessage {
    pub action: TouchAction,
    pub x: u16,
    pub y: u16,
}

impl TouchMessage {
    /// Length of the *payload*, not the full reverse message.
    pub const LEN: usize = 8;

    /// Build from normalized floats, clamping to `[0.0, 1.0]`.
    pub fn from_normalized(action: TouchAction, x: f32, y: f32) -> Self {
        let to_fixed = |v: f32| -> u16 {
            let v = v.clamp(0.0, 1.0);
            (v * 65535.0 + 0.5) as u16
        };
        Self { action, x: to_fixed(x), y: to_fixed(y) }
    }

    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut buf = [0u8; Self::LEN];
        buf[0] = self.action as u8;
        buf[1] = 0; // reserved
        buf[2..4].copy_from_slice(&self.x.to_be_bytes());
        buf[4..6].copy_from_slice(&self.y.to_be_bytes());
        buf
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        if payload.len() < Self::LEN {
            bail!("touch payload too short: {} bytes", payload.len());
        }
        Ok(Self {
            action: TouchAction::from_u8(payload[0])?,
            x: u16::from_be_bytes([payload[2], payload[3]]),
            y: u16::from_be_bytes([payload[4], payload[5]]),
        })
    }
}

/// Encode an ack into the 9-byte reverse-message format.
pub fn encode_ack(pts_ns: u64) -> [u8; REVERSE_MSG_LEN] {
    let mut buf = [0u8; REVERSE_MSG_LEN];
    buf[0] = MSG_ACK;
    buf[1..9].copy_from_slice(&pts_ns.to_be_bytes());
    buf
}

/// Encode a touch message into the 9-byte reverse-message format.
pub fn encode_touch(msg: &TouchMessage) -> [u8; REVERSE_MSG_LEN] {
    let mut buf = [0u8; REVERSE_MSG_LEN];
    buf[0] = MSG_TOUCH;
    buf[1..9].copy_from_slice(&msg.encode());
    buf
}

/// A parsed reverse message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReverseMessage {
    Ack(u64),
    Touch(TouchMessage),
}

/// Decode a full 9-byte reverse message.
pub fn decode_reverse_message(buf: &[u8]) -> Result<ReverseMessage> {
    if buf.len() < REVERSE_MSG_LEN {
        bail!("reverse message too short: {} bytes", buf.len());
    }
    match buf[0] {
        MSG_ACK => Ok(ReverseMessage::Ack(u64::from_be_bytes(
            buf[1..9].try_into().unwrap(),
        ))),
        MSG_TOUCH => Ok(ReverseMessage::Touch(TouchMessage::decode(&buf[1..9])?)),
        other => bail!("unknown reverse message type {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_header_round_trips() {
        let header = StreamHeader {
            width: 1920,
            height: 1200,
            framerate: 60,
            codec: Codec::H264,
        };
        let decoded = StreamHeader::decode(&header.encode()).unwrap();
        assert_eq!(decoded.width, 1920);
        assert_eq!(decoded.height, 1200);
        assert_eq!(decoded.framerate, 60);
        assert_eq!(decoded.codec, Codec::H264);
    }

    #[test]
    fn stream_header_rejects_v2() {
        let mut buf = StreamHeader {
            width: 1, height: 1, framerate: 60, codec: Codec::H264,
        }.encode();
        buf[4..6].copy_from_slice(&2u16.to_be_bytes());
        let err = StreamHeader::decode(&buf).unwrap_err();
        assert!(err.to_string().contains("version 2"));
        assert!(err.to_string().contains("expected 3"));
    }

    #[test]
    fn frame_header_round_trips() {
        let header = FrameHeader {
            length: 32357, pts_ns: 1_666_666_600, keyframe: true, control: false,
        };
        let decoded = FrameHeader::decode(&header.encode()).unwrap();
        assert_eq!(decoded.length, 32357);
        assert!(decoded.keyframe);
        assert!(!decoded.control);
    }

    #[test]
    fn frame_header_control_flag_round_trips() {
        let header = FrameHeader {
            length: ControlMessage::LEN as u32, pts_ns: 0,
            keyframe: false, control: true,
        };
        let decoded = FrameHeader::decode(&header.encode()).unwrap();
        assert!(!decoded.keyframe);
        assert!(decoded.control);
    }

    #[test]
    fn brightness_message_round_trips() {
        let msg = ControlMessage::brightness(75);
        let decoded = ControlMessage::decode(&msg.encode()).unwrap();
        assert_eq!(decoded.kind, ControlKind::Brightness);
        assert_eq!(decoded.value, 75);
    }

    #[test]
    fn brightness_clamps_at_100() {
        assert_eq!(ControlMessage::brightness(255).value, 100);
    }

    #[test]
    fn rotation_message_round_trips() {
        for (degrees, wire) in [(0u16, 0u8), (90, 1), (180, 2), (270, 3)] {
            let msg = ControlMessage::rotation(degrees).unwrap();
            assert_eq!(msg.value, wire);
            let decoded = ControlMessage::decode(&msg.encode()).unwrap();
            assert_eq!(decoded.rotation_degrees(), degrees);
        }
    }

    #[test]
    fn rejects_unknown_rotation() {
        assert!(ControlMessage::rotation(45).is_err());
        assert!(ControlMessage::rotation(360).is_err());
    }

    #[test]
    fn rejects_unknown_control_kind() {
        assert!(ControlMessage::decode(&[0xFF, 0]).is_err());
        assert!(ControlMessage::decode(&[1]).is_err());
    }

    #[test]
    fn rejects_bad_magic() {
        let mut buf = StreamHeader {
            width: 1, height: 1, framerate: 60, codec: Codec::H264,
        }.encode();
        buf[0] = b'X';
        assert!(StreamHeader::decode(&buf).is_err());
    }

    #[test]
    fn ack_round_trips() {
        let bytes = encode_ack(0x0123_4567_89AB_CDEF);
        assert_eq!(bytes[0], MSG_ACK);
        match decode_reverse_message(&bytes).unwrap() {
            ReverseMessage::Ack(pts) => assert_eq!(pts, 0x0123_4567_89AB_CDEF),
            other => panic!("expected Ack, got {other:?}"),
        }
    }

    #[test]
    fn touch_corners_round_trip() {
        for (x, y) in [(0.0f32, 0.0f32), (0.5, 0.5), (1.0, 1.0)] {
            let msg = TouchMessage::from_normalized(TouchAction::Down, x, y);
            let bytes = encode_touch(&msg);
            assert_eq!(bytes[0], MSG_TOUCH);
            match decode_reverse_message(&bytes).unwrap() {
                ReverseMessage::Touch(decoded) => {
                    assert_eq!(decoded.action, TouchAction::Down);
                    let expected_x = (x * 65535.0 + 0.5) as u16;
                    let expected_y = (y * 65535.0 + 0.5) as u16;
                    assert_eq!(decoded.x, expected_x, "x mismatch for ({x}, {y})");
                    assert_eq!(decoded.y, expected_y, "y mismatch for ({x}, {y})");
                }
                other => panic!("expected Touch, got {other:?}"),
            }
        }
    }

    #[test]
    fn touch_actions_round_trip() {
        for action in [TouchAction::Down, TouchAction::Move, TouchAction::Up, TouchAction::Cancel] {
            let msg = TouchMessage { action, x: 0x1234, y: 0xABCD };
            let bytes = encode_touch(&msg);
            match decode_reverse_message(&bytes).unwrap() {
                ReverseMessage::Touch(decoded) => assert_eq!(decoded, msg),
                other => panic!("expected Touch, got {other:?}"),
            }
        }
    }

    #[test]
    fn from_normalized_clamps_out_of_range() {
        let under = TouchMessage::from_normalized(TouchAction::Move, -1.0, -1.0);
        assert_eq!((under.x, under.y), (0, 0));
        let over = TouchMessage::from_normalized(TouchAction::Move, 2.0, 2.0);
        assert_eq!((over.x, over.y), (65535, 65535));
    }

    #[test]
    fn rejects_unknown_touch_action() {
        let mut payload = [0u8; TouchMessage::LEN];
        payload[0] = 0x7F;
        assert!(TouchMessage::decode(&payload).is_err());
    }

    #[test]
    fn rejects_unknown_reverse_message_type() {
        let mut buf = [0u8; REVERSE_MSG_LEN];
        buf[0] = 0xEE;
        assert!(decode_reverse_message(&buf).is_err());
    }

    #[test]
    fn rejects_short_reverse_message() {
        assert!(decode_reverse_message(&[MSG_ACK; 4]).is_err());
    }
}
