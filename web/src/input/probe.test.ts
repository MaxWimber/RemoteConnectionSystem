import { describe, expect, it } from 'vitest';
import { luminance, summarize } from './probe';

describe('input latency probe', () => {
  it('tells black from white', () => {
    expect(luminance(new Uint8ClampedArray([0, 0, 0, 255, 0, 0, 0, 255]))).toBe(0);
    expect(luminance(new Uint8ClampedArray([255, 255, 255, 255]))).toBeCloseTo(255);
    expect(luminance(new Uint8ClampedArray([128, 128, 128, 255]))).toBeCloseTo(128);
    expect(luminance(new Uint8ClampedArray())).toBe(0);
  });

  it('summarizes the trials', () => {
    const r = summarize([40, 10, 30, 20], 5);
    expect(r).toMatchObject({ trials: 5, misses: 1, p50: 30, p95: 40, min: 10, max: 40 });
    expect(summarize([], 3)).toMatchObject({ misses: 3, p50: null, min: null });
  });
});
