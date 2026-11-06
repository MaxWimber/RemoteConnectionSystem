use serde_json::Value;

use super::*;

const VECTORS: &str = include_str!("../../vectors/input.json");

fn vectors() -> Value {
    serde_json::from_str(VECTORS).expect("vectors/input.json is JSON")
}

fn hex_of(case: &Value) -> Vec<u8> {
    from_hex(case["hex"].as_str().unwrap()).unwrap()
}

fn expected_error(case: &Value) -> ParseError {
    let kind = case["type"].as_u64().map(|t| t as u8);
    match case["error"].as_str().unwrap() {
        "empty" => ParseError::Empty,
        "unknown" => ParseError::Unknown(kind.unwrap()),
        "malformed" => ParseError::Malformed(kind.unwrap()),
        other => panic!("unknown error kind {other}"),
    }
}

#[test]
fn client_examples_parse_and_encode_back() {
    for case in vectors()["client"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let bytes = hex_of(case);
        let expected: ClientMessage = serde_json::from_value(case["message"].clone()).unwrap();
        assert_eq!(ClientMessage::parse(&bytes), Ok(expected.clone()), "{name}");
        let mut out = Vec::new();
        expected.encode(&mut out);
        assert_eq!(to_hex(&out), to_hex(&bytes), "{name}");
        assert_eq!(
            serde_json::to_value(&expected).unwrap(),
            case["message"],
            "{name}"
        );
    }
}

#[test]
fn server_examples_parse_and_encode_back() {
    for case in vectors()["server"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let bytes = hex_of(case);
        let expected: ServerMessage = serde_json::from_value(case["message"].clone()).unwrap();
        assert_eq!(ServerMessage::parse(&bytes), Ok(expected.clone()), "{name}");
        let mut out = Vec::new();
        assert!(expected.encode(&mut out), "{name}");
        assert_eq!(to_hex(&out), to_hex(&bytes), "{name}");
    }
}

#[test]
fn invalid_examples_are_refused() {
    let v = vectors();
    for case in v["client_invalid"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        assert_eq!(
            ClientMessage::parse(&hex_of(case)),
            Err(expected_error(case)),
            "{name}"
        );
    }
    for case in v["server_invalid"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        assert_eq!(
            ServerMessage::parse(&hex_of(case)),
            Err(expected_error(case)),
            "{name}"
        );
    }
}

#[test]
fn text_has_a_length_limit() {
    let mut b = vec![TEXT];
    b.extend_from_slice(&(MAX_TEXT_BYTES as u16).to_le_bytes());
    b.extend(std::iter::repeat_n(b'a', MAX_TEXT_BYTES));
    assert!(ClientMessage::parse(&b).is_ok());
    let mut b = vec![TEXT];
    b.extend_from_slice(&(MAX_TEXT_BYTES as u16 + 1).to_le_bytes());
    b.extend(std::iter::repeat_n(b'a', MAX_TEXT_BYTES + 1));
    assert_eq!(ClientMessage::parse(&b), Err(ParseError::Malformed(TEXT)));

    // Encoding cuts long text at a character boundary, so the result still parses.
    let long = "ж".repeat(MAX_TEXT_BYTES);
    let mut out = Vec::new();
    ClientMessage::Text { text: long }.encode(&mut out);
    assert_eq!(out.len(), 3 + MAX_TEXT_BYTES);
    assert!(ClientMessage::parse(&out).is_ok());
}

#[test]
fn cursor_shape_has_a_size_limit() {
    let fits = ServerMessage::CursorShape {
        shape_id: 1,
        hot_x: 0,
        hot_y: 0,
        width: 1,
        height: 1,
        png: vec![7; MAX_CURSOR_SHAPE_BYTES - CURSOR_SHAPE_HEADER],
    };
    let mut out = Vec::new();
    assert!(fits.encode(&mut out));
    assert_eq!(out.len(), MAX_CURSOR_SHAPE_BYTES);
    assert_eq!(ServerMessage::parse(&out), Ok(fits));
    out.push(7);
    assert_eq!(
        ServerMessage::parse(&out),
        Err(ParseError::Malformed(CURSOR_SHAPE))
    );
    let too_big = ServerMessage::CursorShape {
        shape_id: 1,
        hot_x: 0,
        hot_y: 0,
        width: 1,
        height: 1,
        png: vec![7; MAX_CURSOR_SHAPE_BYTES],
    };
    let mut out = Vec::new();
    assert!(!too_big.encode(&mut out));
    assert!(out.is_empty());
}

#[test]
fn seq_follows_serial_number_arithmetic() {
    assert!(seq_newer(1, 0));
    assert!(seq_newer(0, 65535));
    assert!(seq_newer(10, 65530));
    assert!(seq_newer(0x7fff, 0));
    assert!(!seq_newer(0x8000, 0));
    assert!(!seq_newer(5, 5));
    assert!(!seq_newer(65530, 10));

    let mut f = SeqFilter::default();
    assert!(f.accept(65534));
    assert!(f.accept(65535));
    assert!(f.accept(1), "wraps around");
    assert!(!f.accept(1), "a repeat is dropped");
    assert!(!f.accept(65535), "an old one is dropped");
    assert!(f.accept(3));
    f.reset();
    assert!(f.accept(100), "anything goes after a reset");
}

/// xorshift64*: deterministic, so a failure repeats with the same seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next() as u8).collect()
    }
}

/// Parsing never panics, and whatever it accepts encodes back to the same bytes. Returns whether it accepted.
fn check(bytes: &[u8]) -> bool {
    let mut accepted = false;
    if let Ok(m) = ClientMessage::parse(bytes) {
        let mut out = Vec::new();
        m.encode(&mut out);
        assert_eq!(out, bytes, "{m:?}");
        accepted = true;
    }
    if let Ok(m) = ServerMessage::parse(bytes) {
        let mut out = Vec::new();
        assert!(m.encode(&mut out));
        assert_eq!(out, bytes, "{m:?}");
        accepted = true;
    }
    accepted
}

const KNOWN: [u8; 12] = [
    MOUSE_MOVE_ABSOLUTE,
    MOUSE_MOVE_RELATIVE,
    GAMEPAD_STATE,
    KEY,
    MOUSE_BUTTON,
    WHEEL,
    TOUCH,
    RELEASE_ALL,
    TEXT,
    SPECIAL,
    CURSOR_SHAPE,
    CURSOR_STATE,
];

#[test]
fn fuzz_random_bytes() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for _ in 0..200_000 {
        let len = rng.below(80);
        let mut b = rng.bytes(len);
        // Most random bytes are an unknown type; half of the cases get a known one.
        if !b.is_empty() && rng.below(2) == 0 {
            b[0] = KNOWN[rng.below(KNOWN.len())];
        }
        let _ = check(&b);
    }
}

#[test]
fn fuzz_every_length_of_every_type() {
    let mut rng = Rng(42);
    let mut accepted = [0usize; KNOWN.len()];
    for (k, kind) in KNOWN.into_iter().enumerate() {
        for len in 0..=64 {
            for _ in 0..200 {
                let mut b = rng.bytes(len + 1);
                b[0] = kind;
                // Small values in the enum, count and length fields reach the deeper checks.
                if rng.below(2) == 0 {
                    for x in &mut b[1..] {
                        *x = rng.below(4) as u8;
                    }
                }
                if len >= 1 && rng.below(2) == 0 {
                    b[1] = rng.below(12) as u8;
                }
                if len >= 2 && kind == TEXT && rng.below(2) == 0 {
                    let body = (len - 2) as u16;
                    b[1..3].copy_from_slice(&body.to_le_bytes());
                }
                accepted[k] += usize::from(check(&b));
            }
        }
    }
    // Every type gets past its checks often enough for the round trip to mean something.
    for (kind, n) in KNOWN.iter().zip(accepted) {
        assert!(n >= 20, "type 0x{kind:02x}: {n} accepted");
    }
}

#[test]
fn fuzz_mutated_examples() {
    let v = vectors();
    let examples: Vec<Vec<u8>> = ["client", "server", "client_invalid", "server_invalid"]
        .iter()
        .flat_map(|k| v[*k].as_array().unwrap().iter().map(hex_of))
        .collect();
    let mut rng = Rng(7);
    for _ in 0..100_000 {
        let mut b = examples[rng.below(examples.len())].clone();
        match rng.below(4) {
            0 if !b.is_empty() => {
                let i = rng.below(b.len());
                b[i] ^= 1 << rng.below(8);
            }
            1 if !b.is_empty() => {
                let i = rng.below(b.len());
                b[i] = rng.next() as u8;
            }
            2 => b.truncate(rng.below(b.len() + 1)),
            _ => {
                let extra = rng.below(8);
                b.extend(rng.bytes(extra));
            }
        }
        let _ = check(&b);
    }
}
