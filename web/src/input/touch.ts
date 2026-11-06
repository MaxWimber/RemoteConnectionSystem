// Touch on Android (P4-8): touchpad mode with gestures, direct mode with real touches, and the screen keyboard.

import type { InputChannels } from './channels';
import { ContactIds, TouchpadGestures, type GestureAction } from './gestures';
import { contentBox, normalized, takeWhole } from './pointer';
import type { TouchPhase, TouchPoint } from './protocol';
import { scancodeFor } from './scancodes';

export type TouchMode = 'touchpad' | 'direct';

const MODE_KEY = 'rcs.touchMode';
const TICK_MS = 50;

export function loadMode(): TouchMode {
  try {
    return localStorage.getItem(MODE_KEY) === 'direct' ? 'direct' : 'touchpad';
  } catch {
    return 'touchpad';
  }
}

function saveMode(mode: TouchMode): void {
  try {
    localStorage.setItem(MODE_KEY, mode);
  } catch {
    // Not saved; the mode still applies until the page closes.
  }
}

/** What the screen keyboard did to the hidden field: characters to delete at the end, then text to add. */
export function textChange(before: string, after: string): { deletes: number; insert: string } {
  let common = 0;
  const max = Math.min(before.length, after.length);
  while (common < max && before[common] === after[common]) common += 1;
  // Do not split a surrogate pair.
  if (common > 0 && /[\uD800-\uDBFF]/.test(before[common - 1] ?? '')) common -= 1;
  return { deletes: [...before.slice(common)].length, insert: after.slice(common) };
}

export class TouchController {
  private mode: TouchMode = loadMode();
  private gestures: TouchpadGestures;
  private ids = new ContactIds();
  private pending = new Map<number, TouchPoint>();
  private relative = { dx: 0, dy: 0 };
  private wheel = { dx: 0, dy: 0 };
  private frame = 0;
  private timer = 0;
  private offs: (() => void)[] = [];
  private text = '';

  constructor(
    private readonly video: HTMLVideoElement,
    private readonly field: HTMLTextAreaElement,
    private readonly channels: () => InputChannels | null,
    private readonly log: (message: string) => void,
  ) {
    this.gestures = new TouchpadGestures(
      (a) => this.onGesture(a),
      () => {
        const box = contentBox(this.video.getBoundingClientRect(), this.video.videoWidth, this.video.videoHeight);
        return this.video.videoWidth > 0 && box.width > 0 ? this.video.videoWidth / box.width : 1;
      },
    );
  }

  get current(): TouchMode {
    return this.mode;
  }

  /** True while the server pointer must be drawn over the video: the phone has no pointer of its own. */
  get drawsPointer(): boolean {
    return this.mode === 'touchpad';
  }

  toggle(): TouchMode {
    this.reset();
    this.mode = this.mode === 'touchpad' ? 'direct' : 'touchpad';
    saveMode(this.mode);
    this.log(`касание: режим ${this.mode === 'touchpad' ? 'тачпада' : 'прямого касания'}`);
    return this.mode;
  }

  /** Opens the phone's keyboard: the hidden field takes the focus inside the button's tap. */
  showKeyboard(): void {
    this.field.value = '';
    this.text = '';
    this.field.focus();
  }

  attach(): void {
    const v = this.video;
    const listen = <K extends keyof HTMLElementEventMap>(
      target: HTMLElement,
      type: K,
      f: (e: HTMLElementEventMap[K]) => void,
    ) => {
      target.addEventListener(type, f);
      this.offs.push(() => target.removeEventListener(type, f));
    };
    listen(v, 'pointerdown', (e) => this.onPointer(e, 'down'));
    listen(v, 'pointermove', (e) => this.onPointer(e, 'move'));
    listen(v, 'pointerup', (e) => this.onPointer(e, 'up'));
    listen(v, 'pointercancel', (e) => this.onPointer(e, 'cancel'));
    listen(this.field, 'input', () => this.onText());
    listen(this.field, 'keydown', (e) => this.onFieldKey(e));
    this.timer = window.setInterval(() => this.gestures.tick(performance.now()), TICK_MS);
  }

  detach(): void {
    this.offs.forEach((off) => off());
    this.offs = [];
    window.clearInterval(this.timer);
    window.cancelAnimationFrame(this.frame);
    this.frame = 0;
    this.reset();
  }

  private reset(): void {
    this.gestures.cancel();
    this.pending.clear();
    this.ids.clear();
  }

  private point(e: PointerEvent): { x: number; y: number } {
    const box = contentBox(this.video.getBoundingClientRect(), this.video.videoWidth, this.video.videoHeight);
    return normalized(box, e.clientX, e.clientY);
  }

  private onPointer(e: PointerEvent, phase: TouchPhase): void {
    if (e.pointerType !== 'touch') return;
    e.preventDefault();
    if (this.mode === 'touchpad') {
      const now = performance.now();
      if (phase === 'down') this.gestures.down(e.pointerId, e.clientX, e.clientY, now);
      else if (phase === 'move') this.gestures.move(e.pointerId, e.clientX, e.clientY);
      else if (phase === 'up') this.gestures.up(e.pointerId, now);
      else this.gestures.cancel();
      return;
    }
    const id = phase === 'down' ? this.ids.take(e.pointerId) : this.ids.get(e.pointerId);
    if (id === undefined) return;
    if (phase === 'up' || phase === 'cancel') this.ids.free(e.pointerId);
    const { x, y } = this.point(e);
    const earlier = this.pending.get(id);
    // A finger that went down in this frame stays a down until it is sent.
    const kept: TouchPhase = earlier?.phase === 'down' && phase === 'move' ? 'down' : phase;
    this.pending.set(id, { id, phase: kept, x, y });
    if (phase === 'move') this.schedule();
    else this.flush();
  }

  private onGesture(a: GestureAction): void {
    const c = this.channels();
    if (!c?.open) return;
    if (a.type === 'button') {
      this.flush();
      c.send({ type: 'mouse_button', button: a.button, down: a.down });
    } else if (a.type === 'move') {
      this.relative.dx += a.dx;
      this.relative.dy += a.dy;
      this.schedule();
    } else {
      this.wheel.dx += a.dx;
      this.wheel.dy += a.dy;
      this.schedule();
    }
  }

  private schedule(): void {
    if (this.frame === 0) this.frame = window.requestAnimationFrame(() => this.flush());
  }

  private flush(): void {
    window.cancelAnimationFrame(this.frame);
    this.frame = 0;
    const c = this.channels();
    if (!c?.open) {
      this.pending.clear();
      return;
    }
    if (this.pending.size > 0) {
      c.send({ type: 'touch', points: [...this.pending.values()] });
      this.pending.clear();
    }
    const dx = Math.trunc(this.relative.dx);
    const dy = Math.trunc(this.relative.dy);
    if (dx !== 0 || dy !== 0) {
      c.moveRelative(dx, dy);
      this.relative.dx -= dx;
      this.relative.dy -= dy;
    }
    const [wx, restX] = takeWhole(this.wheel.dx);
    const [wy, restY] = takeWhole(this.wheel.dy);
    this.wheel = { dx: restX, dy: restY };
    if (wx !== 0 || wy !== 0) c.send({ type: 'wheel', dx: wx, dy: wy });
  }

  private sendKey(code: string): void {
    const c = this.channels();
    const scancode = scancodeFor(code);
    if (!c?.open || scancode === undefined) return;
    c.send({ type: 'key', down: true, scancode });
    c.send({ type: 'key', down: false, scancode });
  }

  private onText(): void {
    const after = this.field.value;
    const { deletes, insert } = textChange(this.text, after);
    this.text = after;
    for (let i = 0; i < deletes; i += 1) this.sendKey('Backspace');
    if (insert) this.channels()?.send({ type: 'text', text: insert });
    // Keep the field short; the keyboard's word suggestions only need the current word.
    if (after.length > 200) {
      this.field.value = '';
      this.text = '';
    }
  }

  /** Keys the field cannot show as a change: Backspace on an empty field, arrows, Enter on some keyboards. */
  private onFieldKey(e: KeyboardEvent): void {
    if (e.key === 'Backspace' && this.field.value === '') {
      e.preventDefault();
      this.sendKey('Backspace');
    } else if (e.key.startsWith('Arrow') || e.key === 'Tab' || e.key === 'Escape') {
      // For these keys KeyboardEvent.key is also the code name.
      e.preventDefault();
      this.sendKey(e.key);
    }
  }
}
