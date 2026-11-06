import { describe, expect, it } from 'vitest';
import { InputChannels } from './channels';
import { cssCursor, overlayPlacement } from './cursor';
import { buttonOf, contentBox, normalized, takeWhole, wheelUnits } from './pointer';
import { mappedCodes, scancodeFor } from './scancodes';

describe('scan codes', () => {
  it('maps physical keys to Set 1', () => {
    expect(scancodeFor('KeyA')).toBe(0x1e);
    expect(scancodeFor('Escape')).toBe(0x01);
    expect(scancodeFor('ControlRight')).toBe(0xe01d);
    expect(scancodeFor('MetaLeft')).toBe(0xe05b);
    expect(scancodeFor('ArrowUp')).toBe(0xe048);
    expect(scancodeFor('NumpadEnter')).toBe(0xe01c);
    expect(scancodeFor('F12')).toBe(0x58);
    expect(scancodeFor('Pause')).toBeUndefined();
    expect(scancodeFor('NoSuchKey')).toBeUndefined();
  });

  it('gives every key its own code', () => {
    const codes = mappedCodes().map((c) => scancodeFor(c));
    expect(new Set(codes).size).toBe(codes.length);
    for (const c of codes) {
      expect(c! >> 8 === 0 || c! >> 8 === 0xe0).toBe(true);
    }
  });
});

describe('pointer', () => {
  it('finds the picture inside a letterboxed video', () => {
    // 1920x1080 in a 1000x1000 element: a 1000x562.5 band in the middle.
    const box = contentBox({ left: 10, top: 20, width: 1000, height: 1000 }, 1920, 1080);
    expect(box.width).toBeCloseTo(1000);
    expect(box.height).toBeCloseTo(562.5);
    expect(box.top).toBeCloseTo(20 + 218.75);
    expect(normalized(box, 10, box.top)).toEqual({ x: 0, y: 0 });
    expect(normalized(box, 1009, box.top + box.height - 1)).toEqual({ x: 65535, y: 65535 });
    expect(normalized(box, -500, 5000), 'clamped to the edges').toEqual({ x: 0, y: 65535 });
    expect(normalized(box, 10 + 499.5, box.top + (box.height - 1) / 2)).toEqual({ x: 32768, y: 32768 });
  });

  it('keeps the element when the video size is unknown', () => {
    const element = { left: 0, top: 0, width: 800, height: 600 };
    expect(contentBox(element, 0, 0)).toEqual(element);
  });

  it('maps buttons and wheel', () => {
    expect([0, 1, 2, 3, 4, 5].map(buttonOf)).toEqual(['left', 'middle', 'right', 'back', 'forward', undefined]);
    expect(wheelUnits(100, 0)).toBe(-120);
    expect(wheelUnits(-3, 1)).toBe(120);
    expect(wheelUnits(1, 2)).toBe(-120);
    expect(takeWhole(250.5)).toEqual([250, 0.5]);
    expect(takeWhole(-99999)).toEqual([-32768, -99999 + 32768]);
  });
});

class FakeChannel {
  readyState: RTCDataChannelState = 'open';
  bufferedAmount = 0;
  binaryType = 'blob';
  sent: Uint8Array[] = [];
  constructor(
    readonly label: string,
    readonly options: RTCDataChannelInit,
  ) {}
  send(data: ArrayBuffer): void {
    this.sent.push(new Uint8Array(data));
  }
}

function fakePeer(): { pc: RTCPeerConnection; channels: FakeChannel[] } {
  const channels: FakeChannel[] = [];
  const pc = {
    createDataChannel: (label: string, options: RTCDataChannelInit) => {
      const c = new FakeChannel(label, options);
      channels.push(c);
      return c;
    },
  } as unknown as RTCPeerConnection;
  return { pc, channels };
}

describe('input channels', () => {
  it('opens the two channels of PROTOCOL.md', () => {
    const { pc, channels } = fakePeer();
    new InputChannels(pc);
    expect(channels.map((c) => [c.label, c.options])).toEqual([
      ['input-fast', { ordered: false, maxRetransmits: 0 }],
      ['input-reliable', { ordered: true }],
    ]);
    expect(channels.every((c) => c.binaryType === 'arraybuffer')).toBe(true);
  });

  it('sends relative sums with growing seq and drops moves when the buffer is full', () => {
    const { pc, channels } = fakePeer();
    const input = new InputChannels(pc);
    const fast = channels[0]!;
    input.moveRelative(5, -2);
    input.moveRelative(3, 0);
    const hex = (b: Uint8Array) => Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('');
    expect(fast.sent.map(hex)).toEqual(['02010005000000feffffff', '02020008000000feffffff']);
    fast.bufferedAmount = 1 << 20;
    input.moveAbsolute(1, 2);
    expect(fast.sent.length).toBe(2);
    input.send({ type: 'release_all' });
    expect(channels[1]!.sent.map(hex)).toEqual(['14']);
  });

  it('sends nothing on a closed channel', () => {
    const { pc, channels } = fakePeer();
    const input = new InputChannels(pc);
    channels.forEach((c) => (c.readyState = 'connecting'));
    expect(input.open).toBe(false);
    input.send({ type: 'key', down: true, scancode: 0x1e });
    input.moveAbsolute(1, 2);
    expect(channels.every((c) => c.sent.length === 0)).toBe(true);
  });
});

describe('cursor', () => {
  const shape = { url: 'blob:x', width: 32, height: 32, hotX: 4, hotY: 2 };

  it('is drawn where the server says, scaled like the video', () => {
    // 1920 wide picture shown 960 wide: half size.
    const box = { left: 100, top: 50, width: 960, height: 540 };
    const p = overlayPlacement(box, 1920, 65535, 0, shape);
    expect(p).toEqual({ left: 100 + 959 - 2, top: 50 - 1, width: 16, height: 16 });
  });

  it('becomes the CSS cursor when the browser can show it', () => {
    expect(cssCursor(shape, true)).toBe('url("blob:x") 4 2, default');
    expect(cssCursor(shape, false)).toBe('none');
    expect(cssCursor(null, true)).toBe('default');
    expect(cssCursor({ ...shape, width: 256 }, true)).toBe('none');
  });

  it('keeps only the newest position from the unordered channel', () => {
    const { pc, channels } = fakePeer();
    const got: number[] = [];
    new InputChannels(pc, (m) => got.push(m.type === 'cursor_state' ? m.x : -1));
    const fast = channels[0] as unknown as { onmessage: (e: { data: ArrayBuffer }) => void };
    const state = (seq: number, x: number) => {
      const b = new Uint8Array([0x21, seq & 0xff, seq >> 8, 1, x & 0xff, x >> 8, 0, 0]);
      fast.onmessage({ data: b.buffer });
    };
    state(5, 10);
    state(4, 20);
    state(6, 30);
    state(6, 40);
    expect(got).toEqual([10, 30]);
  });
});