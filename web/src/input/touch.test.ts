import { describe, expect, it } from 'vitest';
import { ContactIds, textChange } from './touch';

describe('contact ids', () => {
  it('gives each new finger the next id and runs out after ten at once', () => {
    const ids = new ContactIds();
    expect(ids.take(101)).toBe(0);
    expect(ids.take(102)).toBe(1);
    expect(ids.take(101)).toBe(0);
    ids.free(101);
    // A freed id does not come back soon: a late copy of its messages must not meet a new finger (D-048).
    expect(ids.take(103)).toBe(2);
    for (let p = 200; p < 208; p += 1) ids.take(p);
    expect(ids.take(999)).toBeUndefined();
    ids.clear();
    // After 255 the ids wrap, skipping those in use.
    for (let p = 0; p < 300; p += 1) {
      ids.take(p);
      ids.free(p);
    }
    expect(ids.take(5000)).toBe((11 + 300) % 256);
  });
});

describe('screen keyboard text', () => {
  it('finds what was added and removed at the end', () => {
    expect(textChange('', 'hel')).toEqual({ deletes: 0, insert: 'hel' });
    expect(textChange('hel', 'hell')).toEqual({ deletes: 0, insert: 'l' });
    expect(textChange('teh', 'the ')).toEqual({ deletes: 2, insert: 'he ' });
    expect(textChange('abc', 'ab')).toEqual({ deletes: 1, insert: '' });
    expect(textChange('a😀', 'a')).toEqual({ deletes: 1, insert: '' });
    expect(textChange('a😀', 'a😃')).toEqual({ deletes: 1, insert: '😃' });
  });
});
