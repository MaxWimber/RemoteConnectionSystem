import { describe, expect, test } from 'vitest';
import { summarize, type StatsEntry, type StatsMap } from './stats';

function report(entries: StatsEntry[]): StatsMap {
  return new Map(entries.map((e) => [e.id, e]));
}

function inbound(extra: Record<string, unknown>): StatsEntry {
  return { id: 'in', type: 'inbound-rtp', kind: 'video', codecId: 'c', ...extra };
}

const codec: StatsEntry = {
  id: 'c',
  type: 'codec',
  mimeType: 'video/H264',
  sdpFmtpLine: 'packetization-mode=1;profile-level-id=64001f',
};
const transport: StatsEntry = { id: 't', type: 'transport', selectedCandidatePairId: 'p' };
const pair: StatsEntry = { id: 'p', type: 'candidate-pair', currentRoundTripTime: 0.002 };

describe('summarize', () => {
  test('per-frame times and bitrate come from the difference of two samples', () => {
    const before = report([
      inbound({
        timestamp: 1000,
        bytesReceived: 0,
        jitterBufferDelay: 1,
        jitterBufferEmittedCount: 100,
        totalDecodeTime: 0.5,
        framesDecoded: 100,
      }),
      codec,
    ]);
    const now = report([
      inbound({
        timestamp: 2000,
        bytesReceived: 3_750_000,
        jitterBufferDelay: 1.6,
        jitterBufferEmittedCount: 160,
        totalDecodeTime: 0.62,
        framesDecoded: 160,
        framesPerSecond: 60,
        frameWidth: 1920,
        frameHeight: 1080,
      }),
      codec,
      transport,
      pair,
    ]);
    const s = summarize(now, before);
    expect(s).not.toBeNull();
    expect(s!.bitrateMbps).toBeCloseTo(30, 5);
    expect(s!.jitterBufferMs).toBeCloseTo(10, 5);
    expect(s!.decodeMs).toBeCloseTo(2, 5);
    expect(s!.rttMs).toBeCloseTo(2, 5);
    expect(s!.fps).toBe(60);
    expect(s!.codec).toContain('video/H264');
  });

  test('no video stream gives no summary, and one sample gives no rates', () => {
    expect(summarize(report([codec]))).toBeNull();
    const s = summarize(report([inbound({ framesDecoded: 5 })]));
    expect(s!.bitrateMbps).toBeNull();
    expect(s!.framesDecoded).toBe(5);
  });
});
