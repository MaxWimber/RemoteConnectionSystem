// The shared clipboard (D-040): the `clipboard` channel of one connection, the browser's clipboard, and the wait
// for the server to confirm a text before the paste keys go out.

import { Assembler, encodeText, MAX_TEXT, parseMessage, utf8Length } from './protocol';

const KEY = 'rcs.clipboard';
/** The paste keys go out after this long even without the server's confirmation. */
export const APPLY_TIMEOUT_MS = 500;

export function loadClipboardSharing(): boolean {
  try {
    return localStorage.getItem(KEY) !== 'off';
  } catch {
    return true;
  }
}

export function saveClipboardSharing(on: boolean): void {
  try {
    localStorage.setItem(KEY, on ? 'on' : 'off');
  } catch {
    // Private mode: the choice lasts until the page closes.
  }
}

export interface ClipboardEvents {
  /** Something for the user to see: a text that waits for a tap, one over the limit. Empty clears it. */
  notice(text: string): void;
  log(message: string): void;
}

export class ClipboardSync {
  private readonly assembler = new Assembler();
  private applied: ((total: number) => void) | null = null;
  /** A text from the server the browser would not write without a gesture. */
  private pending: string | null = null;

  constructor(
    private readonly channel: RTCDataChannel,
    private readonly events: ClipboardEvents,
  ) {
    channel.binaryType = 'arraybuffer';
    channel.onmessage = (e: MessageEvent) => {
      if (e.data instanceof ArrayBuffer) this.receive(new Uint8Array(e.data));
    };
    const retry = () => {
      if (this.pending !== null) void this.write(this.pending);
    };
    for (const type of ['pointerdown', 'keydown', 'focus']) {
      window.addEventListener(type, retry, { capture: true, passive: true });
    }
    channel.addEventListener('close', () => {
      for (const type of ['pointerdown', 'keydown', 'focus']) {
        window.removeEventListener(type, retry, { capture: true });
      }
    });
  }

  get open(): boolean {
    return this.channel.readyState === 'open';
  }

  close(): void {
    this.channel.close();
  }

  /** Sends `text` to the server's clipboard; resolves when the server confirms it or after the timeout. */
  async send(text: string): Promise<boolean> {
    const messages = encodeText(text);
    if (!messages) {
      this.events.notice(`Текст больше ${MAX_TEXT / (1 << 20)} МБ, на сервер не передан`);
      return false;
    }
    if (!this.open) return false;
    const total = utf8Length(text);
    const confirmed = new Promise<boolean>((resolve) => {
      const timer = window.setTimeout(() => {
        this.applied = null;
        resolve(false);
      }, APPLY_TIMEOUT_MS);
      this.applied = (got) => {
        if (got !== total) return;
        window.clearTimeout(timer);
        this.applied = null;
        resolve(true);
      };
    });
    for (const m of messages) this.channel.send(m.buffer as ArrayBuffer);
    this.events.log(`буфер обмена → сервер: ${total} байт`);
    return confirmed;
  }

  private receive(b: Uint8Array): void {
    const m = parseMessage(b);
    if (!m) {
      this.events.log('буфер обмена: неверное сообщение');
      return;
    }
    if (m.type === 'applied') {
      this.applied?.(m.total);
      return;
    }
    if (m.type === 'too_large') {
      this.events.notice(`Текст в буфере сервера больше 1 МБ (${Math.round(m.size / 1024)} КБ), не передан`);
      return;
    }
    let text: string | null;
    try {
      text = this.assembler.add(m.total, m.offset, m.part);
    } catch (e) {
      this.events.log(`буфер обмена: ${String(e)}`);
      return;
    }
    if (text === null) return;
    this.events.log(`буфер обмена ← сервер: ${m.total} байт`);
    void this.write(text);
  }

  private async write(text: string): Promise<void> {
    try {
      await navigator.clipboard.writeText(text);
      if (this.pending !== null) this.events.notice('');
      this.pending = null;
    } catch {
      // Not focused, or a phone that wants a gesture: the next tap or key writes it.
      if (this.pending === null) this.events.notice('Текст с сервера скопируется после касания или нажатия клавиши');
      this.pending = text;
    }
  }
}
