// The two input data channels of one RTCPeerConnection, docs/PROTOCOL.md.

import { encodeClient, nextSeq, type ClientMessage } from './protocol';

/** Over this much waiting data a mouse move is dropped: the next one carries the newer position anyway. */
const FAST_BUFFER_LIMIT = 16 * 1024;

export class InputChannels {
  readonly fast: RTCDataChannel;
  readonly reliable: RTCDataChannel;
  private absoluteSeq = 0;
  private relativeSeq = 0;
  /** Sums of relative movement since the session started, which the protocol sends (PROTOCOL.md). */
  private relativeSum = { dx: 0, dy: 0 };

  constructor(pc: RTCPeerConnection) {
    this.fast = pc.createDataChannel('input-fast', { ordered: false, maxRetransmits: 0 });
    this.reliable = pc.createDataChannel('input-reliable', { ordered: true });
    this.fast.binaryType = 'arraybuffer';
    this.reliable.binaryType = 'arraybuffer';
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

  send(m: ClientMessage): void {
    if (this.reliable.readyState !== 'open') return;
    this.reliable.send(encodeClient(m).buffer as ArrayBuffer);
  }

  private sendFast(m: ClientMessage): void {
    if (this.fast.readyState !== 'open' || this.fast.bufferedAmount > FAST_BUFFER_LIMIT) return;
    this.fast.send(encodeClient(m).buffer as ArrayBuffer);
  }
}
