import { describe, expect, it } from 'vitest';
import { HEARTBEAT_MS, NEUTRAL, PadSender, XB, padState, stick } from './gamepads';

function pad(pressed: number[], values: Record<number, number> = {}, axes = [0, 0, 0, 0]) {
  return {
    buttons: Array.from({ length: 17 }, (_, i) => ({ pressed: pressed.includes(i), value: values[i] ?? (pressed.includes(i) ? 1 : 0) })),
    axes,
  };
}

describe('gamepad mapping', () => {
  it('maps the standard layout to XInput bits', () => {
    expect(padState(pad([0, 1, 2, 3])).buttons).toBe(XB.A | XB.B | XB.X | XB.Y);
    expect(padState(pad([4, 5, 8, 9, 10, 11])).buttons).toBe(XB.LB | XB.RB | XB.BACK | XB.START | XB.LTHUMB | XB.RTHUMB);
    expect(padState(pad([12, 13, 14, 15, 16])).buttons).toBe(XB.UP | XB.DOWN | XB.LEFT | XB.RIGHT | XB.GUIDE);
    expect(padState(pad([])).buttons).toBe(0);
  });

  it('reads triggers as 0..255 and not as buttons', () => {
    const s = padState(pad([6, 7], { 6: 0.5, 7: 1 }));
    expect([s.left_trigger, s.right_trigger, s.buttons]).toEqual([128, 255, 0]);
  });

  it('turns the y axis up and keeps a round dead zone', () => {
    expect(stick(0, 0)).toEqual([0, 0]);
    expect(stick(0.1, 0.05), 'inside the dead zone').toEqual([0, 0]);
    expect(stick(0, -1), 'up').toEqual([0, 32767]);
    expect(stick(1, 0)).toEqual([32767, 0]);
    expect(stick(-1, 0)).toEqual([-32767, 0]);
    const [x, y] = stick(0.5, 0.5);
    expect(x).toBe(-y);
    expect(Math.hypot(x, y) / 32767).toBeCloseTo((Math.hypot(0.5, 0.5) - 0.12) / 0.88, 2);
    const s = padState(pad([], {}, [0, -1, 1, 0]));
    expect([s.left_x, s.left_y, s.right_x, s.right_y]).toEqual([0, 32767, 32767, 0]);
  });
});

describe('pad sender', () => {
  it('sends on change and as a heartbeat', () => {
    const s = new PadSender();
    const pressed = { ...NEUTRAL, buttons: XB.A };
    expect(s.due(NEUTRAL, 0)).toBe(true);
    expect(s.due(NEUTRAL, 16)).toBe(false);
    expect(s.due(pressed, 20), 'a change goes at once').toBe(true);
    expect(s.due(pressed, 20 + HEARTBEAT_MS - 1)).toBe(false);
    expect(s.due(pressed, 20 + HEARTBEAT_MS), 'heartbeat').toBe(true);
    s.reset();
    expect(s.due(pressed, 80)).toBe(true);
  });
});
