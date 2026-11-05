// Per-frame latency from requestVideoFrameCallback (docs/HARDWARE.md, В браузере).
//
// The capture time of a frame comes from the browser (`captureTime`, from the abs-capture-time RTP extension) when
// it reports one, else from the frame's RTP timestamp and the server clock measured over the control channel.
// `expectedDisplayTime` is when the compositor plans to show the frame. The client's own display lag after that is
// not included.

import { captureTimeFromRtp, type ClockSync } from './clock';

export interface FrameTimes {
  captureToReceive: number | null;
  receiveToDisplay: number | null;
  captureToDisplay: number | null;
}

export interface LatencySummary {
  frames: number;
  /** Half the fastest ping round trip: how far the clock offset can be off. */
  clockErrorMs: number | null;
  captureToDisplayP50: number | null;
  captureToDisplayP95: number | null;
  captureToReceiveP50: number | null;
  receiveToDisplayP50: number | null;
}

type Metadata = VideoFrameCallbackMetadata & { captureTime?: number; receiveTime?: number; rtpTimestamp?: number };

/**
 * Capture time comes from the browser when it has one, else from the RTP timestamp and the server clock that the
 * control channel measured (docs/PROTOCOL.md). Chrome 157 does not report it for a receive-only stream.
 */
export function frameTimes(m: Metadata, clock: ClockSync | null = null, timeOrigin = performance.timeOrigin): FrameTimes {
  let cap = typeof m.captureTime === 'number' ? m.captureTime : null;
  const best = clock?.best() ?? null;
  if (cap === null && clock && clock.rtpEpochMs !== null && best && typeof m.rtpTimestamp === 'number') {
    cap = captureTimeFromRtp(m.rtpTimestamp, clock.rtpEpochMs, clock.rtpClock, best.offsetMs, timeOrigin, m.expectedDisplayTime);
  }
  const rec = typeof m.receiveTime === 'number' ? m.receiveTime : null;
  const show = m.expectedDisplayTime;
  return {
    captureToReceive: cap !== null && rec !== null ? rec - cap : null,
    receiveToDisplay: rec !== null ? show - rec : null,
    captureToDisplay: cap !== null ? show - cap : null,
  };
}

function percentile(values: number[], q: number): number | null {
  if (values.length === 0) return null;
  const sorted = [...values].sort((a, b) => a - b);
  return sorted[Math.min(sorted.length - 1, Math.max(0, Math.ceil(q * sorted.length) - 1))] ?? null;
}

export function summarizeFrames(frames: FrameTimes[], clockErrorMs: number | null = null): LatencySummary {
  const pick = (key: keyof FrameTimes) =>
    frames.map((f) => f[key]).filter((v): v is number => typeof v === 'number' && Number.isFinite(v));
  const c2d = pick('captureToDisplay');
  return {
    frames: frames.length,
    clockErrorMs,
    captureToDisplayP50: percentile(c2d, 0.5),
    captureToDisplayP95: percentile(c2d, 0.95),
    captureToReceiveP50: percentile(pick('captureToReceive'), 0.5),
    receiveToDisplayP50: percentile(pick('receiveToDisplay'), 0.5),
  };
}

/** Collects the times of the last `window` frames of a video element. */
export class LatencyMeter {
  private frames: FrameTimes[] = [];
  private handle: number | null = null;

  constructor(
    private readonly video: HTMLVideoElement,
    private readonly clock: () => ClockSync | null = () => null,
    private readonly window = 120,
  ) {}

  start(): void {
    if (this.handle !== null || !('requestVideoFrameCallback' in this.video)) return;
    const tick = (_now: number, metadata: VideoFrameCallbackMetadata) => {
      this.frames.push(frameTimes(metadata as Metadata, this.clock()));
      if (this.frames.length > this.window) this.frames.splice(0, this.frames.length - this.window);
      this.handle = this.video.requestVideoFrameCallback(tick);
    };
    this.handle = this.video.requestVideoFrameCallback(tick);
  }

  stop(): void {
    if (this.handle !== null) this.video.cancelVideoFrameCallback(this.handle);
    this.handle = null;
  }

  summary(): LatencySummary {
    const best = this.clock()?.best();
    return summarizeFrames(this.frames, best ? best.rttMs / 2 : null);
  }
}
