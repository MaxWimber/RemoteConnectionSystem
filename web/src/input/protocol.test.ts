import { describe, expect, it } from 'vitest';
import vectors from '../../../crates/rcs-protocol/vectors/input.json';
import { MAX_TEXT_BYTES, decodeServer, encodeClient, nextSeq, seqNewer, type ClientMessage } from './protocol';

function hex(b: Uint8Array): string {
  return Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('');
}

function bytes(h: string): Uint8Array {
  return new Uint8Array((h.match(/../g) ?? []).map((x) => parseInt(x, 16)));
}

describe('shared examples', () => {
  it.each(vectors.client.map((c) => [c.name, c] as const))('encodes %s', (_, c) => {
    expect(hex(encodeClient(c.message as ClientMessage))).toBe(c.hex);
  });

  it.each(vectors.server.map((c) => [c.name, c] as const))('decodes %s', (_, c) => {
    const m = decodeServer(bytes(c.hex));
    const expected = { ...c.message } as Record<string, unknown>;
    if ('png' in expected) expected['png'] = bytes(String(expected['png']));
    expect(m).toEqual(expected);
  });

  it.each(vectors.server_invalid.map((c) => [c.name, c] as const))('refuses %s', (_, c) => {
    expect(decodeServer(bytes(c.hex))).toEqual({ error: c.error, type: c.type });
  });

  it('refuses an empty message', () => {
    expect(decodeServer(new Uint8Array())).toEqual({ error: 'empty' });
  });
});

describe('text', () => {
  it('is cut at a character boundary', () => {
    const b = encodeClient({ type: 'text', text: 'ж'.repeat(MAX_TEXT_BYTES) });
    expect(b.length).toBe(3 + MAX_TEXT_BYTES);
    expect(new TextDecoder('utf-8', { fatal: true }).decode(b.subarray(3))).toBe('ж'.repeat(MAX_TEXT_BYTES / 2));
  });
});

describe('seq', () => {
  it('follows serial number arithmetic', () => {
    expect(seqNewer(1, 0)).toBe(true);
    expect(seqNewer(0, 65535)).toBe(true);
    expect(seqNewer(0x7fff, 0)).toBe(true);
    expect(seqNewer(0x8000, 0)).toBe(false);
    expect(seqNewer(5, 5)).toBe(false);
    expect(seqNewer(65530, 10)).toBe(false);
    expect(nextSeq(65535)).toBe(0);
  });
});
