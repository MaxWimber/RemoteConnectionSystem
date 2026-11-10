// Messages of the `clipboard` data channel (D-040, PROTOCOL.md): UTF-8 text in parts of up to 16 KiB, 1 MiB in all.

export const CLIPBOARD_TEXT = 0x30;
export const CLIPBOARD_APPLIED = 0x31;
export const CLIPBOARD_TOO_LARGE = 0x32;
export const MAX_TEXT = 1 << 20;
export const PART = 16 * 1024;
const HEADER = 9;

export type ClipboardMessage =
  | { type: 'text'; total: number; offset: number; part: Uint8Array }
  | { type: 'applied'; total: number }
  | { type: 'too_large'; size: number };

export function utf8Length(text: string): number {
  return new TextEncoder().encode(text).length;
}

/** The messages that carry `text`, or null when it is over the limit. */
export function encodeText(text: string): Uint8Array[] | null {
  const bytes = new TextEncoder().encode(text);
  if (bytes.length > MAX_TEXT) return null;
  const out: Uint8Array[] = [];
  let offset = 0;
  do {
    const end = Math.min(offset + PART, bytes.length);
    const m = new Uint8Array(HEADER + end - offset);
    const view = new DataView(m.buffer);
    m[0] = CLIPBOARD_TEXT;
    view.setUint32(1, bytes.length, true);
    view.setUint32(5, offset, true);
    m.set(bytes.subarray(offset, end), HEADER);
    out.push(m);
    offset = end;
  } while (offset < bytes.length);
  return out;
}

export function parseMessage(b: Uint8Array): ClipboardMessage | null {
  const view = new DataView(b.buffer, b.byteOffset, b.byteLength);
  if (b[0] === CLIPBOARD_TEXT && b.length >= HEADER) {
    const total = view.getUint32(1, true);
    const offset = view.getUint32(5, true);
    const part = b.subarray(HEADER);
    if (total > MAX_TEXT || part.length > PART || offset + part.length > total) return null;
    return { type: 'text', total, offset, part };
  }
  if (b[0] === CLIPBOARD_APPLIED && b.length === 5) return { type: 'applied', total: view.getUint32(1, true) };
  if (b[0] === CLIPBOARD_TOO_LARGE && b.length === 5) return { type: 'too_large', size: view.getUint32(1, true) };
  return null;
}

/** Puts the parts of one text back together; a part at offset 0 starts a new text. */
export class Assembler {
  private buffer: Uint8Array | null = null;
  private filled = 0;

  /** The whole text after its last part, null while parts are missing; throws on a part out of order. */
  add(total: number, offset: number, part: Uint8Array): string | null {
    if (offset === 0) {
      this.buffer = new Uint8Array(total);
      this.filled = 0;
    } else if (!this.buffer || this.buffer.length !== total || offset !== this.filled) {
      this.buffer = null;
      throw new Error('часть буфера обмена не по порядку');
    }
    this.buffer.set(part, offset);
    this.filled = offset + part.length;
    if (this.filled < total) return null;
    const bytes = this.buffer;
    this.buffer = null;
    return new TextDecoder('utf-8', { fatal: true }).decode(bytes);
  }
}
