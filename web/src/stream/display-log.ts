// Every shown frame of either video path with its capture -> display time (P9-4): the measurement reads it from
// `window.__rcsDisplay` and works out the same latency and freeze numbers for WebRTC and WebTransport.

export type VideoPath = 'webrtc' | 'webtransport';

export interface Shown {
  /** When the frame is shown, performance.now() milliseconds. */
  at: number;
  latency: number | null;
  path: VideoPath;
}

const MAX = 7200;
const log: Shown[] = [];

export function recordShown(at: number, latency: number | null, path: VideoPath): void {
  log.push({ at, latency, path });
  if (log.length > MAX) log.splice(0, log.length - MAX);
}

export interface DisplaySummary {
  frames: number;
  fps: number;
  p50: number | null;
  p95: number | null;
  max: number | null;
  /** Gaps between shown frames over `freezeMs`, and the time they took. */
  freezes: number;
  frozenMs: number;
}

/** The numbers of one path over the frames shown since `from`. */
export function summarizeShown(entries: Shown[], path: VideoPath, from: number, freezeMs = 100): DisplaySummary {
  const own = entries.filter((e) => e.path === path && e.at >= from);
  const lat = own.map((e) => e.latency).filter((v): v is number => v !== null && Number.isFinite(v));
  lat.sort((a, b) => a - b);
  const at = (q: number) => (lat.length ? lat[Math.min(lat.length - 1, Math.ceil(q * lat.length) - 1)]! : null);
  let freezes = 0;
  let frozenMs = 0;
  for (let i = 1; i < own.length; i += 1) {
    const gap = own[i]!.at - own[i - 1]!.at;
    if (gap > freezeMs) {
      freezes += 1;
      frozenMs += gap;
    }
  }
  const span = own.length > 1 ? own[own.length - 1]!.at - own[0]!.at : 0;
  return {
    frames: own.length,
    fps: span > 0 ? ((own.length - 1) * 1000) / span : 0,
    p50: at(0.5),
    p95: at(0.95),
    max: lat.length ? lat[lat.length - 1]! : null,
    freezes,
    frozenMs,
  };
}

if (typeof window !== 'undefined') {
  (window as unknown as { __rcsDisplay: unknown }).__rcsDisplay = {
    log,
    summary: (path: VideoPath, from = 0) => summarizeShown(log, path, from),
  };
}
