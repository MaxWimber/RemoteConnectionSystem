// Per-frame latency from requestVideoFrameCallback (docs/HARDWARE.md, В браузере).
//
// The server writes the capture time of every frame into the abs-capture-time RTP extension. Chrome turns it into
// `captureTime` on this page's clock, using the sender reports to estimate the offset between the two clocks.
// `expectedDisplayTime` is when the compositor plans to show the frame. The client's own display lag after that is
// not included.

export interface FrameTimes {
  captureToReceive: number | null;
  receiveToDisplay: number | null;
  captureToDisplay: number | null;
}

export interface LatencySummary {
  frames: number;
  captureToDisplayP50: number | null;
  captureToDisplayP95: number | null;
  captureToReceiveP50: number | null;
  receiveToDisplayP50: number | null;
}

type Metadata = VideoFrameCallbackMetadata & { captureTime?: number; receiveTime?: number };

export function frameTimes(m: Metadata): FrameTimes {
  const cap = typeof m.captureTime === 'number' ? m.captureTime : null;
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

export function summarizeFrames(frames: FrameTimes[]): LatencySummary {
  const pick = (key: keyof FrameTimes) =>
    frames.map((f) => f[key]).filter((v): v is number => typeof v === 'number' && Number.isFinite(v));
  const c2d = pick('captureToDisplay');
  return {
    frames: frames.length,
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
    private readonly window = 120,
  ) {}

  start(): void {
    if (this.handle !== null || !('requestVideoFrameCallback' in this.video)) return;
    const tick = (_now: number, metadata: VideoFrameCallbackMetadata) => {
      this.frames.push(frameTimes(metadata as Metadata));
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
    return summarizeFrames(this.frames);
  }
}
