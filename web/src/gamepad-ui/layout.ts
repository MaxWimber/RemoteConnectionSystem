// The on-screen gamepad's layout and state (P5-6, P5-7), without the DOM so tests can check it.

import { NEUTRAL, XB, stick, type PadState } from '../input/gamepads';

export type ControlKind = 'stick' | 'button';

export interface Control {
  id: string;
  kind: ControlKind;
  label: string;
  /** Centre in percent of the screen width and height. */
  x: number;
  y: number;
  /** Diameter in percent of the shorter screen side. */
  size: number;
}

export type Layout = Control[];

/** What each button control does: an XInput bit, or a trigger. */
export const BUTTON_ACTIONS: Record<string, { bit?: number; trigger?: 'left' | 'right' }> = {
  a: { bit: XB.A },
  b: { bit: XB.B },
  x: { bit: XB.X },
  y: { bit: XB.Y },
  lb: { bit: XB.LB },
  rb: { bit: XB.RB },
  lt: { trigger: 'left' },
  rt: { trigger: 'right' },
  back: { bit: XB.BACK },
  start: { bit: XB.START },
  guide: { bit: XB.GUIDE },
  l3: { bit: XB.LTHUMB },
  r3: { bit: XB.RTHUMB },
  up: { bit: XB.UP },
  down: { bit: XB.DOWN },
  left: { bit: XB.LEFT },
  right: { bit: XB.RIGHT },
};

/** A landscape phone: sticks low, face buttons on the right, d-pad on the left, shoulders on top. */
export const DEFAULT_LAYOUT: Layout = [
  { id: 'ls', kind: 'stick', label: '', x: 16, y: 70, size: 30 },
  { id: 'rs', kind: 'stick', label: '', x: 66, y: 74, size: 26 },
  { id: 'a', kind: 'button', label: 'A', x: 88, y: 62, size: 11 },
  { id: 'b', kind: 'button', label: 'B', x: 95, y: 46, size: 11 },
  { id: 'x', kind: 'button', label: 'X', x: 81, y: 46, size: 11 },
  { id: 'y', kind: 'button', label: 'Y', x: 88, y: 30, size: 11 },
  { id: 'up', kind: 'button', label: '▲', x: 34, y: 34, size: 9 },
  { id: 'down', kind: 'button', label: '▼', x: 34, y: 58, size: 9 },
  { id: 'left', kind: 'button', label: '◀', x: 28, y: 46, size: 9 },
  { id: 'right', kind: 'button', label: '▶', x: 40, y: 46, size: 9 },
  { id: 'lb', kind: 'button', label: 'LB', x: 7, y: 12, size: 10 },
  { id: 'lt', kind: 'button', label: 'LT', x: 7, y: 32, size: 10 },
  { id: 'rb', kind: 'button', label: 'RB', x: 93, y: 12, size: 10 },
  { id: 'rt', kind: 'button', label: 'RT', x: 76, y: 12, size: 10 },
  { id: 'back', kind: 'button', label: '⧉', x: 42, y: 10, size: 8 },
  { id: 'guide', kind: 'button', label: '⊗', x: 50, y: 10, size: 8 },
  { id: 'start', kind: 'button', label: '≡', x: 58, y: 10, size: 8 },
  { id: 'l3', kind: 'button', label: 'L3', x: 30, y: 88, size: 8 },
  { id: 'r3', kind: 'button', label: 'R3', x: 50, y: 88, size: 8 },
];

const MIN_SIZE = 5;
const MAX_SIZE = 45;

function clamp(v: number, lo: number, hi: number): number {
  return Math.min(hi, Math.max(lo, v));
}

/** A saved layout, checked: unknown or broken entries fall back to the default, positions stay on screen. */
export function parseLayout(text: string | null): Layout {
  if (!text) return DEFAULT_LAYOUT.map((c) => ({ ...c }));
  let saved: unknown;
  try {
    saved = JSON.parse(text);
  } catch {
    return DEFAULT_LAYOUT.map((c) => ({ ...c }));
  }
  const list = Array.isArray(saved) ? (saved as Partial<Control>[]) : [];
  return DEFAULT_LAYOUT.map((d) => {
    const s = list.find((c) => c && c.id === d.id);
    const num = (v: unknown, fallback: number) => (typeof v === 'number' && Number.isFinite(v) ? v : fallback);
    return {
      ...d,
      x: clamp(num(s?.x, d.x), 0, 100),
      y: clamp(num(s?.y, d.y), 0, 100),
      size: clamp(num(s?.size, d.size), MIN_SIZE, MAX_SIZE),
    };
  });
}

export function serializeLayout(layout: Layout): string {
  return JSON.stringify(layout.map(({ id, x, y, size }) => ({ id, x, y, size })));
}

export function resize(control: Control, delta: number): Control {
  return { ...control, size: clamp(control.size + delta, MIN_SIZE, MAX_SIZE) };
}

/** A stick from a finger offset in pixels and the stick's radius: -1..1, y down, clamped to the circle. */
export function stickVector(dx: number, dy: number, radius: number): [number, number] {
  if (radius <= 0) return [0, 0];
  const m = Math.hypot(dx, dy) / radius;
  const k = m > 1 ? 1 / m : 1;
  return [(dx / radius) * k, (dy / radius) * k];
}

/** What is held on the screen: button ids, and stick vectors in -1..1 with y down. */
export interface ScreenInput {
  pressed: ReadonlySet<string>;
  sticks: Readonly<Record<string, [number, number]>>;
}

/** The XInput state of the on-screen gamepad. Sticks get a small dead zone: a resting thumb is centred. */
export function screenPadState(input: ScreenInput): PadState {
  const s = { ...NEUTRAL };
  for (const id of input.pressed) {
    const a = BUTTON_ACTIONS[id];
    if (!a) continue;
    if (a.bit) s.buttons |= a.bit;
    if (a.trigger === 'left') s.left_trigger = 255;
    if (a.trigger === 'right') s.right_trigger = 255;
  }
  const [lx, ly] = input.sticks['ls'] ?? [0, 0];
  const [rx, ry] = input.sticks['rs'] ?? [0, 0];
  [s.left_x, s.left_y] = stick(lx, ly, 0.05);
  [s.right_x, s.right_y] = stick(rx, ry, 0.05);
  return s;
}
