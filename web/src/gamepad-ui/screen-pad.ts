// The on-screen gamepad (P5-6) and its layout editor (P5-7). Plain DOM outside Preact: every control follows its
// own pointers, so several fingers work at once, and the state is read by the gamepad poller each frame.

import type { PadState } from '../input/gamepads';
import { parseLayout, resize, screenPadState, serializeLayout, stickVector, type Control, type Layout } from './layout';

const LAYOUT_KEY = 'rcs.padLayout';

function loadLayout(): Layout {
  try {
    return parseLayout(localStorage.getItem(LAYOUT_KEY));
  } catch {
    return parseLayout(null);
  }
}

function saveLayout(layout: Layout): void {
  try {
    localStorage.setItem(LAYOUT_KEY, serializeLayout(layout));
  } catch {
    // Not saved; the layout still applies until the page closes.
  }
}

function el<K extends keyof HTMLElementTagNameMap>(tag: K, cls: string, text = ''): HTMLElementTagNameMap[K] {
  const e = document.createElement(tag);
  e.className = cls;
  if (text) e.textContent = text;
  return e;
}

export class ScreenPad {
  private readonly root = el('div', 'screen-pad');
  private readonly bar = el('div', 'screen-pad-bar');
  private layout: Layout = loadLayout();
  private controls = new Map<string, HTMLDivElement>();
  private pressed = new Set<string>();
  private sticks: Record<string, [number, number]> = {};
  /** Which pointer moves each stick; a stick follows only its first finger. */
  private stickPointer = new Map<string, number>();
  private editing = false;
  private selected: string | null = null;
  private shown = false;

  constructor(
    parent: HTMLElement,
    /** Called when the pad changes or shows, so the poller sends at once. */
    private readonly changed: () => void,
  ) {
    this.root.hidden = true;
    parent.appendChild(this.root);
    this.build();
  }

  get visible(): boolean {
    return this.shown;
  }

  /** The pad's state while it is shown; null when hidden, so its heartbeats stop and the server zeroes it. */
  read(): PadState | null {
    if (!this.shown || this.editing) return null;
    return screenPadState({ pressed: this.pressed, sticks: this.sticks });
  }

  toggle(): boolean {
    this.shown = !this.shown;
    this.root.hidden = !this.shown;
    if (!this.shown) this.releaseAll();
    this.changed();
    return this.shown;
  }

  destroy(): void {
    this.root.remove();
  }

  private releaseAll(): void {
    this.pressed.clear();
    this.sticks = {};
    this.stickPointer.clear();
    for (const [id, e] of this.controls) {
      e.classList.remove('active');
      if (this.kindOf(id) === 'stick') this.moveKnob(e, 0, 0);
    }
  }

  private kindOf(id: string): Control['kind'] | undefined {
    return this.layout.find((c) => c.id === id)?.kind;
  }

  private build(): void {
    this.root.replaceChildren();
    this.controls.clear();
    for (const c of this.layout) {
      const e = el('div', `pad-control pad-${c.kind}`, c.kind === 'button' ? c.label : '');
      if (c.kind === 'stick') e.appendChild(el('div', 'pad-knob'));
      this.place(e, c);
      e.addEventListener('pointerdown', (ev) => this.down(c.id, e, ev));
      e.addEventListener('pointermove', (ev) => this.move(c.id, e, ev));
      e.addEventListener('pointerup', (ev) => this.up(c.id, e, ev));
      e.addEventListener('pointercancel', (ev) => this.up(c.id, e, ev));
      e.addEventListener('contextmenu', (ev) => ev.preventDefault());
      this.controls.set(c.id, e);
      this.root.appendChild(e);
    }
    this.buildBar();
    this.root.appendChild(this.bar);
  }

  private buildBar(): void {
    this.bar.replaceChildren();
    const button = (text: string, title: string, f: () => void) => {
      const b = el('button', '', text);
      b.title = title;
      b.addEventListener('click', f);
      this.bar.appendChild(b);
    };
    if (!this.editing) {
      button('✎', 'Изменить раскладку', () => this.setEditing(true));
      return;
    }
    button('−', 'Меньше', () => this.resizeSelected(-2));
    button('+', 'Больше', () => this.resizeSelected(2));
    button('Сброс', 'Раскладка по умолчанию', () => {
      this.layout = parseLayout(null);
      saveLayout(this.layout);
      this.build();
    });
    button('Готово', 'Закончить правку', () => this.setEditing(false));
  }

  private setEditing(on: boolean): void {
    this.editing = on;
    this.selected = null;
    this.releaseAll();
    this.root.classList.toggle('editing', on);
    this.buildBar();
    this.changed();
  }

  private resizeSelected(delta: number): void {
    const i = this.layout.findIndex((c) => c.id === this.selected);
    if (i < 0) return;
    this.layout[i] = resize(this.layout[i]!, delta);
    saveLayout(this.layout);
    const e = this.controls.get(this.layout[i]!.id);
    if (e) this.place(e, this.layout[i]!);
  }

  private place(e: HTMLElement, c: Control): void {
    e.style.width = `${c.size}vmin`;
    e.style.height = `${c.size}vmin`;
    e.style.left = `calc(${c.x}% - ${c.size / 2}vmin)`;
    e.style.top = `calc(${c.y}% - ${c.size / 2}vmin)`;
  }

  private moveKnob(e: HTMLElement, x: number, y: number): void {
    const knob = e.firstElementChild as HTMLElement | null;
    if (knob) knob.style.transform = `translate(${x * 50}%, ${y * 50}%)`;
  }

  private stickFrom(e: HTMLElement, ev: PointerEvent): [number, number] {
    const r = e.getBoundingClientRect();
    return stickVector(ev.clientX - (r.left + r.width / 2), ev.clientY - (r.top + r.height / 2), r.width / 2);
  }

  private down(id: string, e: HTMLDivElement, ev: PointerEvent): void {
    ev.preventDefault();
    ev.stopPropagation();
    try {
      e.setPointerCapture(ev.pointerId);
    } catch {
      // The pointer is gone already.
    }
    if (this.editing) {
      this.selected = id;
      for (const [cid, ce] of this.controls) ce.classList.toggle('selected', cid === id);
      return;
    }
    if (this.kindOf(id) === 'stick') {
      if (this.stickPointer.has(id)) return;
      this.stickPointer.set(id, ev.pointerId);
      this.sticks[id] = this.stickFrom(e, ev);
      this.moveKnob(e, ...this.sticks[id]!);
    } else {
      this.pressed.add(id);
      navigator.vibrate?.(12);
    }
    e.classList.add('active');
    this.changed();
  }

  private move(id: string, e: HTMLDivElement, ev: PointerEvent): void {
    if (this.editing) {
      if (this.selected !== id || !e.hasPointerCapture(ev.pointerId)) return;
      const i = this.layout.findIndex((c) => c.id === id);
      if (i < 0) return;
      const x = Math.min(100, Math.max(0, (ev.clientX / window.innerWidth) * 100));
      const y = Math.min(100, Math.max(0, (ev.clientY / window.innerHeight) * 100));
      this.layout[i] = { ...this.layout[i]!, x, y };
      this.place(e, this.layout[i]!);
      return;
    }
    if (this.stickPointer.get(id) !== ev.pointerId) return;
    this.sticks[id] = this.stickFrom(e, ev);
    this.moveKnob(e, ...this.sticks[id]!);
    this.changed();
  }

  private up(id: string, e: HTMLDivElement, ev: PointerEvent): void {
    if (this.editing) {
      saveLayout(this.layout);
      return;
    }
    if (this.kindOf(id) === 'stick') {
      if (this.stickPointer.get(id) !== ev.pointerId) return;
      this.stickPointer.delete(id);
      delete this.sticks[id];
      this.moveKnob(e, 0, 0);
    } else {
      this.pressed.delete(id);
    }
    e.classList.remove('active');
    this.changed();
  }
}
