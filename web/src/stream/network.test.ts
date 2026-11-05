import { describe, expect, it } from 'vitest';
import { describeConnection } from './network';

describe('describeConnection', () => {
  it('lists what the browser reports', () => {
    const c = Object.assign(new EventTarget(), { type: 'cellular', effectiveType: '4g', downlink: 1.7, rtt: 150 });
    expect(describeConnection(c)).toBe('cellular, 4g, 1.7 Мбит/с, rtt 150 мс');
  });

  it('says so when there is nothing', () => {
    expect(describeConnection(undefined)).toBe('нет данных');
    expect(describeConnection(new EventTarget())).toBe('нет данных');
  });
});
