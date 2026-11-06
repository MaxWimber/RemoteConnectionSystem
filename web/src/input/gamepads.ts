// Physical gamepads through the Gamepad API (P5-3, P5-4): polled every animation frame, mapped to XInput, sent on
// change and at least every 50 ms so the server knows the pad is alive (it zeroes a pad after 500 ms of silence).

import type { InputChannels } from './channels';

export const HEARTBEAT_MS = 50;
export const MAX_PADS = 4;
/** Stick deflection under this share of the full range counts as centred. */
export const DEADZONE = 0.12;

/** XInput button bits, docs/PROTOCOL.md. */
export const XB = {
  UP: 0x0001,
  DOWN: 0x0002,
  LEFT: 0x0004,
  RIGHT: 0x0008,
  START: 0x0010,
  BACK: 0x0020,
  LTHUMB: 0x0040,
  RTHUMB: 0x0080,
  LB: 0x0100,
  RB: 0x0200,
  GUIDE: 0x0400,
  A: 0x1000,
  B: 0x2000,
  X: 0x4000,
  Y: 0x8000,
} as const;

/** Buttons of the W3C standard mapping in order, as XInput bits; 6 and 7 are the triggers. */
const STANDARD: readonly number[] = [
  XB.A, XB.B, XB.X, XB.Y, XB.LB, XB.RB, 0, 0, XB.BACK, XB.START, XB.LTHUMB, XB.RTHUMB,
  XB.UP, XB.DOWN, XB.LEFT, XB.RIGHT, XB.GUIDE,
];

export interface PadState {
  buttons: number;
  left_trigger: number;
  right_trigger: number;
  left_x: number;
  left_y: number;
  right_x: number;
  right_y: number;
}

export const NEUTRAL: PadState = { buttons: 0, left_trigger: 0, right_trigger: 0, left_x: 0, left_y: 0, right_x: 0, right_y: 0 };

/** A stick with a round dead zone, rescaled so it still reaches full deflection; y up is positive, as in XInput. */
export function stick(x: number, y: number, deadzone = DEADZONE): [number, number] {
  const m = Math.hypot(x, y);
  if (m < deadzone || m === 0) return [0, 0];
  const scaled = Math.min(1, (m - deadzone) / (1 - deadzone)) / m;
  // `| 0` also turns -0 into 0.
  const axis = (v: number) => Math.max(-32768, Math.min(32767, Math.round(v * scaled * 32767))) | 0;
  return [axis(x), axis(-y)];
}

interface PadLike {
  buttons: readonly { pressed: boolean; value: number }[];
  axes: readonly number[];
}

/** The XInput state of a gamepad with the standard mapping. */
export function padState(gp: PadLike): PadState {
  let buttons = 0;
  STANDARD.forEach((bit, i) => {
    if (bit !== 0 && gp.buttons[i]?.pressed) buttons |= bit;
  });
  const trigger = (i: number) => Math.round(Math.max(0, Math.min(1, gp.buttons[i]?.value ?? 0)) * 255);
  const [lx, ly] = stick(gp.axes[0] ?? 0, gp.axes[1] ?? 0);
  const [rx, ry] = stick(gp.axes[2] ?? 0, gp.axes[3] ?? 0);
  return {
    buttons,
    left_trigger: trigger(6),
    right_trigger: trigger(7),
    left_x: lx,
    left_y: ly,
    right_x: rx,
    right_y: ry,
  };
}

export function samePad(a: PadState, b: PadState): boolean {
  return (
    a.buttons === b.buttons &&
    a.left_trigger === b.left_trigger &&
    a.right_trigger === b.right_trigger &&
    a.left_x === b.left_x &&
    a.left_y === b.left_y &&
    a.right_x === b.right_x &&
    a.right_y === b.right_y
  );
}

/** Decides when a pad's state goes out: on change, or when the heartbeat is due. */
export class PadSender {
  private last: PadState | null = null;
  private sentAt = -Infinity;

  due(state: PadState, now: number): boolean {
    if (this.last && samePad(this.last, state) && now - this.sentAt < HEARTBEAT_MS) return false;
    this.last = state;
    this.sentAt = now;
    return true;
  }

  reset(): void {
    this.last = null;
    this.sentAt = -Infinity;
  }
}

/** Polls the browser's gamepads while any is connected. The first four connected pads are pads 0..3. */
export class GamepadPoller {
  private senders = Array.from({ length: MAX_PADS }, () => new PadSender());
  private frame = 0;
  private offs: (() => void)[] = [];
  /** Extra pads, for example the on-screen gamepad: numbered after the physical ones, polled with them. */
  private extra: (() => PadState | null)[] = [];

  constructor(
    private readonly channels: () => InputChannels | null,
    private readonly log: (message: string) => void,
    /** Called when a browser gamepad connects, for example to check that the server can take it. */
    private readonly onConnect: () => void = () => {},
  ) {}

  /** How many browser gamepads are connected. */
  get connected(): number {
    return this.pads().length;
  }

  attach(): void {
    const connected = (e: GamepadEvent) => {
      this.log(`геймпад подключён: ${e.gamepad.id}`);
      this.onConnect();
      this.start();
    };
    const disconnected = (e: GamepadEvent) => this.log(`геймпад отключён: ${e.gamepad.id}`);
    window.addEventListener('gamepadconnected', connected);
    window.addEventListener('gamepaddisconnected', disconnected);
    this.offs.push(() => {
      window.removeEventListener('gamepadconnected', connected);
      window.removeEventListener('gamepaddisconnected', disconnected);
    });
    if (this.connected > 0) this.start();
  }

  detach(): void {
    this.offs.forEach((off) => off());
    this.offs = [];
    window.cancelAnimationFrame(this.frame);
    this.frame = 0;
  }

  /** Adds a pad from another source; it is sent like a physical one while it returns a state. */
  addSource(read: () => PadState | null): void {
    this.extra.push(read);
    this.start();
  }

  /** Polls again from the next frame, for example when the on-screen pad appears or changes. */
  wake(): void {
    this.start();
  }

  private pads(): Gamepad[] {
    const list = typeof navigator.getGamepads === 'function' ? navigator.getGamepads() : [];
    return list.filter((g): g is Gamepad => g !== null && g.connected);
  }

  private start(): void {
    if (this.frame === 0) this.frame = window.requestAnimationFrame(() => this.poll());
  }

  private poll(): void {
    this.frame = 0;
    const now = performance.now();
    const c = this.channels();
    const states = new Map<number, PadState>();
    const physical = this.pads().slice(0, MAX_PADS);
    physical.forEach((gp, i) => states.set(i, padState(gp)));
    let next = physical.length;
    for (const read of this.extra) {
      const s = read();
      if (s && next < MAX_PADS) states.set(next++, s);
    }
    for (const [pad, state] of states) {
      const sender = this.senders[pad];
      if (c?.open && sender?.due(state, now)) c.sendGamepad(pad, state);
    }
    // Pads that disappeared get no more heartbeats; the server sets them to neutral after 500 ms.
    this.senders.forEach((s, pad) => {
      if (!states.has(pad)) s.reset();
    });
    if (states.size > 0) this.start();
  }
}
