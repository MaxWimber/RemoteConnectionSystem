import { describe, expect, it } from 'vitest';
import { isPaste } from '../input/controller';
import { Assembler, encodeText, MAX_TEXT, PART, parseMessage } from './protocol';

function roundtrip(text: string): string | null {
  const a = new Assembler();
  let done: string | null = null;
  for (const m of encodeText(text) ?? []) {
    const parsed = parseMessage(m);
    if (!parsed || parsed.type !== 'text') throw new Error('not text');
    expect(done).toBeNull();
    done = a.add(parsed.total, parsed.offset, parsed.part);
  }
  return done;
}

describe('clipboard protocol', () => {
  it('brings texts of any size back whole, Cyrillic, emoji and line breaks included', () => {
    expect(roundtrip('')).toBe('');
    const text = 'Привет 👋\r\nвторая строка\tтаб';
    expect(roundtrip(text)).toBe(text);
    const big = 'ёжик 🦔 '.repeat(MAX_TEXT / 16);
    expect(encodeText(big)!.length).toBeGreaterThan(4);
    expect(roundtrip(big)).toBe(big);
  });

  it('splits a character across parts and still decodes it', () => {
    const text = 'a' + 'ж'.repeat(PART);
    expect(roundtrip(text)).toBe(text);
  });

  it('refuses a text over 1 MiB', () => {
    expect(encodeText('a'.repeat(MAX_TEXT + 1))).toBeNull();
  });

  it('matches the Rust encoding byte for byte', () => {
    // 0x30, total 2, offset 0, "ok": the same bytes rcs-protocol builds.
    expect(Array.from(encodeText('ok')![0]!)).toEqual([0x30, 2, 0, 0, 0, 0, 0, 0, 0, 0x6f, 0x6b]);
    expect(parseMessage(new Uint8Array([0x31, 7, 0, 0, 0]))).toEqual({ type: 'applied', total: 7 });
    expect(parseMessage(new Uint8Array([0x32, 9, 0, 0, 0]))).toEqual({ type: 'too_large', size: 9 });
    expect(parseMessage(new Uint8Array([0x31, 7, 0, 0]))).toBeNull();
    expect(parseMessage(new Uint8Array([0x30, 2, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3]))).toBeNull();
  });

  it('throws on a part out of order', () => {
    expect(() => new Assembler().add(10, 5, new Uint8Array(3))).toThrow();
  });
});

describe('isPaste', () => {
  const key = (code: string, mods: Partial<Record<'ctrlKey' | 'shiftKey' | 'altKey' | 'metaKey', boolean>> = {}) => ({
    code,
    ctrlKey: false,
    shiftKey: false,
    altKey: false,
    metaKey: false,
    ...mods,
  });

  it('knows Ctrl+V and Shift+Insert and nothing else', () => {
    expect(isPaste(key('KeyV', { ctrlKey: true }))).toBe(true);
    expect(isPaste(key('KeyV', { ctrlKey: true, shiftKey: true }))).toBe(true);
    expect(isPaste(key('Insert', { shiftKey: true }))).toBe(true);
    expect(isPaste(key('KeyV'))).toBe(false);
    expect(isPaste(key('KeyV', { ctrlKey: true, altKey: true }))).toBe(false);
    expect(isPaste(key('Insert', { ctrlKey: true }))).toBe(false);
  });
});
