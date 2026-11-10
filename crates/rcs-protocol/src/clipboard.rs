//! Messages of the `clipboard` data channel (D-040, `docs/PROTOCOL.md`): text in parts of up to 16 KiB, at most
//! 1 MiB in all, the same format both ways.

/// `u8 type`, `u32 total`, `u32 offset`, then a part of the UTF-8 text.
pub const CLIPBOARD_TEXT: u8 = 0x30;
/// Server to client: `u8 type`, `u32 total`; the text of that length is now in the Windows clipboard.
pub const CLIPBOARD_APPLIED: u8 = 0x31;
/// Server to client: `u8 type`, `u32 size`; the server's text is over the limit and was not sent.
pub const CLIPBOARD_TOO_LARGE: u8 = 0x32;

/// The largest text, in UTF-8 bytes.
pub const MAX_TEXT: usize = 1 << 20;
/// The largest part of the text in one message: well under the browsers' message limit.
pub const PART: usize = 16 * 1024;
const HEADER: usize = 9;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message<'a> {
    /// A part of a text of `total` bytes, starting at `offset`.
    Text {
        total: u32,
        offset: u32,
        part: &'a [u8],
    },
    Applied {
        total: u32,
    },
    TooLarge {
        size: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardError {
    /// Too short, an unknown type, or a part that does not fit its text.
    Malformed,
    /// Over `MAX_TEXT`.
    TooLarge(usize),
    /// A part that does not follow the one before it.
    OutOfOrder,
    /// The whole text is not UTF-8.
    NotUtf8,
}

impl std::fmt::Display for ClipboardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed => f.write_str("malformed clipboard message"),
            Self::TooLarge(n) => write!(f, "clipboard text of {n} bytes is over the limit"),
            Self::OutOfOrder => f.write_str("clipboard part out of order"),
            Self::NotUtf8 => f.write_str("clipboard text is not UTF-8"),
        }
    }
}

impl std::error::Error for ClipboardError {}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

impl<'a> Message<'a> {
    pub fn parse(b: &'a [u8]) -> Result<Self, ClipboardError> {
        let bad = ClipboardError::Malformed;
        match b.first() {
            Some(&CLIPBOARD_TEXT) if b.len() >= HEADER => {
                let total = u32_at(b, 1);
                let offset = u32_at(b, 5);
                let part = &b[HEADER..];
                if total as usize > MAX_TEXT {
                    return Err(ClipboardError::TooLarge(total as usize));
                }
                if part.len() > PART || offset as usize + part.len() > total as usize {
                    return Err(bad);
                }
                Ok(Self::Text {
                    total,
                    offset,
                    part,
                })
            }
            Some(&CLIPBOARD_APPLIED) if b.len() == 5 => Ok(Self::Applied {
                total: u32_at(b, 1),
            }),
            Some(&CLIPBOARD_TOO_LARGE) if b.len() == 5 => Ok(Self::TooLarge { size: u32_at(b, 1) }),
            _ => Err(bad),
        }
    }
}

/// The messages that carry `text`, in order. An empty text is one message with no part.
pub fn encode_text(text: &[u8]) -> Result<Vec<Vec<u8>>, ClipboardError> {
    if text.len() > MAX_TEXT {
        return Err(ClipboardError::TooLarge(text.len()));
    }
    let total = (text.len() as u32).to_le_bytes();
    let mut out = Vec::with_capacity(text.len() / PART + 1);
    let mut offset = 0;
    loop {
        let end = (offset + PART).min(text.len());
        let mut m = Vec::with_capacity(HEADER + end - offset);
        m.push(CLIPBOARD_TEXT);
        m.extend_from_slice(&total);
        m.extend_from_slice(&(offset as u32).to_le_bytes());
        m.extend_from_slice(&text[offset..end]);
        out.push(m);
        offset = end;
        if offset >= text.len() {
            return Ok(out);
        }
    }
}

pub fn encode_applied(total: u32) -> Vec<u8> {
    let mut m = vec![CLIPBOARD_APPLIED];
    m.extend_from_slice(&total.to_le_bytes());
    m
}

pub fn encode_too_large(size: u32) -> Vec<u8> {
    let mut m = vec![CLIPBOARD_TOO_LARGE];
    m.extend_from_slice(&size.to_le_bytes());
    m
}

/// Puts the parts of one text back together.
#[derive(Debug, Default)]
pub struct Assembler {
    buffer: Vec<u8>,
    total: usize,
}

impl Assembler {
    /// The whole text once its last part arrived. A part at offset 0 starts a new text and drops an unfinished one.
    pub fn add(
        &mut self,
        total: u32,
        offset: u32,
        part: &[u8],
    ) -> Result<Option<String>, ClipboardError> {
        let (total, offset) = (total as usize, offset as usize);
        if total > MAX_TEXT {
            return Err(ClipboardError::TooLarge(total));
        }
        if offset == 0 {
            self.buffer.clear();
            self.total = total;
        } else if total != self.total || offset != self.buffer.len() {
            self.buffer.clear();
            return Err(ClipboardError::OutOfOrder);
        }
        if offset + part.len() > total {
            self.buffer.clear();
            return Err(ClipboardError::Malformed);
        }
        self.buffer.extend_from_slice(part);
        if self.buffer.len() < total {
            return Ok(None);
        }
        let bytes = std::mem::take(&mut self.buffer);
        String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| ClipboardError::NotUtf8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(text: &str) -> String {
        let mut a = Assembler::default();
        let mut done = None;
        for m in encode_text(text.as_bytes()).unwrap() {
            let Message::Text {
                total,
                offset,
                part,
            } = Message::parse(&m).unwrap()
            else {
                panic!("not text");
            };
            assert!(done.is_none(), "text finished before its last part");
            done = a.add(total, offset, part).unwrap();
        }
        done.expect("text never finished")
    }

    #[test]
    fn texts_of_any_size_come_back_whole() {
        assert_eq!(roundtrip(""), "");
        assert_eq!(
            roundtrip("Привет, мир 👋\r\nвторая строка\tтаб"),
            "Привет, мир 👋\r\nвторая строка\tтаб"
        );
        let big: String = "ёжик 🦔 ".repeat(MAX_TEXT / 16);
        assert!(big.len() <= MAX_TEXT && big.len() > PART * 4);
        assert_eq!(roundtrip(&big), big);
    }

    #[test]
    fn a_part_boundary_inside_a_character_is_fine() {
        // 16 KiB boundary falls inside a two-byte letter: only the whole text must be UTF-8.
        let text = format!("a{}", "ж".repeat(PART));
        assert_eq!(roundtrip(&text), text);
    }

    #[test]
    fn over_the_limit_is_refused_both_ways() {
        let big = vec![b'a'; MAX_TEXT + 1];
        assert_eq!(
            encode_text(&big),
            Err(ClipboardError::TooLarge(MAX_TEXT + 1))
        );
        let mut m = vec![CLIPBOARD_TEXT];
        m.extend_from_slice(&((MAX_TEXT as u32) + 1).to_le_bytes());
        m.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            Message::parse(&m),
            Err(ClipboardError::TooLarge(MAX_TEXT + 1))
        );
    }

    #[test]
    fn bad_messages_are_refused() {
        assert_eq!(Message::parse(&[]), Err(ClipboardError::Malformed));
        assert_eq!(
            Message::parse(&[0x30, 1, 0, 0]),
            Err(ClipboardError::Malformed)
        );
        // A part that runs past its total.
        let mut m = vec![CLIPBOARD_TEXT];
        m.extend_from_slice(&2u32.to_le_bytes());
        m.extend_from_slice(&0u32.to_le_bytes());
        m.extend_from_slice(b"abc");
        assert_eq!(Message::parse(&m), Err(ClipboardError::Malformed));
        assert_eq!(
            Message::parse(&[0x31, 1, 0, 0]),
            Err(ClipboardError::Malformed)
        );
        assert_eq!(
            Message::parse(&encode_applied(7)),
            Ok(Message::Applied { total: 7 })
        );
        assert_eq!(
            Message::parse(&encode_too_large(9)),
            Ok(Message::TooLarge { size: 9 })
        );
    }

    #[test]
    fn parts_out_of_order_or_not_utf8_are_refused() {
        let mut a = Assembler::default();
        assert_eq!(a.add(10, 5, b"abc"), Err(ClipboardError::OutOfOrder));
        assert_eq!(a.add(2, 0, &[0xff, 0xfe]), Err(ClipboardError::NotUtf8));
        // A new text at offset 0 drops the unfinished one.
        assert_eq!(a.add(6, 0, b"abc"), Ok(None));
        assert_eq!(a.add(2, 0, b"ok"), Ok(Some("ok".into())));
    }
}
