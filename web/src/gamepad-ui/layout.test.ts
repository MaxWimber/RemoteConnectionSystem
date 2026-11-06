import { describe, expect, it } from 'vitest';
import { XB } from '../input/gamepads';
import { DEFAULT_LAYOUT, parseLayout, resize, screenPadState, serializeLayout, stickVector } from './layout';

describe('on-screen gamepad layout', () => {
  it('saves and loads positions and sizes', () => {
    const layout = parseLayout(null);
    layout[0] = { ...layout[0]!, x: 20, y: 60, size: 35 };
    const back = parseLayout(serializeLayout(layout));
    expect(back[0]).toMatchObject({ id: 'ls', x: 20, y: 60, size: 35 });
    expect(back).toHaveLength(DEFAULT_LAYOUT.length);
  });

  it('falls back to the default for broken or foreign data and keeps controls on screen', () => {
    expect(parseLayout('not json')).toEqual(DEFAULT_LAYOUT);
    expect(parseLayout('{"a":1}')).toEqual(DEFAULT_LAYOUT);
    const odd = parseLayout(JSON.stringify([{ id: 'a', x: 500, y: -3, size: 1 }, { id: 'zz', x: 1 }]));
    expect(odd.find((c) => c.id === 'a')).toMatchObject({ x: 100, y: 0, size: 5 });
    expect(odd.find((c) => c.id === 'zz')).toBeUndefined();
  });

  it('limits sizes', () => {
    const c = DEFAULT_LAYOUT[2]!;
    expect(resize(c, 100).size).toBe(45);
    expect(resize(c, -100).size).toBe(5);
  });
});

describe('on-screen gamepad state', () => {
  it('stays inside the stick circle', () => {
    expect(stickVector(0, 0, 50)).toEqual([0, 0]);
    expect(stickVector(25, 0, 50)).toEqual([0.5, 0]);
    const [x, y] = stickVector(300, 400, 50);
    expect(Math.hypot(x, y)).toBeCloseTo(1);
    expect(x / y).toBeCloseTo(0.75);
  });

  it('turns held controls into an XInput state', () => {
    const s = screenPadState({ pressed: new Set(['a', 'lb', 'rt', 'up', 'nothing']), sticks: { ls: [1, 0], rs: [0, -1] } });
    expect(s.buttons).toBe(XB.A | XB.LB | XB.UP);
    expect([s.left_trigger, s.right_trigger]).toEqual([0, 255]);
    expect([s.left_x, s.left_y, s.right_x, s.right_y]).toEqual([32767, 0, 0, 32767]);
    expect(screenPadState({ pressed: new Set(), sticks: { ls: [0.02, 0.02] } }).left_x, 'a resting thumb').toBe(0);
  });
});
