// Binary input messages from docs/PROTOCOL.md; the Rust side is crates/rcs-protocol/src/input.rs.
// Field names follow the shared examples in crates/rcs-protocol/vectors/input.json.

export const MAX_TOUCH_POINTS = 10;
export const MAX_TEXT_BYTES = 4096;
export const MAX_CURSOR_SHAPE_BYTES = 64 * 1024;

export type MouseButton = 'left' | 'right' | 'middle' | 'back' | 'forward';
export type TouchPhase = 'down' | 'move' | 'up' | 'cancel';

const BUTTONS: readonly MouseButton[] = ['left', 'right', 'middle', 'back', 'forward'];
const PHASES: readonly TouchPhase[] = ['down', 'move', 'up', 'cancel'];

export interface TouchPoint {
  id: number;
  phase: TouchPhase;
  x: number;
  y: number;
}

export interface GamepadState {
  type: 'gamepad_state';
  seq: number;
  pad: number;
  buttons: number;
  left_trigger: number;
  right_trigger: number;
  left_x: number;
  left_y: number;
  right_x: number;
  right_y: number;
}

export type ClientMessage =
  | { type: 'mouse_move_absolute'; seq: number; x: number; y: number }
  | { type: 'mouse_move_relative'; seq: number; dx: number; dy: number }
  | GamepadState
  | { type: 'key'; down: boolean; scancode: number }
  | { type: 'mouse_button'; button: MouseButton; down: boolean }
  | { type: 'wheel'; dx: number; dy: number }
  | { type: 'touch'; points: TouchPoint[] }
  | { type: 'release_all' }
  | { type: 'text'; text: string }
  | { type: 'special'; code: number };

export type ServerMessage =
  | { type: 'cursor_state'; seq: number; visible: boolean; x: number; y: number }
  | { type: 'cursor_shape'; shape_id: number; hot_x: number; hot_y: number; width: number; height: number; png: Uint8Array };

export type ParseError = { error: 'empty' } | { error: 'unknown' | 'malformed'; type: number };

const utf8 = new TextEncoder();

/** UTF-8 of `text`, cut at a character boundary within MAX_TEXT_BYTES. */
function textBytes(text: string): Uint8Array {
  const all = utf8.encode(text);
  if (all.length <= MAX_TEXT_BYTES) return all;
  let end = MAX_TEXT_BYTES;
  while (end > 0 && ((all[end] ?? 0) & 0xc0) === 0x80) end -= 1;
  return all.subarray(0, end);
}

/** Encodes one browser → server message. Touch lists over MAX_TOUCH_POINTS are cut; the caller sends the rest. */
export function encodeClient(m: ClientMessage): Uint8Array {
  const make = (size: number, type: number): [Uint8Array, DataView] => {
    const bytes = new Uint8Array(size);
    bytes[0] = type;
    return [bytes, new DataView(bytes.buffer)];
  };
  switch (m.type) {
    case 'mouse_move_absolute': {
      const [b, v] = make(7, 0x01);
      v.setUint16(1, m.seq, true);
      v.setUint16(3, m.x, true);
      v.setUint16(5, m.y, true);
      return b;
    }
    case 'mouse_move_relative': {
      const [b, v] = make(11, 0x02);
      v.setUint16(1, m.seq, true);
      v.setInt32(3, m.dx, true);
      v.setInt32(7, m.dy, true);
      return b;
    }
    case 'gamepad_state': {
      const [b, v] = make(16, 0x03);
      v.setUint16(1, m.seq, true);
      v.setUint8(3, m.pad);
      v.setUint16(4, m.buttons, true);
      v.setUint8(6, m.left_trigger);
      v.setUint8(7, m.right_trigger);
      v.setInt16(8, m.left_x, true);
      v.setInt16(10, m.left_y, true);
      v.setInt16(12, m.right_x, true);
      v.setInt16(14, m.right_y, true);
      return b;
    }
    case 'key': {
      const [b, v] = make(4, 0x10);
      v.setUint8(1, m.down ? 1 : 0);
      v.setUint16(2, m.scancode, true);
      return b;
    }
    case 'mouse_button': {
      const [b] = make(3, 0x11);
      b[1] = BUTTONS.indexOf(m.button);
      b[2] = m.down ? 1 : 0;
      return b;
    }
    case 'wheel': {
      const [b, v] = make(5, 0x12);
      v.setInt16(1, m.dx, true);
      v.setInt16(3, m.dy, true);
      return b;
    }
    case 'touch': {
      const points = m.points.slice(0, MAX_TOUCH_POINTS);
      const [b, v] = make(2 + 6 * points.length, 0x13);
      b[1] = points.length;
      points.forEach((p, i) => {
        const at = 2 + 6 * i;
        v.setUint8(at, p.id);
        v.setUint8(at + 1, PHASES.indexOf(p.phase));
        v.setUint16(at + 2, p.x, true);
        v.setUint16(at + 4, p.y, true);
      });
      return b;
    }
    case 'release_all':
      return make(1, 0x14)[0];
    case 'text': {
      const body = textBytes(m.text);
      const [b, v] = make(3 + body.length, 0x15);
      v.setUint16(1, body.length, true);
      b.set(body, 3);
      return b;
    }
    case 'special': {
      const [b] = make(2, 0x16);
      b[1] = m.code;
      return b;
    }
  }
}

/** Parses one server → browser message. */
export function decodeServer(b: Uint8Array): ServerMessage | ParseError {
  const type = b[0];
  if (type === undefined) return { error: 'empty' };
  const bad: ParseError = { error: 'malformed', type };
  const view = new DataView(b.buffer, b.byteOffset, b.byteLength);
  switch (type) {
    case 0x21: {
      if (b.length !== 8) return bad;
      const visible = b[3];
      if (visible !== 0 && visible !== 1) return bad;
      return {
        type: 'cursor_state',
        seq: view.getUint16(1, true),
        visible: visible === 1,
        x: view.getUint16(4, true),
        y: view.getUint16(6, true),
      };
    }
    case 0x20: {
      if (b.length <= 11 || b.length > MAX_CURSOR_SHAPE_BYTES) return bad;
      return {
        type: 'cursor_shape',
        shape_id: view.getUint16(1, true),
        hot_x: view.getUint16(3, true),
        hot_y: view.getUint16(5, true),
        width: view.getUint16(7, true),
        height: view.getUint16(9, true),
        png: b.slice(11),
      };
    }
    default:
      return { error: 'unknown', type };
  }
}

/** RFC 1982 for 16 bits: `a` comes after `b`. Equal numbers and numbers half the range apart are not newer. */
export function seqNewer(a: number, b: number): boolean {
  const d = (a - b) & 0xffff;
  return d !== 0 && d < 0x8000;
}

/** The next `seq` after `s`, wrapping at 65535. */
export function nextSeq(s: number): number {
  return (s + 1) & 0xffff;
}
