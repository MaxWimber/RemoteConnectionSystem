// Experimental video over WebTransport + WebCodecs (P9-4, D-043): every frame arrives in its own unidirectional
// stream, is decoded by VideoDecoder and drawn on a canvas. Input, sound and the rest stay on WebRTC.
//
// Frame message: u64 capture time (Unix microseconds), u32 seq, u8 flags (bit 0 keyframe), then H.264 Annex B.

import type { ClockSync } from './clock';
import { recordShown } from './display-log';

export interface WtOffer {
  url: string;
  cert_sha256: string;
}

export function isWtOffer(v: unknown): v is WtOffer {
  const o = v as Record<string, unknown> | null;
  return !!o && typeof o['url'] === 'string' && typeof o['cert_sha256'] === 'string';
}

export interface WtFrameHeader {
  capturedUnixMs: number;
  seq: number;
  keyframe: boolean;
}

export const HEADER = 13;

export function parseHeader(b: Uint8Array): WtFrameHeader | null {
  if (b.length < HEADER) return null;
  const v = new DataView(b.buffer, b.byteOffset, b.byteLength);
  const us = Number(v.getBigUint64(0, true));
  return { capturedUnixMs: us / 1000, seq: v.getUint32(8, true), keyframe: (b[12]! & 1) === 1 };
}

function hexBytes(hex: string): Uint8Array {
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < out.length; i += 1) out[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  return out;
}

async function readAll(stream: ReadableStream<Uint8Array>): Promise<Uint8Array> {
  const reader = stream.getReader();
  const parts: Uint8Array[] = [];
  let size = 0;
  for (;;) {
    const { value, done } = await reader.read();
    if (done) break;
    parts.push(value);
    size += value.length;
  }
  const out = new Uint8Array(size);
  let at = 0;
  for (const p of parts) {
    out.set(p, at);
    at += p.length;
  }
  return out;
}

export interface WtStats {
  received: number;
  decoded: number;
  /** Frames skipped: late, after a gap, or before the first keyframe. */
  skipped: number;
  gaps: number;
  keyframeRequests: number;
  errors: number;
}

export class WtVideo {
  private transport: WebTransport | null = null;
  private decoder: VideoDecoder | null = null;
  private lastSeq: number | null = null;
  private waitingKeyframe = true;
  private captureOf = new Map<number, number>();
  /** Start of the last page frame and the refresh interval seen between page frames. */
  private lastFrameStart = 0;
  private refreshMs = 1000 / 60;
  readonly stats: WtStats = { received: 0, decoded: 0, skipped: 0, gaps: 0, keyframeRequests: 0, errors: 0 };

  constructor(
    private readonly canvas: HTMLCanvasElement,
    private readonly clock: () => ClockSync | null,
    private readonly askKeyframe: () => void,
    private readonly log: (m: string) => void,
  ) {}

  async start(offer: WtOffer): Promise<void> {
    const transport = new WebTransport(offer.url, {
      serverCertificateHashes: [{ algorithm: 'sha-256', value: hexBytes(offer.cert_sha256) }],
      congestionControl: 'low-latency',
    } as WebTransportOptions);
    this.transport = transport;
    await transport.ready;
    this.log('webtransport: соединение открыто');
    const g = this.canvas.getContext('2d');
    this.decoder = new VideoDecoder({
      output: (frame) => {
        const capture = this.captureOf.get(frame.timestamp);
        this.captureOf.delete(frame.timestamp);
        if (this.canvas.width !== frame.displayWidth) this.canvas.width = frame.displayWidth;
        if (this.canvas.height !== frame.displayHeight) this.canvas.height = frame.displayHeight;
        g?.drawImage(frame, 0, 0);
        frame.close();
        this.stats.decoded += 1;
        // Drawn now, composited in the next page frame and visible one refresh later: the moment that rVFC's
        // expectedDisplayTime gives for video, so both paths are measured alike.
        requestAnimationFrame((start) => {
          const gap = start - this.lastFrameStart;
          if (gap > 4 && gap < 25) this.refreshMs = this.refreshMs * 0.9 + gap * 0.1;
          this.lastFrameStart = start;
          const shown = start + this.refreshMs;
          recordShown(shown, capture !== undefined ? shown - capture : null, 'webtransport');
        });
      },
      error: (e) => {
        this.stats.errors += 1;
        this.log(`webtransport: декодер: ${String(e)}`);
        this.restartDecoder();
      },
    });
    this.configure();
    void this.receive(transport);
  }

  stop(): void {
    try {
      this.transport?.close();
    } catch {
      // already closed
    }
    this.transport = null;
    if (this.decoder && this.decoder.state !== 'closed') this.decoder.close();
    this.decoder = null;
  }

  private configure(): void {
    // High 4.2: 1080p60. Annex B input, so no description.
    this.decoder?.configure({ codec: 'avc1.64002a', optimizeForLatency: true, hardwareAcceleration: 'prefer-hardware' });
  }

  private restartDecoder(): void {
    if (!this.decoder) return;
    if (this.decoder.state === 'closed') return;
    this.decoder.reset();
    this.configure();
    this.waitForKeyframe();
  }

  private waitForKeyframe(): void {
    this.waitingKeyframe = true;
    this.stats.keyframeRequests += 1;
    this.askKeyframe();
  }

  private async receive(transport: WebTransport): Promise<void> {
    const reader = transport.incomingUnidirectionalStreams.getReader();
    for (;;) {
      let next: ReadableStreamReadResult<ReadableStream<Uint8Array>>;
      try {
        next = await reader.read();
      } catch (e) {
        this.log(`webtransport: приём закончен: ${String(e)}`);
        return;
      }
      if (next.done) return;
      void readAll(next.value).then(
        (bytes) => this.frame(bytes),
        () => {
          // A reset stream: the frame was too late on the server.
          this.stats.skipped += 1;
        },
      );
    }
  }

  private frame(bytes: Uint8Array): void {
    const h = parseHeader(bytes);
    const decoder = this.decoder;
    if (!h || !decoder || decoder.state !== 'configured') return;
    this.stats.received += 1;
    if (this.lastSeq !== null && h.seq <= this.lastSeq) {
      this.stats.skipped += 1;
      return;
    }
    if (this.lastSeq !== null && h.seq !== this.lastSeq + 1 && !h.keyframe) {
      this.stats.gaps += 1;
      this.stats.skipped += 1;
      this.lastSeq = h.seq;
      this.waitForKeyframe();
      return;
    }
    this.lastSeq = h.seq;
    if (this.waitingKeyframe && !h.keyframe) {
      this.stats.skipped += 1;
      return;
    }
    this.waitingKeyframe = false;
    const best = this.clock()?.best();
    const capture = best ? h.capturedUnixMs - best.offsetMs - performance.timeOrigin : undefined;
    const timestamp = h.seq * 1000;
    if (capture !== undefined) this.captureOf.set(timestamp, capture);
    if (this.captureOf.size > 120) this.captureOf.clear();
    decoder.decode(
      new EncodedVideoChunk({ type: h.keyframe ? 'key' : 'delta', timestamp, data: bytes.subarray(HEADER) }),
    );
  }
}
