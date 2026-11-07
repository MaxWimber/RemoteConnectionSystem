// Uploads of this tab, one at a time. The queue lives as long as the page, not the file panel: closing the panel
// leaves the uploads running and the 📁 button shows their progress.

import { useEffect, useState } from 'preact/hooks';
import { browserDeps, Upload, type Source, type UploadView } from './upload';

export interface Job {
  readonly view: UploadView;
  readonly active: boolean;
  run(): Promise<void>;
  cancel(): Promise<void>;
}

export interface QueueRow {
  key: number;
  job: Job;
  view: UploadView;
}

export type MakeJob = (source: Source, path: string, changed: (v: UploadView) => void) => Job;

export interface Progress {
  /** Uploads not yet finished, the waiting ones included. */
  count: number;
  sent: number;
  size: number;
}

export class UploadQueue {
  rows: readonly QueueRow[] = [];
  private readonly listeners = new Set<() => void>();
  private readonly finished = new Set<(path: string) => void>();
  private running = false;
  private next = 1;

  constructor(private readonly make: MakeJob) {}

  /** Calls `fn` after every change of `rows`. */
  subscribe(fn: () => void): () => void {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }

  /** Calls `fn` with the path of every upload that is done. */
  onFinished(fn: (path: string) => void): () => void {
    this.finished.add(fn);
    return () => this.finished.delete(fn);
  }

  get active(): boolean {
    return this.rows.some((r) => r.job.active);
  }

  progress(): Progress {
    const busy = this.rows.filter((r) => r.job.active);
    return {
      count: busy.length,
      sent: busy.reduce((s, r) => s + r.view.sent, 0),
      size: busy.reduce((s, r) => s + r.view.size, 0),
    };
  }

  /** Queues the files; returns the names refused because the same file is already being uploaded here. */
  add(items: Array<{ source: Source; path: string }>): string[] {
    const refused: string[] = [];
    const added: QueueRow[] = [];
    for (const { source, path } of items) {
      const same = [...this.rows, ...added].some(
        (r) => r.job.active && r.view.path === path && r.view.size === source.size,
      );
      if (same) {
        refused.push(source.name);
        continue;
      }
      const key = this.next++;
      const job = this.make(source, path, (view) => this.update(key, view));
      added.push({ key, job, view: { ...job.view } });
    }
    if (added.length > 0) {
      this.set([...this.rows, ...added]);
      void this.pump();
    }
    return refused;
  }

  /** Takes a finished row off the list. */
  dismiss(key: number): void {
    this.set(this.rows.filter((r) => r.key !== key || r.job.active));
  }

  private set(rows: readonly QueueRow[]): void {
    this.rows = rows;
    for (const fn of this.listeners) fn();
  }

  private update(key: number, view: UploadView): void {
    this.set(this.rows.map((r) => (r.key === key ? { ...r, view } : r)));
  }

  /** One upload at a time: they share the disk and the uplink. */
  private async pump(): Promise<void> {
    if (this.running) return;
    this.running = true;
    try {
      for (;;) {
        const row = this.rows.find((r) => r.job.view.phase === 'starting');
        if (!row) break;
        await row.job.run();
        if (row.job.view.phase === 'done') for (const fn of this.finished) fn(row.job.view.path);
      }
    } finally {
      this.running = false;
    }
  }
}

/** Re-renders the component on every change of the queue. */
export function useQueue(queue: UploadQueue): readonly QueueRow[] {
  const [, tick] = useState(0);
  useEffect(() => queue.subscribe(() => tick((n) => n + 1)), [queue]);
  return queue.rows;
}

/** The page's queue: real uploads, a question before replacing an existing file. */
export function browserQueue(base: string): UploadQueue {
  const deps = browserDeps(base, async (name) => confirm(`«${name}» уже есть в этой папке. Заменить?`));
  return new UploadQueue((source, path, changed) => new Upload(source, path, deps, changed));
}
