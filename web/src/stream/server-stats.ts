// The agent's once-a-second `stats` on the control channel and the client's bitrate limit (D-029, PROTOCOL.md).

export interface ServerStats {
  adaptive: boolean;
  targetKbps: number;
  estimateKbps: number | null;
  sentKbps: number;
  outKbps: number;
  /** 0..1 from the browser's reports, null until the first report. */
  loss: number | null;
  rttMs: number | null;
  minKbps: number;
  maxKbps: number;
  rateChanges: number;
}

const num = (v: unknown): number | null => (typeof v === 'number' && Number.isFinite(v) ? v : null);

/** A `stats` message, or null for anything else or a malformed one. */
export function parseServerStats(m: Record<string, unknown>): ServerStats | null {
  if (m['t'] !== 'stats') return null;
  const target = num(m['target_kbps']);
  const sent = num(m['sent_kbps']);
  const out = num(m['out_kbps']);
  const min = num(m['min_kbps']);
  const max = num(m['max_kbps']);
  if (target === null || sent === null || out === null || min === null || max === null) return null;
  return {
    adaptive: m['adaptive'] === true,
    targetKbps: target,
    estimateKbps: num(m['estimate_kbps']),
    sentKbps: sent,
    outKbps: out,
    loss: num(m['loss']),
    rttMs: num(m['rtt_ms']),
    minKbps: min,
    maxKbps: max,
    rateChanges: num(m['rate_changes']) ?? 0,
  };
}

/** Choices for the limit, kbit/s; null is the server's own limit. */
export const BITRATE_LIMITS: readonly (number | null)[] = [null, 30000, 20000, 10000, 5000, 3000];

const KEY = 'rcs.maxBitrateKbps';

/** The limit chosen on this browser. Storage may be missing or refuse: then the server's limit. */
export function loadBitrateLimit(storage: Pick<Storage, 'getItem'> | null = safeStorage()): number | null {
  try {
    const v = Number(storage?.getItem(KEY) ?? '');
    return BITRATE_LIMITS.includes(v) && v > 0 ? v : null;
  } catch {
    return null;
  }
}

export function saveBitrateLimit(
  kbps: number | null,
  storage: Pick<Storage, 'setItem' | 'removeItem'> | null = safeStorage(),
): void {
  try {
    if (kbps === null) storage?.removeItem(KEY);
    else storage?.setItem(KEY, String(kbps));
  } catch {
    // Only this browser forgets the choice; the current connection still gets it.
  }
}

function safeStorage(): Storage | null {
  try {
    return typeof localStorage === 'undefined' ? null : localStorage;
  } catch {
    return null;
  }
}

export function settingsMessage(kbps: number | null): string {
  return JSON.stringify({ t: 'settings', max_bitrate_kbps: kbps });
}

/** «12,3 Мбит/с» from kbit/s. */
export function mbps(kbps: number | null): string {
  return kbps === null ? '—' : (kbps / 1000).toFixed(1).replace('.', ',');
}
