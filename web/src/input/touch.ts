// Touch on Android (P4-8): the fingers go to Windows as real touches (InjectTouchInput), so Windows' own touch
// gestures apply, and the phone's keyboard types through a hidden field. The touchpad mode was removed at the
// user's request (D-022).

import type { InputChannels } from './channels';
import { contentBox, normalized } from './pointer';
import type { TouchPhase, TouchPoint } from './protocol';
import { scancodeFor } from './scancodes';

/** What the screen keyboard did to the hidden field: characters to delete at the end, then text to add. */
export function textChange(before: string, after: string): { deletes: number; insert: string } {
  let common = 0;
  const max = Math.min(before.length, after.length);
  while (common < max && before[common] === after[common]) common += 1;
  // Do not split a surrogate pair.
  if (common > 0 && /[\uD800-\uDBFF]/.test(before[common - 1] ?? '')) common -= 1;
  return { deletes: [...before.slice(common)].length, insert: after.slice(common) };
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

export class TouchController {
  private ids = new ContactIds();
  private pending = new Map<number, TouchPoint>();
  private frame = 0;
  private offs: (() => void)[] = [];
  private text = '';

  constructor(
    private readonly video: HTMLVideoElement,
    private readonly field: HTMLTextAreaElement,
    private readonly channels: () => InputChannels | null,
  ) {}

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
  }

  detach(): void {
    this.offs.forEach((off) => off());
    this.offs = [];
    window.cancelAnimationFrame(this.frame);
    this.frame = 0;
    this.pending.clear();
    this.ids.clear();
  }

  private onPointer(e: PointerEvent, phase: TouchPhase): void {
    if (e.pointerType !== 'touch') return;
    e.preventDefault();
    const id = phase === 'down' ? this.ids.take(e.pointerId) : this.ids.get(e.pointerId);
    if (id === undefined) return;
    if (phase === 'up' || phase === 'cancel') this.ids.free(e.pointerId);
    const box = contentBox(this.video.getBoundingClientRect(), this.video.videoWidth, this.video.videoHeight);
    const { x, y } = normalized(box, e.clientX, e.clientY);
    const earlier = this.pending.get(id);
    // A finger that went down in this frame stays a down until it is sent.
    const kept: TouchPhase = earlier?.phase === 'down' && phase === 'move' ? 'down' : phase;
    this.pending.set(id, { id, phase: kept, x, y });
    // Moves are merged up to one message per frame; downs and ups go at once.
    if (phase === 'move') {
      if (this.frame === 0) this.frame = window.requestAnimationFrame(() => this.flush());
    } else {
      this.flush();
    }
  }

  private flush(): void {
    window.cancelAnimationFrame(this.frame);
    this.frame = 0;
    const c = this.channels();
    if (c?.open && this.pending.size > 0) c.send({ type: 'touch', points: [...this.pending.values()] });
    this.pending.clear();
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

  /** Keys the field cannot show as a change: Backspace on an empty field, arrows, Tab, Escape. */
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
