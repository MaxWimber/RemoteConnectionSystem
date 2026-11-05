// "Скачать диагностику": what the browser saw, as one JSON file (docs/ARCHITECTURE.md, Диагностика).

import type { LatencySummary } from './latency';
import type { StatsMap, StreamSummary } from './stats';

const MAX_EVENTS = 500;
const MAX_SAMPLES = 300;

interface LogEvent {
  at: string;
  message: string;
}

const events: LogEvent[] = [];
const errors: LogEvent[] = [];
const samples: Array<{ at: string; summary: StreamSummary; latency: LatencySummary | null }> = [];

function push<T>(list: T[], item: T, max: number): void {
  list.push(item);
  if (list.length > max) list.splice(0, list.length - max);
}

export function logEvent(message: string): void {
  push(events, { at: new Date().toISOString(), message }, MAX_EVENTS);
}

export function recordSample(summary: StreamSummary, latency: LatencySummary | null = null): void {
  push(samples, { at: new Date().toISOString(), summary, latency }, MAX_SAMPLES);
}

let installed = false;

/** Keeps console errors and uncaught failures for the file. */
export function installErrorCapture(): void {
  if (installed) return;
  installed = true;
  const original = console.error.bind(console);
  console.error = (...args: unknown[]) => {
    push(errors, { at: new Date().toISOString(), message: args.map(String).join(' ') }, MAX_EVENTS);
    original(...args);
  };
  window.addEventListener('error', (e) =>
    push(errors, { at: new Date().toISOString(), message: `${e.message} (${e.filename}:${e.lineno})` }, MAX_EVENTS),
  );
  window.addEventListener('unhandledrejection', (e) =>
    push(errors, { at: new Date().toISOString(), message: `unhandled: ${String(e.reason)}` }, MAX_EVENTS),
  );
}

function device(): Record<string, unknown> {
  const nav = navigator as Navigator & {
    deviceMemory?: number;
    connection?: { effectiveType?: string; downlink?: number; rtt?: number; type?: string };
    userAgentData?: { brands?: unknown; mobile?: boolean; platform?: string };
  };
  return {
    userAgent: nav.userAgent,
    userAgentData: nav.userAgentData
      ? { brands: nav.userAgentData.brands, mobile: nav.userAgentData.mobile, platform: nav.userAgentData.platform }
      : null,
    language: nav.language,
    hardwareConcurrency: nav.hardwareConcurrency,
    deviceMemory: nav.deviceMemory ?? null,
    connection: nav.connection
      ? {
          type: nav.connection.type,
          effectiveType: nav.connection.effectiveType,
          downlink: nav.connection.downlink,
          rtt: nav.connection.rtt,
        }
      : null,
    screen: {
      width: screen.width,
      height: screen.height,
      devicePixelRatio: window.devicePixelRatio,
      colorDepth: screen.colorDepth,
    },
    viewport: { width: window.innerWidth, height: window.innerHeight },
  };
}

export function buildReport(stats: StatsMap | null, agentStatus: unknown): Record<string, unknown> {
  return {
    created: new Date().toISOString(),
    page: window.location.pathname,
    device: device(),
    events,
    errors,
    samples,
    agent: agentStatus,
    getStats: stats ? Array.from(stats.values()) : null,
  };
}

export function download(report: Record<string, unknown>): void {
  const blob = new Blob([JSON.stringify(report, null, 2)], { type: 'application/json' });
  const url = URL.createObjectURL(blob);
  const a = document.createElement('a');
  a.href = url;
  a.download = `rcs-diagnostics-${new Date().toISOString().replace(/[:.]/g, '-')}.json`;
  document.body.appendChild(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}
