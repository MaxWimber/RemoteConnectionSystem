//! Messages between `rcs-web` and `rcs-agent` over `\\.\pipe\rcs-agent`, see `docs/ARCHITECTURE.md`.
//!
//! Each message is JSON with its length in front: a little-endian `u32` byte count, then the bytes.

use std::io::{Read, Write};

use serde::{Deserialize, Serialize};

/// The pipe the agent listens on.
pub const PIPE_NAME: &str = r"\\.\pipe\rcs-agent";
/// Upper bound of one message; an SDP offer is a few kilobytes.
pub const MAX_MESSAGE: u32 = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Starts the media session for a browser. A new session replaces the previous one.
    OpenSession {
        session_id: String,
        sdp_offer: String,
    },
    CloseSession {
        session_id: String,
    },
    Status,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    SessionOpened {
        session_id: String,
        sdp_answer: String,
    },
    SessionClosed {
        session_id: String,
    },
    /// Boxed: it is far bigger than the other replies. Serde writes it the same as unboxed.
    AgentStatus(Box<AgentStatus>),
    Error {
        code: String,
        message: String,
    },
}

/// What the agent reports for the status panel and the diagnostics.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentStatus {
    pub build: String,
    pub connected: bool,
    /// `amf` or `mf`, the backend actually in use, or empty before the first session.
    pub encoder: String,
    pub adapter: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u64,
    pub sessions: u64,
    pub frames_encoded: u64,
    pub frames_sent: u64,
    pub bytes_sent: u64,
    pub frames_dropped_queue: u64,
    pub keyframe_requests: u64,
    /// Datagrams held back by the agent's `--simulate-loss` test option.
    #[serde(default)]
    pub datagrams_dropped_test: u64,
    pub pipeline_restarts: u64,
    pub last_error: Option<String>,
    /// `record` or `sendinput`, the agent's `--input` (D-022).
    #[serde(default)]
    pub input: String,
    /// Input events handed to Windows, or only counted in `record`.
    #[serde(default)]
    pub input_events: u64,
    /// Unknown or malformed input messages.
    #[serde(default)]
    pub input_rejected: u64,
    /// Dropped: the queue was full, or the message was from an ended session or had an old `seq`.
    #[serde(default)]
    pub input_dropped: u64,
    /// Events Windows did not take, for example while an elevated window is in front.
    #[serde(default)]
    pub input_errors: u64,
    #[serde(default)]
    pub input_releases: u64,
    /// `vigem`, `record`, or why there are no virtual gamepads (phase 5).
    #[serde(default)]
    pub gamepad: String,
    #[serde(default)]
    pub gamepad_events: u64,
}

#[derive(Debug)]
pub enum FrameError {
    Io(std::io::Error),
    TooLarge(u32),
    Json(serde_json::Error),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "pipe: {e}"),
            Self::TooLarge(n) => write!(f, "message of {n} bytes is over the limit"),
            Self::Json(e) => write!(f, "bad message: {e}"),
        }
    }
}

impl std::error::Error for FrameError {}

/// Encodes a message with its length in front.
pub fn encode<T: Serialize>(message: &T) -> Result<Vec<u8>, FrameError> {
    let body = serde_json::to_vec(message).map_err(FrameError::Json)?;
    let len = u32::try_from(body.len()).map_err(|_| FrameError::TooLarge(u32::MAX))?;
    if len > MAX_MESSAGE {
        return Err(FrameError::TooLarge(len));
    }
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Checks a length prefix before the body is read.
pub fn body_len(prefix: [u8; 4]) -> Result<usize, FrameError> {
    let len = u32::from_le_bytes(prefix);
    if len > MAX_MESSAGE {
        return Err(FrameError::TooLarge(len));
    }
    Ok(len as usize)
}

pub fn decode<T: for<'de> Deserialize<'de>>(body: &[u8]) -> Result<T, FrameError> {
    serde_json::from_slice(body).map_err(FrameError::Json)
}

/// Writes one message to a blocking stream.
pub fn write_message<T: Serialize>(out: &mut impl Write, message: &T) -> Result<(), FrameError> {
    out.write_all(&encode(message)?).map_err(FrameError::Io)?;
    out.flush().map_err(FrameError::Io)
}

/// Reads one message from a blocking stream; `Ok(None)` at a clean end of stream.
pub fn read_message<T: for<'de> Deserialize<'de>>(
    input: &mut impl Read,
) -> Result<Option<T>, FrameError> {
    let mut prefix = [0u8; 4];
    match input.read_exact(&mut prefix) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(FrameError::Io(e)),
    }
    let mut body = vec![0u8; body_len(prefix)?];
    input.read_exact(&mut body).map_err(FrameError::Io)?;
    decode(&body).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip() {
        let request = Request::OpenSession {
            session_id: "s1".into(),
            sdp_offer: "v=0".into(),
        };
        let bytes = encode(&request).unwrap();
        let mut input = bytes.as_slice();
        let back: Request = read_message(&mut input).unwrap().unwrap();
        assert_eq!(back, request);
        let none: Option<Request> = read_message(&mut input).unwrap();
        assert!(none.is_none());
    }

    #[test]
    fn json_uses_snake_case_tags() {
        let text = String::from_utf8(encode(&Request::Status).unwrap()[4..].to_vec()).unwrap();
        assert_eq!(text, r#"{"type":"status"}"#);
    }

    #[test]
    fn oversized_messages_are_refused() {
        let prefix = (MAX_MESSAGE + 1).to_le_bytes();
        assert!(matches!(body_len(prefix), Err(FrameError::TooLarge(_))));
    }
}
