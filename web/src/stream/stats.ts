// Turns two getStats() samples into the numbers of the statistics panel.

export type StatsEntry = Record<string, unknown> & { type: string; id: string };
export type StatsMap = Map<string, StatsEntry>;

export interface StreamSummary {
  fps: number | null;
  width: number | null;
  height: number | null;
  bitrateMbps: number | null;
  /** Average time a frame waited in the jitter buffer, over the last interval. */
  jitterBufferMs: number | null;
  /** Average decode time per frame over the last interval. */
  decodeMs: number | null;
  /** From the first packet of a frame to the end of its decode, per frame over the last interval. */
  processingMs: number | null;
  rttMs: number | null;
  framesDecoded: number;
  framesDropped: number;
  packetsLost: number;
  nackCount: number;
  pliCount: number;
  firCount: number;
  freezeCount: number;
  decoder: string | null;
  powerEfficientDecoder: boolean | null;
  codec: string | null;
}

function num(entry: StatsEntry | undefined, key: string): number | null {
  const value = entry?.[key];
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

function str(entry: StatsEntry | undefined, key: string): string | null {
  const value = entry?.[key];
  return typeof value === 'string' ? value : null;
}

/** The inbound video RTP stream of a report. */
export function inboundVideo(report: StatsMap): StatsEntry | undefined {
  for (const entry of report.values()) {
    if (entry.type === 'inbound-rtp' && entry['kind'] === 'video') return entry;
  }
  return undefined;
}

function selectedPair(report: StatsMap): StatsEntry | undefined {
  for (const entry of report.values()) {
    if (entry.type === 'transport' && typeof entry['selectedCandidatePairId'] === 'string') {
      return report.get(entry['selectedCandidatePairId']);
    }
  }
  for (const entry of report.values()) {
    if (entry.type === 'candidate-pair' && entry['nominated'] === true && entry['state'] === 'succeeded') {
      return entry;
    }
  }
  return undefined;
}

/** Per-frame average of a cumulative time counter between two samples, in milliseconds. */
function perFrameMs(
  now: StatsEntry,
  before: StatsEntry | undefined,
  timeKey: string,
  countKey: string,
): number | null {
  const time = num(now, timeKey);
  const count = num(now, countKey);
  if (time === null || count === null) return null;
  const time0 = num(before, timeKey) ?? 0;
  const count0 = num(before, countKey) ?? 0;
  const frames = count - count0;
  if (frames <= 0) return null;
  return ((time - time0) / frames) * 1000;
}

export function summarize(now: StatsMap, before?: StatsMap): StreamSummary | null {
  const video = inboundVideo(now);
  if (!video) return null;
  const previous = before ? inboundVideo(before) : undefined;
  let bitrateMbps: number | null = null;
  const bytes = num(video, 'bytesReceived');
  const bytes0 = num(previous, 'bytesReceived');
  const t = num(video, 'timestamp');
  const t0 = num(previous, 'timestamp');
  if (bytes !== null && bytes0 !== null && t !== null && t0 !== null && t > t0) {
    bitrateMbps = ((bytes - bytes0) * 8) / ((t - t0) / 1000) / 1e6;
  }
  const pair = selectedPair(now);
  const rtt = num(pair, 'currentRoundTripTime');
  const codecId = str(video, 'codecId');
  const codec = codecId ? now.get(codecId) : undefined;
  const efficient = video['powerEfficientDecoder'];
  return {
    fps: num(video, 'framesPerSecond'),
    width: num(video, 'frameWidth'),
    height: num(video, 'frameHeight'),
    bitrateMbps,
    jitterBufferMs: perFrameMs(video, previous, 'jitterBufferDelay', 'jitterBufferEmittedCount'),
    decodeMs: perFrameMs(video, previous, 'totalDecodeTime', 'framesDecoded'),
    processingMs: perFrameMs(video, previous, 'totalProcessingDelay', 'framesDecoded'),
    rttMs: rtt === null ? null : rtt * 1000,
    framesDecoded: num(video, 'framesDecoded') ?? 0,
    framesDropped: num(video, 'framesDropped') ?? 0,
    packetsLost: num(video, 'packetsLost') ?? 0,
    nackCount: num(video, 'nackCount') ?? 0,
    pliCount: num(video, 'pliCount') ?? 0,
    firCount: num(video, 'firCount') ?? 0,
    freezeCount: num(video, 'freezeCount') ?? 0,
    decoder: str(video, 'decoderImplementation'),
    powerEfficientDecoder: typeof efficient === 'boolean' ? efficient : null,
    codec: codec ? [str(codec, 'mimeType'), str(codec, 'sdpFmtpLine')].filter(Boolean).join(' ') : null,
  };
}

/** Copies a live RTCStatsReport into plain objects, for summaries and the diagnostics file. */
export function toMap(report: RTCStatsReport): StatsMap {
  const map: StatsMap = new Map();
  report.forEach((value: unknown) => {
    const entry = { ...(value as object) } as StatsEntry;
    map.set(entry.id, entry);
  });
  return map;
}
