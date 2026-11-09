// Sound against picture (P9-1): the test window on the server (`rcs-bench avsync`) flashes a square and clicks at
// the same moment once a second. Here the flash is found in the shown frames and the click in the received sound,
// both in the browser's own clock, without playing anything out loud.

import { luminance, summarize, type ProbeResult } from '../input/probe';

/** Centre of the test window's 400 px square at the top-left corner of a 1920x1080 screen. */
const SQUARE = { x: 200 / 1920, y: 200 / 1080, size: 16 };
/** A click belongs to a flash when it comes no further than this from it. */
const PAIR_WINDOW_MS = 400;

export interface AvSyncResult extends ProbeResult {
  flashes: number;
  clicks: number;
  /** WebAudio's own output latency, already in the click times. */
  outputLatencyMs: number | null;
}

/** For each flash the nearest click inside the window: click minus flash, positive when the sound is late. */
export function pairEvents(flashes: number[], clicks: number[], windowMs = PAIR_WINDOW_MS): number[] {
  const out: number[] = [];
  for (const f of flashes) {
    let best: number | null = null;
    for (const c of clicks) {
      const d = c - f;
      if (Math.abs(d) <= windowMs && (best === null || Math.abs(d) < Math.abs(best))) best = d;
    }
    if (best !== null) out.push(best);
  }
  return out;
}

type FrameCallback = (now: number, meta: { expectedDisplayTime: number }) => void;
type VideoWithFrames = HTMLVideoElement & {
  requestVideoFrameCallback(cb: FrameCallback): number;
  cancelVideoFrameCallback(id: number): void;
};

export async function probeAvSync(
  video: HTMLVideoElement,
  sound: MediaStream,
  base: string,
  seconds = 20,
  onProgress?: (flashes: number, clicks: number) => void,
): Promise<AvSyncResult> {
  const v = video as VideoWithFrames;
  if (typeof v.requestVideoFrameCallback !== 'function') throw new Error('requestVideoFrameCallback недоступен');
  const canvas = document.createElement('canvas');
  canvas.width = SQUARE.size;
  canvas.height = SQUARE.size;
  const g = canvas.getContext('2d', { willReadFrequently: true });
  if (!g) throw new Error('нет canvas 2d');

  const ctx = new AudioContext({ latencyHint: 'interactive' });
  await ctx.audioWorklet.addModule(`${base}av-meter.js`);
  await ctx.resume();
  const source = ctx.createMediaStreamSource(sound);
  const meter = new AudioWorkletNode(ctx, 'click-meter');
  const mute = ctx.createGain();
  mute.gain.value = 0;
  source.connect(meter).connect(mute).connect(ctx.destination);

  const flashes: number[] = [];
  const clicks: number[] = [];
  meter.port.onmessage = (e: MessageEvent<number>) => {
    const ts = ctx.getOutputTimestamp();
    if (ts.contextTime === undefined || ts.performanceTime === undefined) return;
    clicks.push(ts.performanceTime + (e.data - ts.contextTime) * 1000);
    onProgress?.(flashes.length, clicks.length);
  };

  let bright = false;
  let frameId = 0;
  const onFrame: FrameCallback = (_, meta) => {
    const w = v.videoWidth;
    const h = v.videoHeight;
    if (w > 0 && h > 0) {
      const x = SQUARE.x * w - SQUARE.size / 2;
      const y = SQUARE.y * h - SQUARE.size / 2;
      g.drawImage(v, x, y, SQUARE.size, SQUARE.size, 0, 0, SQUARE.size, SQUARE.size);
      const now = luminance(g.getImageData(0, 0, SQUARE.size, SQUARE.size).data) > 128;
      if (now && !bright) {
        flashes.push(meta.expectedDisplayTime);
        onProgress?.(flashes.length, clicks.length);
      }
      bright = now;
    }
    frameId = v.requestVideoFrameCallback(onFrame);
  };
  frameId = v.requestVideoFrameCallback(onFrame);
  await new Promise((r) => window.setTimeout(r, seconds * 1000));
  v.cancelVideoFrameCallback(frameId);
  const outputLatency = (ctx as AudioContext & { outputLatency?: number }).outputLatency;
  source.disconnect();
  await ctx.close();

  const skews = pairEvents(flashes, clicks);
  return {
    ...summarize(skews, flashes.length),
    flashes: flashes.length,
    clicks: clicks.length,
    outputLatencyMs: typeof outputLatency === 'number' ? outputLatency * 1000 : null,
  };
}
