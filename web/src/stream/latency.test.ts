import { expect, test } from 'vitest';
import { frameTimes, summarizeFrames } from './latency';

function meta(extra: Record<string, number>): VideoFrameCallbackMetadata {
  return {
    presentationTime: 0,
    expectedDisplayTime: 100,
    width: 1920,
    height: 1080,
    mediaTime: 0,
    presentedFrames: 1,
    ...extra,
  } as VideoFrameCallbackMetadata;
}

test('frame times split capture, receive and display', () => {
  const t = frameTimes(meta({ captureTime: 80, receiveTime: 92 }));
  expect(t).toEqual({ captureToReceive: 12, receiveToDisplay: 8, captureToDisplay: 20 });
});

test('without a capture time only receive to display is known', () => {
  const t = frameTimes(meta({ receiveTime: 95 }));
  expect(t.captureToDisplay).toBeNull();
  expect(t.receiveToDisplay).toBe(5);
});

test('summary percentiles ignore frames without the value', () => {
  const frames = [10, 20, 30, 40].map((v) => ({ captureToReceive: null, receiveToDisplay: 1, captureToDisplay: v }));
  frames.push({ captureToReceive: null, receiveToDisplay: 1, captureToDisplay: null as unknown as number });
  const s = summarizeFrames(frames);
  expect(s.frames).toBe(5);
  expect(s.captureToDisplayP50).toBe(20);
  expect(s.captureToDisplayP95).toBe(40);
  expect(s.receiveToDisplayP50).toBe(1);
});
