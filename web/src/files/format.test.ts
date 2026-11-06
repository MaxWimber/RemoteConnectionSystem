import { describe, expect, it } from 'vitest';
import { formatEta, formatSize, formatSpeed } from './format';
import { joinPath, parentPath } from './api';

describe('format', () => {
  it('sizes', () => {
    expect(formatSize(0)).toBe('0 Б');
    expect(formatSize(1023)).toBe('1023 Б');
    expect(formatSize(1536)).toBe('1,50 КБ');
    expect(formatSize(10 * 1024 ** 3)).toBe('10,0 ГБ');
    expect(formatSize(200 * 1024 ** 3)).toBe('200 ГБ');
    expect(formatSpeed(12.5 * 1024 ** 2)).toBe('12,5 МБ/с');
  });

  it('time left', () => {
    expect(formatEta(12.4)).toBe('12 с');
    expect(formatEta(187)).toBe('3 мин 07 с');
    expect(formatEta(7500)).toBe('2 ч 05 мин');
    expect(formatEta(Infinity)).toBe('');
  });

  it('paths', () => {
    expect(joinPath('', 'a')).toBe('a');
    expect(joinPath('a/b', 'c')).toBe('a/b/c');
    expect(parentPath('a/b/c')).toBe('a/b');
    expect(parentPath('a')).toBe('');
  });
});
