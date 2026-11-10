// Mouse and keyboard from the browser to the server (P4-5, P4-6), outside Preact state (ARCHITECTURE.md).
//
// Moves are merged up to one per animation frame. Buttons, wheel and keys go at once on input-reliable. With
// Pointer Lock the mouse is relative; otherwise it is absolute over the picture. Keyboard Lock in full screen lets
// Esc, Alt+Tab and Win reach the server. Leaving the page or losing focus sends ReleaseAll.

import type { ClipboardSync } from '../clipboard/sync';
import type { InputChannels } from './channels';
import { buttonOf, contentBox, normalized, takeWhole, wheelUnits } from './pointer';
import { scancodeFor } from './scancodes';

/** Ctrl+V or Shift+Insert, without Alt: the keys that paste in Windows. */
export function isPaste(e: Pick<KeyboardEvent, 'code' | 'ctrlKey' | 'shiftKey' | 'altKey' | 'metaKey'>): boolean {
  if (e.altKey || e.metaKey) return false;
  return (e.code === 'KeyV' && e.ctrlKey) || (e.code === 'Insert' && e.shiftKey && !e.ctrlKey);
}

interface KeyboardLock {
  lock(keys?: string[]): Promise<void>;
  unlock(): void;
}

export class InputController {
  private pendingAbsolute: { x: number; y: number } | null = null;
  /** Relative movement not sent yet, fractions included. */
  private relative = { dx: 0, dy: 0 };
  private wheel = { dx: 0, dy: 0 };
  private frame = 0;
  private keysDown = new Set<number>();
  private buttonsDown = new Set<number>();
  private detachAll: (() => void) | null = null;
  /** The V or Insert of a paste, held back until the text is on the server; `up` once its release came. */
  private pasteKey: { scancode: number; up: boolean } | null = null;

  constructor(
    private readonly video: HTMLVideoElement,
    private readonly channels: () => InputChannels | null,
    private readonly log: (message: string) => void,
    private readonly clipboard: () => ClipboardSync | null = () => null,
  ) {}

  get pointerLocked(): boolean {
    return document.pointerLockElement === this.video;
  }

  /** Relative mouse for games: the pointer is hidden and moves are not limited by the screen edge. */
  capturePointer(): void {
    const request = this.video.requestPointerLock as (options?: { unadjustedMovement?: boolean }) => Promise<void>;
    // Raw movement without the browser's acceleration when the platform has it.
    Promise.resolve(request.call(this.video, { unadjustedMovement: true }))
      .catch(() => Promise.resolve(request.call(this.video)))
      .catch((e: unknown) => this.log(`pointer lock: ${String(e)}`));
  }

  attach(): void {
    if (this.detachAll) return;
    const v = this.video;
    const on = <K extends keyof HTMLElementEventMap>(
      target: HTMLElement,
      type: K,
      f: (e: HTMLElementEventMap[K]) => void,
      options?: AddEventListenerOptions,
    ) => {
      target.addEventListener(type, f, options);
      return () => target.removeEventListener(type, f, options);
    };
    const offs = [
      on(v, 'pointermove', (e) => this.onMove(e)),
      on(v, 'pointerdown', (e) => this.onButton(e, true)),
      on(v, 'pointerup', (e) => this.onButton(e, false)),
      on(v, 'contextmenu', (e) => e.preventDefault()),
      on(v, 'wheel', (e) => this.onWheel(e), { passive: false }),
    ];
    const keydown = (e: KeyboardEvent) => this.onKey(e, true);
    const keyup = (e: KeyboardEvent) => this.onKey(e, false);
    const blur = () => this.releaseAll('окно потеряло фокус');
    const visibility = () => {
      if (document.visibilityState === 'hidden') this.releaseAll('страница скрыта');
    };
    const fullscreen = () => void this.followFullscreen();
    window.addEventListener('keydown', keydown);
    window.addEventListener('keyup', keyup);
    window.addEventListener('blur', blur);
    document.addEventListener('visibilitychange', visibility);
    document.addEventListener('fullscreenchange', fullscreen);
    offs.push(() => {
      window.removeEventListener('keydown', keydown);
      window.removeEventListener('keyup', keyup);
      window.removeEventListener('blur', blur);
      document.removeEventListener('visibilitychange', visibility);
      document.removeEventListener('fullscreenchange', fullscreen);
    });
    this.detachAll = () => offs.forEach((off) => off());
  }

  detach(): void {
    this.releaseAll('ввод отключён');
    this.detachAll?.();
    this.detachAll = null;
    window.cancelAnimationFrame(this.frame);
    this.frame = 0;
  }

  /** Lets go of everything on the server and forgets what was down here. */
  releaseAll(why: string): void {
    const had = this.keysDown.size + this.buttonsDown.size > 0;
    this.keysDown.clear();
    this.buttonsDown.clear();
    const c = this.channels();
    if (!c?.open) return;
    c.send({ type: 'release_all' });
    if (had) this.log(`ввод: всё отпущено (${why})`);
  }

  private schedule(): void {
    if (this.frame === 0) this.frame = window.requestAnimationFrame(() => this.flush());
  }

  /** Sends what was merged since the last frame. Also called before a button so the click lands where it should. */
  private flush(): void {
    this.frame = 0;
    const c = this.channels();
    if (!c?.open) {
      this.pendingAbsolute = null;
      this.relative = { dx: 0, dy: 0 };
      this.wheel = { dx: 0, dy: 0 };
      return;
    }
    if (this.pendingAbsolute) {
      c.moveAbsolute(this.pendingAbsolute.x, this.pendingAbsolute.y);
      this.pendingAbsolute = null;
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

  private isMouse(e: PointerEvent): boolean {
    return e.pointerType === 'mouse' || e.pointerType === 'pen';
  }

  private onMove(e: PointerEvent): void {
    if (!this.isMouse(e)) return;
    if (this.pointerLocked) {
      this.relative.dx += e.movementX;
      this.relative.dy += e.movementY;
    } else {
      const box = contentBox(this.video.getBoundingClientRect(), this.video.videoWidth, this.video.videoHeight);
      this.pendingAbsolute = normalized(box, e.clientX, e.clientY);
    }
    this.schedule();
  }

  private onButton(e: PointerEvent, down: boolean): void {
    if (!this.isMouse(e)) return;
    const button = buttonOf(e.button);
    if (!button) return;
    e.preventDefault();
    if (down) {
      // Capture keeps the button's release on the video; under Pointer Lock it is not needed and throws.
      if (!this.pointerLocked) {
        try {
          this.video.setPointerCapture(e.pointerId);
        } catch {
          // The pointer is already gone; the release still comes through the window.
        }
      }
      this.onMove(e);
    }
    this.flush();
    if (down) this.buttonsDown.add(e.button);
    else if (!this.buttonsDown.delete(e.button)) return;
    this.channels()?.send({ type: 'mouse_button', button, down });
  }

  private onWheel(e: WheelEvent): void {
    e.preventDefault();
    this.wheel.dy += wheelUnits(e.deltaY, e.deltaMode);
    // Horizontal: the browser's deltaX > 0 is right, as in Windows.
    this.wheel.dx -= wheelUnits(e.deltaX, e.deltaMode);
    this.schedule();
  }

  private onKey(e: KeyboardEvent, down: boolean): void {
    const c = this.channels();
    if (!c?.open) return;
    // Typing into a field of this page, if one ever has focus, stays local.
    const target = e.target as HTMLElement | null;
    if (target && (target.tagName === 'INPUT' || target.tagName === 'TEXTAREA')) return;
    // So do keys inside a panel of the page, such as the file manager.
    if (target?.closest?.('[data-local-keys]')) return;
    const scancode = scancodeFor(e.code);
    if (scancode === undefined) return;
    if (this.pasteKey?.scancode === scancode) {
      // Repeats and the release of a held-back paste key wait for it.
      e.preventDefault();
      if (!down) this.pasteKey.up = true;
      return;
    }
    if (down && isPaste(e) && this.clipboard()) {
      // No preventDefault: the browser fires `paste` with the text, no permission needed (D-040).
      this.holdPaste(scancode);
      return;
    }
    e.preventDefault();
    if (down) this.keysDown.add(scancode);
    else if (!this.keysDown.delete(scancode)) return;
    // Held keys repeat in the browser; Windows does not repeat injected keys, so the repeats are sent too.
    c.send({ type: 'key', down, scancode });
  }

  /** Sends the browser's clipboard to the server, then the held paste key, so the server pastes the new text. */
  private holdPaste(scancode: number): void {
    this.pasteKey = { scancode, up: false };
    const done = () => {
      const held = this.pasteKey;
      this.pasteKey = null;
      const c = this.channels();
      if (!held || !c?.open) return;
      c.send({ type: 'key', down: true, scancode: held.scancode });
      if (held.up) c.send({ type: 'key', down: false, scancode: held.scancode });
      else this.keysDown.add(held.scancode);
    };
    let settled = false;
    const onPaste = (e: ClipboardEvent) => {
      if (settled) return;
      settled = true;
      e.preventDefault();
      const text = e.clipboardData?.getData('text/plain') ?? '';
      const sync = this.clipboard();
      if (!text || !sync) {
        done();
        return;
      }
      void sync.send(text).then(done, done);
    };
    window.addEventListener('paste', onPaste, { once: true });
    // No text in the clipboard, or no paste event at all: the key goes on its own.
    window.setTimeout(() => {
      window.removeEventListener('paste', onPaste);
      if (settled) return;
      settled = true;
      done();
    }, 300);
  }

  /** Keyboard Lock only works in full screen: take it there, give it back when leaving. */
  private async followFullscreen(): Promise<void> {
    const keyboard = (navigator as Navigator & { keyboard?: KeyboardLock }).keyboard;
    if (!keyboard) return;
    if (document.fullscreenElement) {
      try {
        await keyboard.lock();
        this.log('клавиатура захвачена: Esc, Alt+Tab и Win уходят на сервер, долгое Esc выходит');
      } catch (e) {
        this.log(`keyboard lock: ${String(e)}`);
      }
    } else {
      keyboard.unlock();
      this.releaseAll('выход из полноэкранного режима');
    }
  }
}
