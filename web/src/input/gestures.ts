// Touchpad gestures on a phone (P4-8), without the DOM so tests can drive them with plain numbers.
//
// One finger moves the pointer, a tap clicks, tap and then hold-and-move drags, a long press or a two-finger tap
// is a right click, two fingers scroll. Times are in milliseconds, positions in CSS pixels.

import type { MouseButton } from './protocol';

export type GestureAction =
  | { type: 'move'; dx: number; dy: number }
  | { type: 'button'; button: MouseButton; down: boolean }
  | { type: 'wheel'; dx: number; dy: number };

export const TAP_MS = 250;
export const DOUBLE_TAP_MS = 300;
export const LONG_PRESS_MS = 600;
/** Movement under this many CSS pixels still counts as a tap. */
export const TAP_SLOP = 10;
/** Wheel units per CSS pixel of a two-finger scroll: 40 px is one step. */
export const SCROLL_UNITS_PER_PX = 3;

interface Finger {
  x: number;
  y: number;
  startX: number;
  startY: number;
}

export class TouchpadGestures {
  private fingers = new Map<number, Finger>();
  private downAt = 0;
  private moved = false;
  private twoFinger = false;
  private dragging = false;
  private longPressed = false;
  private lastTap: { at: number; x: number; y: number } | null = null;

  constructor(
    private readonly emit: (a: GestureAction) => void,
    /** Remote pixels per CSS pixel of finger movement. */
    private readonly scale: () => number,
  ) {}

  down(id: number, x: number, y: number, now: number): void {
    this.fingers.set(id, { x, y, startX: x, startY: y });
    if (this.fingers.size === 1) {
      this.downAt = now;
      this.moved = false;
      this.longPressed = false;
      this.twoFinger = false;
      const t = this.lastTap;
      if (t && now - t.at < DOUBLE_TAP_MS && Math.hypot(x - t.x, y - t.y) < 3 * TAP_SLOP) {
        // Tap, then touch again: hold the left button and drag.
        this.dragging = true;
        this.lastTap = null;
        this.emit({ type: 'button', button: 'left', down: true });
      }
    } else {
      this.twoFinger = true;
    }
  }

  move(id: number, x: number, y: number): void {
    const f = this.fingers.get(id);
    if (!f) return;
    const dx = x - f.x;
    const dy = y - f.y;
    f.x = x;
    f.y = y;
    if (Math.hypot(x - f.startX, y - f.startY) >= TAP_SLOP) this.moved = true;
    if (this.twoFinger) {
      // Two fingers move together; each reports half of the scroll. Fingers down scroll up, as on the phone.
      const n = this.fingers.size;
      this.emit({ type: 'wheel', dx: (-dx * SCROLL_UNITS_PER_PX) / n, dy: (dy * SCROLL_UNITS_PER_PX) / n });
    } else if (dx !== 0 || dy !== 0) {
      const s = this.scale();
      this.emit({ type: 'move', dx: dx * s, dy: dy * s });
    }
  }

  up(id: number, now: number): void {
    const f = this.fingers.get(id);
    if (!f) return;
    this.fingers.delete(id);
    if (this.fingers.size > 0) return;
    const quick = now - this.downAt < TAP_MS;
    if (this.twoFinger) {
      if (!this.moved && now - this.downAt < LONG_PRESS_MS) this.click('right');
    } else if (this.dragging) {
      this.dragging = false;
      this.emit({ type: 'button', button: 'left', down: false });
    } else if (!this.moved && !this.longPressed && quick) {
      this.click('left');
      this.lastTap = { at: now, x: f.x, y: f.y };
    }
    this.twoFinger = false;
  }

  cancel(): void {
    this.fingers.clear();
    if (this.dragging) this.emit({ type: 'button', button: 'left', down: false });
    this.dragging = false;
    this.twoFinger = false;
  }

  /** Call regularly: a finger held still long enough is a right click. */
  tick(now: number): void {
    if (
      this.fingers.size === 1 &&
      !this.twoFinger &&
      !this.dragging &&
      !this.moved &&
      !this.longPressed &&
      now - this.downAt >= LONG_PRESS_MS
    ) {
      this.longPressed = true;
      this.click('right');
    }
  }

  private click(button: MouseButton): void {
    this.emit({ type: 'button', button, down: true });
    this.emit({ type: 'button', button, down: false });
  }
}

/** Small contact ids for the protocol: the lowest free one for each browser pointer. */
export class ContactIds {
  private ids = new Map<number, number>();

  take(pointerId: number): number | undefined {
    const known = this.ids.get(pointerId);
    if (known !== undefined) return known;
    const used = new Set(this.ids.values());
    for (let id = 0; id < 10; id += 1) {
      if (!used.has(id)) {
        this.ids.set(pointerId, id);
        return id;
      }
    }
    return undefined;
  }

  get(pointerId: number): number | undefined {
    return this.ids.get(pointerId);
  }

  free(pointerId: number): void {
    this.ids.delete(pointerId);
  }

  clear(): void {
    this.ids.clear();
  }
}
