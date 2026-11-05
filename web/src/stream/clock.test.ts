import { expect, test } from 'vitest';
import { ClockSync, captureTimeFromRtp, sampleFromPong } from './clock';

test('the offset is read at the middle of the round trip', () => {
  // Client sends at 100 ms, gets the answer at 104 ms; the server said 1_000_052 Unix ms.
  const s = sampleFromPong(100, 104, 1_000_052, 1_000_000 - 50);
  expect(s.rttMs).toBe(4);
  // Client Unix time at the midpoint: 999_950 + 102 = 1_000_052, so the clocks agree.
  expect(s.offsetMs).toBe(0);
});

test('the fastest exchange wins', () => {
  const c = new ClockSync(3);
  c.add({ rttMs: 10, offsetMs: 5 });
  c.add({ rttMs: 2, offsetMs: 1 });
  c.add({ rttMs: 7, offsetMs: 3 });
  expect(c.best()).toEqual({ rttMs: 2, offsetMs: 1 });
  c.add({ rttMs: 8, offsetMs: 4 });
  c.add({ rttMs: 9, offsetMs: 4 });
  c.add({ rttMs: 9, offsetMs: 4 });
  expect(c.best()?.rttMs).toBe(8);
});

test('rtp time maps to the capture moment on the client timeline', () => {
  const timeOrigin = 1_700_000_000_000;
  const offset = 250; // the server clock is 250 ms ahead
  const epoch = timeOrigin + 1000 + offset; // RTP 0 was captured at client perf time 1000
  // A frame 2 s after RTP 0, shown at perf 3020: capture is at perf 3000.
  const t = captureTimeFromRtp(2 * 90000, epoch, 90000, offset, timeOrigin, 3020);
  expect(t).toBeCloseTo(3000, 6);
});

test('a wrapped 32-bit rtp time is placed next to now', () => {
  const timeOrigin = 0;
  const wrapMs = (2 ** 32 / 90000) * 1000;
  const epoch = 0;
  // The true capture is one wrap plus 1 s after the epoch; the RTP field only holds the 1 s.
  const now = wrapMs + 1010;
  const t = captureTimeFromRtp(90000, epoch, 90000, 0, timeOrigin, now);
  expect(t).toBeCloseTo(wrapMs + 1000, 3);
});
