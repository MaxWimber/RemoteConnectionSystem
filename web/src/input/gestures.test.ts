import { describe, expect, it } from 'vitest';
import { ContactIds, DOUBLE_TAP_MS, LONG_PRESS_MS, TouchpadGestures, type GestureAction } from './gestures';
import { textChange, touchpadGain } from './touch';

function recorder(scale = 2) {
  const actions: GestureAction[] = [];
  const g = new TouchpadGestures((a) => actions.push(a), () => scale);
  return { g, actions };
}

const click = (button: 'left' | 'right') => [
  { type: 'button', button, down: true },
  { type: 'button', button, down: false },
];

describe('touchpad gestures', () => {
  it('moves the pointer by the scaled finger movement', () => {
    const { g, actions } = recorder(3);
    g.down(1, 100, 100, 0);
    g.move(1, 110, 95);
    g.up(1, 500);
    expect(actions).toEqual([{ type: 'move', dx: 30, dy: -15 }]);
  });

  it('a short tap is a left click', () => {
    const { g, actions } = recorder();
    g.down(1, 50, 50, 0);
    g.move(1, 53, 52);
    g.up(1, 120);
    expect(actions.filter((a) => a.type === 'button')).toEqual(click('left'));
  });

  it('tap and then hold and move drags with the left button', () => {
    const { g, actions } = recorder(1);
    g.down(1, 50, 50, 0);
    g.up(1, 100);
    actions.length = 0;
    g.down(1, 52, 52, 100 + DOUBLE_TAP_MS - 50);
    g.move(1, 80, 52);
    g.up(1, 2000);
    expect(actions).toEqual([
      { type: 'button', button: 'left', down: true },
      { type: 'move', dx: 28, dy: 0 },
      { type: 'button', button: 'left', down: false },
    ]);
  });

  it('a long press is a right click, and the release adds nothing', () => {
    const { g, actions } = recorder();
    g.down(1, 50, 50, 0);
    g.tick(LONG_PRESS_MS - 1);
    expect(actions).toEqual([]);
    g.tick(LONG_PRESS_MS);
    g.tick(LONG_PRESS_MS + 100);
    g.up(1, LONG_PRESS_MS + 200);
    expect(actions).toEqual(click('right'));
  });

  it('a two-finger tap is a right click', () => {
    const { g, actions } = recorder();
    g.down(1, 50, 50, 0);
    g.down(2, 90, 50, 20);
    g.up(1, 120);
    g.up(2, 130);
    expect(actions).toEqual(click('right'));
  });

  it('two fingers scroll, fingers down scroll up', () => {
    const { g, actions } = recorder();
    g.down(1, 50, 50, 0);
    g.down(2, 90, 50, 0);
    g.move(1, 50, 70);
    g.move(2, 90, 70);
    g.up(1, 900);
    g.up(2, 900);
    expect(actions).toEqual([
      { type: 'wheel', dx: -0, dy: 30 },
      { type: 'wheel', dx: -0, dy: 30 },
    ]);
  });

  it('cancel lets go of a drag', () => {
    const { g, actions } = recorder();
    g.down(1, 50, 50, 0);
    g.up(1, 50);
    g.down(1, 50, 50, 100);
    g.cancel();
    expect(actions.slice(-1)).toEqual([{ type: 'button', button: 'left', down: false }]);
  });
});

describe('contact ids', () => {
  it('reuses the lowest free id and runs out after ten', () => {
    const ids = new ContactIds();
    expect(ids.take(101)).toBe(0);
    expect(ids.take(102)).toBe(1);
    expect(ids.take(101)).toBe(0);
    ids.free(101);
    expect(ids.take(103)).toBe(0);
    for (let p = 200; p < 208; p += 1) ids.take(p);
    expect(ids.take(999)).toBeUndefined();
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

describe('touchpad gain', () => {
  it('is 1.5 for slow moves and grows to 4 for quick ones', () => {
    expect(touchpadGain(0)).toBe(1.5);
    expect(touchpadGain(0.3)).toBe(1.5);
    expect(touchpadGain(1.3)).toBeCloseTo(3);
    expect(touchpadGain(10)).toBe(4);
  });
});