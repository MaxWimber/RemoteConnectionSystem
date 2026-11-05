// The server clock seen from the browser, for capture -> display latency (docs/PROTOCOL.md, control).
//
// All client times are on the performance timeline (performance.now()); `timeOrigin` turns them into Unix ms.

export interface ClockSample {
  rttMs: number;
  /** Server Unix ms minus client Unix ms. */
  offsetMs: number;
}

/** One ping/pong exchange: the server clock read in the middle of the round trip. */
export function sampleFromPong(sentPerf: number, receivedPerf: number, serverMs: number, timeOrigin: number): ClockSample {
  const rttMs = receivedPerf - sentPerf;
  const clientMid = timeOrigin + sentPerf + rttMs / 2;
  return { rttMs, offsetMs: serverMs - clientMid };
}

/** Keeps the last exchanges and trusts the fastest one: its midpoint guess is the tightest. */
export class ClockSync {
  private samples: ClockSample[] = [];
  rtpEpochMs: number | null = null;
  rtpClock = 90000;

  constructor(private readonly keep = 16) {}

  add(sample: ClockSample): void {
    if (!Number.isFinite(sample.rttMs) || sample.rttMs < 0) return;
    this.samples.push(sample);
    if (this.samples.length > this.keep) this.samples.splice(0, this.samples.length - this.keep);
  }

  best(): ClockSample | null {
    let best: ClockSample | null = null;
    for (const s of this.samples) if (!best || s.rttMs < best.rttMs) best = s;
    return best;
  }
}

const RTP_WRAP = 2 ** 32;

/**
 * Capture time of a frame on the client's performance timeline, from its RTP timestamp. The 32-bit RTP time wraps
 * every 13 hours at 90 kHz; the wrap closest to `nowPerf` is taken.
 */
export function captureTimeFromRtp(
  rtp: number,
  rtpEpochMs: number,
  rtpClock: number,
  offsetMs: number,
  timeOrigin: number,
  nowPerf: number,
): number {
  const wrapMs = (RTP_WRAP / rtpClock) * 1000;
  const nowServerMs = timeOrigin + nowPerf + offsetMs;
  const base = rtpEpochMs + (rtp / rtpClock) * 1000;
  const k = Math.round((nowServerMs - base) / wrapMs);
  const captureServerMs = base + k * wrapMs;
  return captureServerMs - offsetMs - timeOrigin;
}
