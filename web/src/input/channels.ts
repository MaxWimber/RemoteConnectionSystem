// The two input data channels of one RTCPeerConnection, docs/PROTOCOL.md.

import {
  decodeServer,
  encodeClient,
  nextSeq,
  seqNewer,
  type ClientMessage,
  type GamepadState,
  type ServerMessage,
  type TouchPoint,
} from './protocol';

/** Over this much waiting data a mouse move is dropped: the next one carries the newer position anyway. */
const FAST_BUFFER_LIMIT = 16 * 1024;

export class InputChannels {
  readonly fast: RTCDataChannel;
  readonly reliable: RTCDataChannel;
  private absoluteSeq = 0;
  private relativeSeq = 0;
  /** Sums of relative movement since the session started, which the protocol sends (PROTOCOL.md). */
  private relativeSum = { dx: 0, dy: 0 };
  private cursorSeq: number | null = null;
  private padSeq = [0, 0, 0, 0];
  private touchSeq = 0;

  /** `onServer` gets the server's pointer positions, newest only: input-fast is unordered. */
  constructor(pc: RTCPeerConnection, onServer?: (m: ServerMessage) => void) {
    this.fast = pc.createDataChannel('input-fast', { ordered: false, maxRetransmits: 0 });
    this.reliable = pc.createDataChannel('input-reliable', { ordered: true });
    this.fast.binaryType = 'arraybuffer';
    this.reliable.binaryType = 'arraybuffer';
    this.fast.onmessage = (e: MessageEvent) => {
      if (!(e.data instanceof ArrayBuffer) || !onServer) return;
      const m = decodeServer(new Uint8Array(e.data));
      if ('error' in m || m.type !== 'cursor_state') return;
      if (this.cursorSeq !== null && !seqNewer(m.seq, this.cursorSeq)) return;
      this.cursorSeq = m.seq;
      onServer(m);
    };
  }

  get open(): boolean {
    return this.reliable.readyState === 'open';
  }

  moveAbsolute(x: number, y: number): void {
    this.absoluteSeq = nextSeq(this.absoluteSeq);
    this.sendFast({ type: 'mouse_move_absolute', seq: this.absoluteSeq, x, y });
  }

  /** Adds a movement and sends the new sums; a lost message loses nothing. */
  moveRelative(dx: number, dy: number): void {
    this.relativeSum.dx = (this.relativeSum.dx + dx) | 0;
    this.relativeSum.dy = (this.relativeSum.dy + dy) | 0;
    this.relativeSeq = nextSeq(this.relativeSeq);
    this.sendFast({ type: 'mouse_move_relative', seq: this.relativeSeq, ...this.relativeSum });
  }

  /** One pad's whole state on input-fast, with that pad's own seq (PROTOCOL.md). */
  sendGamepad(pad: number, state: Omit<GamepadState, 'type' | 'seq' | 'pad'>): void {
    const seq = nextSeq(this.padSeq[pad] ?? 0);
    this.padSeq[pad] = seq;
    this.sendFast({ type: 'gamepad_state', seq, pad, ...state });
  }

  /** Touch points on input-fast (P11-1): a lost datagram holds nothing back, the next one has newer places. */
  sendTouchFast(points: TouchPoint[]): void {
    this.touchSeq = nextSeq(this.touchSeq);
    this.sendFast({ type: 'touch_fast', seq: this.touchSeq, points });
  }

  send(m: ClientMessage): void {
    if (this.reliable.readyState !== 'open') return;
    this.reliable.send(encodeClient(m).buffer as ArrayBuffer);
  }

  private sendFast(m: ClientMessage): void {
    if (this.fast.readyState !== 'open' || this.fast.bufferedAmount > FAST_BUFFER_LIMIT) return;
    this.fast.send(encodeClient(m).buffer as ArrayBuffer);
  }
}
