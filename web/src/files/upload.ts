// Uploads with resume (docs/PROTOCOL.md «Загрузка с докачкой», D-026). Chunks go one after another; the next one is
// read and hashed while the current one is on the wire. After a break the client asks the server how much it has and
// goes on from there, also after the page was reloaded if the same file is picked again.

import { ApiError, apiUrl, check } from './api';

export type Phase = 'starting' | 'uploading' | 'retrying' | 'completing' | 'done' | 'error' | 'cancelled';

export interface UploadView {
  name: string;
  path: string;
  size: number;
  sent: number;
  phase: Phase;
  /** Bytes per second over the last few seconds. */
  speed: number;
  sha256?: string;
  error?: string;
  retryInSec?: number;
}

export interface Store {
  get(key: string): string | null;
  set(key: string, value: string): void;
  remove(key: string): void;
}

export interface Deps {
  base: string;
  fetch: typeof fetch;
  digest(data: ArrayBuffer): Promise<string>;
  sleep(ms: number, signal: AbortSignal): Promise<void>;
  now(): number;
  store: Store;
  /** Asked once when the target exists; `false` leaves the upload cancelled. */
  confirmOverwrite(name: string): Promise<boolean>;
  /** Runs `fn` while holding `name` in every tab of this site, or returns `'busy'` at once if another holds it. */
  exclusive<T>(name: string, fn: () => Promise<T>): Promise<T | 'busy'>;
}

export interface Source {
  readonly name: string;
  readonly size: number;
  readonly lastModified: number;
  slice(start: number, end: number): Blob;
}

const SPEED_WINDOW_MS = 5000;
const MAX_BACKOFF_MS = 30_000;
const MAX_BAD_HASH = 3;
/** `409` this many times in a row: someone else is writing the same upload. */
const MAX_CONFLICTS = 3;

export async function sha256Hex(data: ArrayBuffer): Promise<string> {
  const d = new Uint8Array(await crypto.subtle.digest('SHA-256', data));
  return Array.from(d, (b) => b.toString(16).padStart(2, '0')).join('');
}

export const browserDeps = (base: string, confirmOverwrite: (name: string) => Promise<boolean>): Deps => ({
  base,
  fetch: (...a) => fetch(...a),
  digest: sha256Hex,
  sleep: (ms, signal) =>
    new Promise((resolve) => {
      const t = setTimeout(resolve, ms);
      signal.addEventListener('abort', () => {
        clearTimeout(t);
        resolve();
      });
    }),
  now: () => performance.now(),
  store: {
    get: (k) => {
      try {
        return localStorage.getItem(k);
      } catch {
        return null;
      }
    },
    set: (k, v) => {
      try {
        localStorage.setItem(k, v);
      } catch {
        // Resume after a reload is a convenience.
      }
    },
    remove: (k) => {
      try {
        localStorage.removeItem(k);
      } catch {
        // As above.
      }
    },
  },
  confirmOverwrite,
  exclusive: async (name, fn) => {
    const locks = (navigator as Navigator & { locks?: LockManager }).locks;
    if (!locks) return fn();
    return locks.request(name, { ifAvailable: true }, async (lock) => (lock ? fn() : ('busy' as const)));
  },
});

/** The browser can no longer read the picked file: it was changed, moved or deleted after the pick. */
function unreadable(e: unknown): boolean {
  return e instanceof DOMException && (e.name === 'NotReadableError' || e.name === 'NotFoundError');
}

/** A break the client waits out: no network, a proxy error, a server restart. */
function transient(e: unknown): boolean {
  if (e instanceof ApiError) return e.status >= 500 && e.status !== 507;
  if (unreadable(e)) return false;
  return !(e instanceof DOMException && e.name === 'AbortError');
}

interface Prepared {
  offset: number;
  data: ArrayBuffer;
  sha: string;
}

export class Upload {
  readonly view: UploadView;
  private id = '';
  private chunk = 8 * 1024 * 1024;
  private readonly abort = new AbortController();
  private readonly samples: Array<[number, number]> = [];
  private readonly key: string;

  constructor(
    private readonly file: Source,
    path: string,
    private readonly deps: Deps,
    private readonly changed: (v: UploadView) => void,
  ) {
    this.view = { name: file.name, path, size: file.size, sent: 0, phase: 'starting', speed: 0 };
    this.key = `rcs-upload:${path}:${file.size}:${file.lastModified}`;
  }

  get active(): boolean {
    return !['done', 'error', 'cancelled'].includes(this.view.phase);
  }

  private set(patch: Partial<UploadView>): void {
    Object.assign(this.view, patch);
    this.changed({ ...this.view });
  }

  private progress(received: number): void {
    const t = this.deps.now();
    this.samples.push([t, received]);
    while (this.samples.length > 2 && t - this.samples[0]![0] > SPEED_WINDOW_MS) this.samples.shift();
    const [t0, r0] = this.samples[0]!;
    const speed = t > t0 ? ((received - r0) * 1000) / (t - t0) : 0;
    this.set({ sent: received, speed: Math.max(0, speed) });
  }

  private url(path: string): string {
    return apiUrl(this.deps.base, path);
  }

  private request(path: string, method: string, body?: BodyInit, headers: Record<string, string> = {}) {
    return this.deps.fetch(this.url(path), {
      method,
      credentials: 'same-origin',
      cache: 'no-store',
      headers: { 'X-RCS-Request': '1', ...headers },
      body,
      signal: this.abort.signal,
    });
  }

  private async status(): Promise<number> {
    const b = await check(await this.request(`uploads/${this.id}`, 'GET'));
    return Number(b['received'] ?? 0);
  }

  /** Picks up an upload of the same file from before a reload, or starts a new one. */
  private async open(): Promise<number> {
    const saved = this.deps.store.get(this.key);
    if (saved) {
      this.id = saved;
      try {
        return await this.status();
      } catch (e) {
        if (transient(e)) throw e;
        this.deps.store.remove(this.key);
      }
    }
    const create = (overwrite: boolean) =>
      this.request('uploads', 'POST', JSON.stringify({ path: this.view.path, size: this.file.size, overwrite }), {
        'Content-Type': 'application/json',
      });
    let r = await create(false);
    if (r.status === 409 && (await r.clone().json().catch(() => ({})))?.error === 'exists') {
      if (!(await this.deps.confirmOverwrite(this.view.name))) throw new DOMException('skipped', 'AbortError');
      r = await create(true);
    }
    const b = await check(r);
    this.id = String(b['id']);
    this.chunk = Number(b['chunk_size'] ?? this.chunk);
    this.deps.store.set(this.key, this.id);
    return Number(b['received'] ?? 0);
  }

  private async prepare(offset: number): Promise<Prepared> {
    const end = Math.min(offset + this.chunk, this.file.size);
    const data = await this.file.slice(offset, end).arrayBuffer();
    return { offset, data, sha: await this.deps.digest(data) };
  }

  /** Sends chunks from `received` to the end; returns when the server has everything. */
  private async send(received: number): Promise<void> {
    let badHash = 0;
    let conflicts = 0;
    let next: Promise<Prepared> | null = null;
    while (received < this.file.size) {
      const current: Prepared = await (next && (await next).offset === received ? next : this.prepare(received));
      const following = current.offset + current.data.byteLength;
      next = following < this.file.size ? this.prepare(following) : null;
      // Read while the current chunk is on the wire; a failed read surfaces when the chunk is used, not before.
      next?.catch(() => {});
      const r = await this.request(`uploads/${this.id}?offset=${current.offset}`, 'PUT', current.data, {
        'X-Chunk-SHA256': current.sha,
      });
      if (r.status === 409) {
        const b = (await r.json().catch(() => ({}))) as Record<string, unknown>;
        if (++conflicts >= MAX_CONFLICTS) throw new ApiError(409, 'another_writer', b);
        received = Number(b['received'] ?? (await this.status()));
        next = null;
        continue;
      }
      if (r.status === 422 && ++badHash < MAX_BAD_HASH) {
        next = null;
        continue;
      }
      const b = await check(r);
      received = Number(b['received']);
      badHash = 0;
      conflicts = 0;
      this.progress(received);
    }
  }

  private async complete(): Promise<string> {
    const r = await this.request(`uploads/${this.id}/complete`, 'POST');
    const b = await check(r);
    return String(b['sha256'] ?? '');
  }

  /** One writer per file across tabs and rows: a second one would only collide with the first (D-026). */
  async run(): Promise<void> {
    const result = await this.deps.exclusive(this.key, () => this.loop());
    if (result === 'busy') this.set({ phase: 'error', error: 'busy', speed: 0 });
  }

  private async loop(): Promise<void> {
    let backoff = 1000;
    let completing = false;
    for (;;) {
      try {
        if (!this.id) {
          const received = await this.open();
          this.samples.length = 0;
          this.progress(received);
        }
        if (!completing) {
          this.set({ phase: 'uploading', retryInSec: undefined });
          await this.send(this.view.sent);
          completing = true;
        }
        this.set({ phase: 'completing' });
        const sha256 = await this.complete();
        this.deps.store.remove(this.key);
        this.set({ phase: 'done', sha256, sent: this.file.size, speed: 0 });
        return;
      } catch (e) {
        if (this.abort.signal.aborted || (e instanceof DOMException && e.name === 'AbortError')) {
          this.set({ phase: 'cancelled', speed: 0 });
          return;
        }
        if (e instanceof ApiError && e.code === 'incomplete') {
          completing = false;
          this.progress(Number(e.body['received'] ?? 0));
          continue;
        }
        if (completing && e instanceof ApiError && e.status === 404) {
          // The answer to an earlier `complete` was lost: the server already finished and forgot the upload.
          this.deps.store.remove(this.key);
          this.set({ phase: 'done', sent: this.file.size, speed: 0 });
          return;
        }
        if (!transient(e)) {
          const error = e instanceof ApiError ? e.code : unreadable(e) ? 'file_unreadable' : String(e);
          // The server keeps what it has: the same file picked again goes on from there.
          this.set({ phase: 'error', error, speed: 0 });
          return;
        }
        this.set({ phase: 'retrying', retryInSec: Math.round(backoff / 1000), speed: 0 });
        await this.deps.sleep(backoff, this.abort.signal);
        backoff = Math.min(backoff * 2, MAX_BACKOFF_MS);
        if (this.abort.signal.aborted) continue;
        try {
          if (this.id && !completing) this.progress(await this.status());
          backoff = 1000;
        } catch {
          // Still down; the loop waits again.
        }
      }
    }
  }

  /** Stops sending and removes what the server has so far. */
  async cancel(): Promise<void> {
    this.abort.abort();
    // Without an id this one never started: the upload, if any, belongs to the writer that holds the lock.
    if (!this.id || this.view.phase === 'done') return;
    this.deps.store.remove(this.key);
    try {
      await this.deps.fetch(this.url(`uploads/${this.id}`), {
        method: 'DELETE',
        credentials: 'same-origin',
        headers: { 'X-RCS-Request': '1' },
      });
    } catch {
      // Left on the server, the hourly sweep removes it after 7 days.
    }
  }
}
