// Input latency in the browser's own clock (D-022): press a key, watch the picture until the test page on the
// server (input-test.html, square in the top-left corner) changes colour. No clock sync is involved.

import type { InputChannels } from './channels';

/** F24: no program reacts to it, the test page flips on any key. */
const PROBE_SCANCODE = 0x76;
/** Centre of the 240 px square of input-test.html, in screen pixels of the server. */
const SAMPLE = { x: 120, y: 120, size: 16 };
const TRIAL_TIMEOUT_MS = 1500;

export interface ProbeResult {
  trials: number;
  misses: number;
  /** Press to the display of the first changed frame, ms. */
  samples: number[];
  p50: number | null;
  p95: number | null;
  min: number | null;
  max: number | null;
}

/** Mean luminance 0..255 of RGBA pixels. */
export function luminance(rgba: Uint8ClampedArray): number {
  let sum = 0;
  const n = rgba.length / 4;
  for (let i = 0; i < rgba.length; i += 4) {
    sum += 0.2126 * rgba[i]! + 0.7152 * rgba[i + 1]! + 0.0722 * rgba[i + 2]!;
  }
  return n > 0 ? sum / n : 0;
}

export function summarize(samples: number[], trials: number): ProbeResult {
  const sorted = [...samples].sort((a, b) => a - b);
  const at = (p: number) => (sorted.length ? sorted[Math.min(sorted.length - 1, Math.floor(p * sorted.length))]! : null);
  return {
    trials,
    misses: trials - samples.length,
    samples,
    p50: at(0.5),
    p95: at(0.95),
    min: sorted[0] ?? null,
    max: sorted[sorted.length - 1] ?? null,
  };
}

type FrameCallback = (now: number, meta: { expectedDisplayTime: number }) => void;
type VideoWithFrames = HTMLVideoElement & { requestVideoFrameCallback(cb: FrameCallback): number };

function sleep(ms: number): Promise<void> {
  return new Promise((r) => window.setTimeout(r, ms));
}

/** Runs `trials` presses one after another and reports the latency of each. */
export async function probeInputLatency(
  video: HTMLVideoElement,
  channels: () => InputChannels | null,
  trials = 30,
  onProgress?: (done: number) => void,
): Promise<ProbeResult> {
  const v = video as VideoWithFrames;
  if (typeof v.requestVideoFrameCallback !== 'function') throw new Error('requestVideoFrameCallback недоступен');
  const canvas = document.createElement('canvas');
  canvas.width = SAMPLE.size;
  canvas.height = SAMPLE.size;
  const ctx = canvas.getContext('2d', { willReadFrequently: true });
  if (!ctx) throw new Error('нет canvas 2d');
  const read = () => {
    ctx.drawImage(v, SAMPLE.x - SAMPLE.size / 2, SAMPLE.y - SAMPLE.size / 2, SAMPLE.size, SAMPLE.size, 0, 0, SAMPLE.size, SAMPLE.size);
    return luminance(ctx.getImageData(0, 0, SAMPLE.size, SAMPLE.size).data);
  };
  const nextFrame = () => new Promise<number>((resolve) => v.requestVideoFrameCallback((_, m) => resolve(m.expectedDisplayTime)));

  const samples: number[] = [];
  for (let i = 0; i < trials; i += 1) {
    const c = channels();
    if (!c?.open) throw new Error('нет соединения');
    await nextFrame();
    const before = read() > 128;
    const sent = performance.now();
    c.send({ type: 'key', down: true, scancode: PROBE_SCANCODE });
    c.send({ type: 'key', down: false, scancode: PROBE_SCANCODE });
    while (performance.now() - sent < TRIAL_TIMEOUT_MS) {
      const shown = await nextFrame();
      if (read() > 128 !== before) {
        samples.push(shown - sent);
        break;
      }
    }
    onProgress?.(i + 1);
    // A pause of varying length, so the presses do not lock onto the frame rhythm.
    await sleep(200 + Math.random() * 150);
  }
  return summarize(samples, trials);
}
