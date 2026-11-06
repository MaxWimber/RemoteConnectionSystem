//! Binary messages of the `input-fast`, `input-reliable` and `control` data channels, `docs/PROTOCOL.md`.
//!
//! Numbers are little-endian, the first byte is the type. [`ClientMessage::parse`] never panics: a message of a
//! known type with a wrong length or a field out of range is [`ParseError::Malformed`], an unknown type is
//! [`ParseError::Unknown`]. Every message it accepts encodes back to the same bytes.

use serde::{Deserialize, Serialize};

pub const MOUSE_MOVE_ABSOLUTE: u8 = 0x01;
pub const MOUSE_MOVE_RELATIVE: u8 = 0x02;
pub const GAMEPAD_STATE: u8 = 0x03;
pub const KEY: u8 = 0x10;
pub const MOUSE_BUTTON: u8 = 0x11;
pub const WHEEL: u8 = 0x12;
pub const TOUCH: u8 = 0x13;
pub const RELEASE_ALL: u8 = 0x14;
pub const TEXT: u8 = 0x15;
pub const SPECIAL: u8 = 0x16;
pub const CURSOR_SHAPE: u8 = 0x20;
pub const CURSOR_STATE: u8 = 0x21;

/// Most touch points in one `Touch` message.
pub const MAX_TOUCH_POINTS: usize = 10;
/// Longest `Text`, in UTF-8 bytes.
pub const MAX_TEXT_BYTES: usize = 4096;
/// Largest `CursorShape` message, header included.
pub const MAX_CURSOR_SHAPE_BYTES: usize = 64 * 1024;
const CURSOR_SHAPE_HEADER: usize = 11;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    Empty,
    /// A type this version does not know. Ignored and counted.
    Unknown(u8),
    /// A known type with a wrong length or a field out of range. Dropped.
    Malformed(u8),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "empty message"),
            Self::Unknown(t) => write!(f, "unknown message type 0x{t:02x}"),
            Self::Malformed(t) => write!(f, "malformed message of type 0x{t:02x}"),
        }
    }
}

impl std::error::Error for ParseError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

impl MouseButton {
    pub const ALL: [Self; 5] = [
        Self::Left,
        Self::Right,
        Self::Middle,
        Self::Back,
        Self::Forward,
    ];

    pub fn from_wire(v: u8) -> Option<Self> {
        Self::ALL.get(usize::from(v)).copied()
    }

    pub fn wire(self) -> u8 {
        self as u8
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TouchPhase {
    Down,
    Move,
    Up,
    Cancel,
}

impl TouchPhase {
    pub fn from_wire(v: u8) -> Option<Self> {
        [Self::Down, Self::Move, Self::Up, Self::Cancel]
            .get(usize::from(v))
            .copied()
    }

    pub fn wire(self) -> u8 {
        self as u8
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TouchPoint {
    pub id: u8,
    pub phase: TouchPhase,
    pub x: u16,
    pub y: u16,
}

/// Up to [`MAX_TOUCH_POINTS`] points without a heap allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TouchPoints {
    len: u8,
    points: [TouchPoint; MAX_TOUCH_POINTS],
}

impl TouchPoints {
    const EMPTY: TouchPoint = TouchPoint {
        id: 0,
        phase: TouchPhase::Down,
        x: 0,
        y: 0,
    };

    /// None when there are no points or more than [`MAX_TOUCH_POINTS`].
    pub fn new(list: &[TouchPoint]) -> Option<Self> {
        if list.is_empty() || list.len() > MAX_TOUCH_POINTS {
            return None;
        }
        let mut points = [Self::EMPTY; MAX_TOUCH_POINTS];
        points[..list.len()].copy_from_slice(list);
        Some(Self {
            len: list.len() as u8,
            points,
        })
    }

    pub fn as_slice(&self) -> &[TouchPoint] {
        &self.points[..usize::from(self.len)]
    }
}

impl Serialize for TouchPoints {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.as_slice().serialize(s)
    }
}

impl<'de> Deserialize<'de> for TouchPoints {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let list = Vec::<TouchPoint>::deserialize(d)?;
        Self::new(&list).ok_or_else(|| serde::de::Error::custom("1 to 10 touch points"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GamepadState {
    pub seq: u16,
    pub pad: u8,
    pub buttons: u16,
    pub left_trigger: u8,
    pub right_trigger: u8,
    pub left_x: i16,
    pub left_y: i16,
    pub right_x: i16,
    pub right_y: i16,
}

/// Browser to server, on `input-fast` and `input-reliable`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Normalized over the video area, 0..65535.
    MouseMoveAbsolute {
        seq: u16,
        x: u16,
        y: u16,
    },
    /// Sums of dx and dy since the session started: a lost packet loses no movement.
    MouseMoveRelative {
        seq: u16,
        dx: i32,
        dy: i32,
    },
    GamepadState(GamepadState),
    /// PS/2 Set 1 scan code, extended keys with 0xE0 in the high byte.
    Key {
        down: bool,
        scancode: u16,
    },
    MouseButton {
        button: MouseButton,
        down: bool,
    },
    /// 120 units are one wheel step.
    Wheel {
        dx: i16,
        dy: i16,
    },
    Touch {
        points: TouchPoints,
    },
    ReleaseAll,
    Text {
        text: String,
    },
    /// 1 Ctrl+Alt+Del (from phase 6), 2 next monitor.
    Special {
        code: u8,
    },
}

/// Special codes known to this version.
pub const SPECIAL_CTRL_ALT_DEL: u8 = 1;
pub const SPECIAL_NEXT_MONITOR: u8 = 2;

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn i16_at(b: &[u8], at: usize) -> i16 {
    i16::from_le_bytes([b[at], b[at + 1]])
}

fn i32_at(b: &[u8], at: usize) -> i32 {
    i32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn bool_at(b: &[u8], at: usize) -> Option<bool> {
    match b[at] {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}

impl ClientMessage {
    pub fn parse(b: &[u8]) -> Result<Self, ParseError> {
        let &kind = b.first().ok_or(ParseError::Empty)?;
        let bad = ParseError::Malformed(kind);
        let exact = |n: usize| if b.len() == n { Ok(()) } else { Err(bad) };
        Ok(match kind {
            MOUSE_MOVE_ABSOLUTE => {
                exact(7)?;
                Self::MouseMoveAbsolute {
                    seq: u16_at(b, 1),
                    x: u16_at(b, 3),
                    y: u16_at(b, 5),
                }
            }
            MOUSE_MOVE_RELATIVE => {
                exact(11)?;
                Self::MouseMoveRelative {
                    seq: u16_at(b, 1),
                    dx: i32_at(b, 3),
                    dy: i32_at(b, 7),
                }
            }
            GAMEPAD_STATE => {
                exact(16)?;
                let pad = b[3];
                if pad > 3 {
                    return Err(bad);
                }
                Self::GamepadState(GamepadState {
                    seq: u16_at(b, 1),
                    pad,
                    buttons: u16_at(b, 4),
                    left_trigger: b[6],
                    right_trigger: b[7],
                    left_x: i16_at(b, 8),
                    left_y: i16_at(b, 10),
                    right_x: i16_at(b, 12),
                    right_y: i16_at(b, 14),
                })
            }
            KEY => {
                exact(4)?;
                // Bits other than 0 are reserved.
                let down = bool_at(b, 1).ok_or(bad)?;
                Self::Key {
                    down,
                    scancode: u16_at(b, 2),
                }
            }
            MOUSE_BUTTON => {
                exact(3)?;
                Self::MouseButton {
                    button: MouseButton::from_wire(b[1]).ok_or(bad)?,
                    down: bool_at(b, 2).ok_or(bad)?,
                }
            }
            WHEEL => {
                exact(5)?;
                Self::Wheel {
                    dx: i16_at(b, 1),
                    dy: i16_at(b, 3),
                }
            }
            TOUCH => {
                let count = usize::from(*b.get(1).ok_or(bad)?);
                if count == 0 || count > MAX_TOUCH_POINTS {
                    return Err(bad);
                }
                exact(2 + 6 * count)?;
                let mut list = [TouchPoints::EMPTY; MAX_TOUCH_POINTS];
                for (i, p) in list.iter_mut().take(count).enumerate() {
                    let at = 2 + 6 * i;
                    *p = TouchPoint {
                        id: b[at],
                        phase: TouchPhase::from_wire(b[at + 1]).ok_or(bad)?,
                        x: u16_at(b, at + 2),
                        y: u16_at(b, at + 4),
                    };
                }
                Self::Touch {
                    points: TouchPoints {
                        len: count as u8,
                        points: list,
                    },
                }
            }
            RELEASE_ALL => {
                exact(1)?;
                Self::ReleaseAll
            }
            TEXT => {
                if b.len() < 3 {
                    return Err(bad);
                }
                let len = usize::from(u16_at(b, 1));
                if len > MAX_TEXT_BYTES {
                    return Err(bad);
                }
                exact(3 + len)?;
                let text = std::str::from_utf8(&b[3..]).map_err(|_| bad)?;
                Self::Text { text: text.into() }
            }
            SPECIAL => {
                exact(2)?;
                Self::Special { code: b[1] }
            }
            other => return Err(ParseError::Unknown(other)),
        })
    }

    /// Appends the message to `out`. Values that cannot be sent (no touch points, text over the limit) are the
    /// caller's mistake; text is cut at a character boundary within [`MAX_TEXT_BYTES`].
    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Self::MouseMoveAbsolute { seq, x, y } => {
                out.push(MOUSE_MOVE_ABSOLUTE);
                out.extend_from_slice(&seq.to_le_bytes());
                out.extend_from_slice(&x.to_le_bytes());
                out.extend_from_slice(&y.to_le_bytes());
            }
            Self::MouseMoveRelative { seq, dx, dy } => {
                out.push(MOUSE_MOVE_RELATIVE);
                out.extend_from_slice(&seq.to_le_bytes());
                out.extend_from_slice(&dx.to_le_bytes());
                out.extend_from_slice(&dy.to_le_bytes());
            }
            Self::GamepadState(g) => {
                out.push(GAMEPAD_STATE);
                out.extend_from_slice(&g.seq.to_le_bytes());
                out.push(g.pad);
                out.extend_from_slice(&g.buttons.to_le_bytes());
                out.push(g.left_trigger);
                out.push(g.right_trigger);
                for v in [g.left_x, g.left_y, g.right_x, g.right_y] {
                    out.extend_from_slice(&v.to_le_bytes());
                }
            }
            Self::Key { down, scancode } => {
                out.push(KEY);
                out.push(u8::from(*down));
                out.extend_from_slice(&scancode.to_le_bytes());
            }
            Self::MouseButton { button, down } => {
                out.extend_from_slice(&[MOUSE_BUTTON, button.wire(), u8::from(*down)]);
            }
            Self::Wheel { dx, dy } => {
                out.push(WHEEL);
                out.extend_from_slice(&dx.to_le_bytes());
                out.extend_from_slice(&dy.to_le_bytes());
            }
            Self::Touch { points } => {
                out.push(TOUCH);
                out.push(points.len);
                for p in points.as_slice() {
                    out.push(p.id);
                    out.push(p.phase.wire());
                    out.extend_from_slice(&p.x.to_le_bytes());
                    out.extend_from_slice(&p.y.to_le_bytes());
                }
            }
            Self::ReleaseAll => out.push(RELEASE_ALL),
            Self::Text { text } => {
                let mut end = text.len().min(MAX_TEXT_BYTES);
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                out.push(TEXT);
                out.extend_from_slice(&(end as u16).to_le_bytes());
                out.extend_from_slice(&text.as_bytes()[..end]);
            }
            Self::Special { code } => out.extend_from_slice(&[SPECIAL, *code]),
        }
    }

    /// The `seq` stream this message belongs to, if it has one: moves and each gamepad have their own.
    pub fn seq_stream(&self) -> Option<(SeqStream, u16)> {
        match self {
            Self::MouseMoveAbsolute { seq, .. } => Some((SeqStream::MouseAbsolute, *seq)),
            Self::MouseMoveRelative { seq, .. } => Some((SeqStream::MouseRelative, *seq)),
            Self::GamepadState(g) => Some((SeqStream::Gamepad(g.pad), g.seq)),
            _ => None,
        }
    }
}

/// Server to browser: the cursor, `CursorState` on `input-fast` and `CursorShape` on `control`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    CursorState {
        seq: u16,
        visible: bool,
        x: u16,
        y: u16,
    },
    CursorShape {
        shape_id: u16,
        hot_x: u16,
        hot_y: u16,
        width: u16,
        height: u16,
        /// PNG bytes, hex in the JSON examples.
        #[serde(with = "hex_bytes")]
        png: Vec<u8>,
    },
}

impl ServerMessage {
    pub fn parse(b: &[u8]) -> Result<Self, ParseError> {
        let &kind = b.first().ok_or(ParseError::Empty)?;
        let bad = ParseError::Malformed(kind);
        Ok(match kind {
            CURSOR_STATE => {
                if b.len() != 8 {
                    return Err(bad);
                }
                Self::CursorState {
                    seq: u16_at(b, 1),
                    visible: bool_at(b, 3).ok_or(bad)?,
                    x: u16_at(b, 4),
                    y: u16_at(b, 6),
                }
            }
            CURSOR_SHAPE => {
                if b.len() <= CURSOR_SHAPE_HEADER || b.len() > MAX_CURSOR_SHAPE_BYTES {
                    return Err(bad);
                }
                Self::CursorShape {
                    shape_id: u16_at(b, 1),
                    hot_x: u16_at(b, 3),
                    hot_y: u16_at(b, 5),
                    width: u16_at(b, 7),
                    height: u16_at(b, 9),
                    png: b[CURSOR_SHAPE_HEADER..].to_vec(),
                }
            }
            other => return Err(ParseError::Unknown(other)),
        })
    }

    /// Appends the message to `out`. A `CursorShape` over [`MAX_CURSOR_SHAPE_BYTES`] returns false and writes
    /// nothing: the sender must make the picture smaller.
    pub fn encode(&self, out: &mut Vec<u8>) -> bool {
        match self {
            Self::CursorState { seq, visible, x, y } => {
                out.push(CURSOR_STATE);
                out.extend_from_slice(&seq.to_le_bytes());
                out.push(u8::from(*visible));
                out.extend_from_slice(&x.to_le_bytes());
                out.extend_from_slice(&y.to_le_bytes());
                true
            }
            Self::CursorShape {
                shape_id,
                hot_x,
                hot_y,
                width,
                height,
                png,
            } => {
                if png.is_empty() || CURSOR_SHAPE_HEADER + png.len() > MAX_CURSOR_SHAPE_BYTES {
                    return false;
                }
                out.push(CURSOR_SHAPE);
                for v in [shape_id, hot_x, hot_y, width, height] {
                    out.extend_from_slice(&v.to_le_bytes());
                }
                out.extend_from_slice(png);
                true
            }
        }
    }
}

/// Which `seq` counter a message uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeqStream {
    MouseAbsolute,
    MouseRelative,
    Gamepad(u8),
}

/// True when `a` comes after `b` in serial number arithmetic (RFC 1982) for 16 bits. Equal numbers and numbers
/// exactly half the range apart are not newer.
pub fn seq_newer(a: u16, b: u16) -> bool {
    let d = a.wrapping_sub(b);
    d != 0 && d < 0x8000
}

/// Drops messages with an old or repeated `seq`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SeqFilter {
    last: Option<u16>,
}

impl SeqFilter {
    /// True when the message should be applied; it then becomes the newest.
    pub fn accept(&mut self, seq: u16) -> bool {
        match self.last {
            Some(last) if !seq_newer(seq, last) => false,
            _ => {
                self.last = Some(seq);
                true
            }
        }
    }

    pub fn reset(&mut self) {
        self.last = None;
    }
}

mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&super::to_hex(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(d)?;
        super::from_hex(&text).ok_or_else(|| serde::de::Error::custom("hex bytes"))
    }
}

pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn from_hex(text: &str) -> Option<Vec<u8>> {
    let text: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests;
