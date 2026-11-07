import { describe, expect, it } from 'vitest';
import { UploadQueue, type Job } from './queue';
import type { Source, UploadView } from './upload';

/** A job that finishes when the test says so. */
class FakeJob implements Job {
  view: UploadView;
  cancelled = false;
  private finish: (() => void) | null = null;

  constructor(
    source: Source,
    path: string,
    private readonly changed: (v: UploadView) => void,
  ) {
    this.view = { name: source.name, path, size: source.size, sent: 0, phase: 'starting', speed: 0 };
  }

  get active(): boolean {
    return !['done', 'error', 'cancelled'].includes(this.view.phase);
  }

  run(): Promise<void> {
    this.set({ phase: 'uploading' });
    return new Promise((resolve) => {
      this.finish = resolve;
    });
  }

  send(sent: number): void {
    this.set({ sent });
  }

  done(): void {
    this.set({ phase: 'done', sent: this.view.size });
    this.finish?.();
  }

  async cancel(): Promise<void> {
    this.cancelled = true;
    this.set({ phase: 'cancelled' });
    this.finish?.();
  }

  private set(patch: Partial<UploadView>): void {
    Object.assign(this.view, patch);
    this.changed({ ...this.view });
  }
}

const file = (name: string, size: number): Source => ({
  name,
  size,
  lastModified: 1,
  slice: () => new Blob(),
});

function setup() {
  const jobs: FakeJob[] = [];
  const queue = new UploadQueue((source, path, changed) => {
    const j = new FakeJob(source, path, changed);
    jobs.push(j);
    return j;
  });
  return { queue, jobs };
}

const flush = () => new Promise((r) => setTimeout(r, 0));

describe('upload queue', () => {
  it('runs one upload at a time and reports the finished ones', async () => {
    const { queue, jobs } = setup();
    const finished: string[] = [];
    queue.onFinished((p) => finished.push(p));
    queue.add([
      { source: file('a.bin', 10), path: 'a.bin' },
      { source: file('b.bin', 20), path: 'dir/b.bin' },
    ]);
    await flush();
    expect(jobs.map((j) => j.view.phase)).toEqual(['uploading', 'starting']);
    jobs[0]!.done();
    await flush();
    expect(jobs.map((j) => j.view.phase)).toEqual(['done', 'uploading']);
    jobs[1]!.done();
    await flush();
    expect(finished).toEqual(['a.bin', 'dir/b.bin']);
    expect(queue.active).toBe(false);
  });

  it('keeps the uploads and their progress without any listener', async () => {
    const { queue, jobs } = setup();
    let changes = 0;
    const off = queue.subscribe(() => changes++);
    queue.add([{ source: file('big.bin', 100), path: 'big.bin' }]);
    await flush();
    // The panel closes: its listener goes, the upload goes on.
    off();
    const before = changes;
    jobs[0]!.send(40);
    expect(changes).toBe(before);
    // The panel opens again and sees where the upload is.
    expect(queue.rows[0]!.view.sent).toBe(40);
    expect(queue.progress()).toEqual({ count: 1, sent: 40, size: 100 });
  });

  it('refuses the same file while it is being uploaded, takes it again once finished', async () => {
    const { queue, jobs } = setup();
    queue.add([{ source: file('x.bin', 5), path: 'x.bin' }]);
    expect(queue.add([{ source: file('x.bin', 5), path: 'x.bin' }])).toEqual(['x.bin']);
    // The same name in one pick counts too.
    expect(
      queue.add([
        { source: file('y.bin', 5), path: 'y.bin' },
        { source: file('y.bin', 5), path: 'y.bin' },
      ]),
    ).toEqual(['y.bin']);
    expect(jobs).toHaveLength(2);
    await flush();
    jobs[0]!.done();
    await flush();
    expect(queue.add([{ source: file('x.bin', 5), path: 'x.bin' }])).toEqual([]);
    expect(jobs).toHaveLength(3);
  });

  it('dismisses only finished rows', async () => {
    const { queue, jobs } = setup();
    queue.add([{ source: file('a', 1), path: 'a' }]);
    await flush();
    queue.dismiss(queue.rows[0]!.key);
    expect(queue.rows).toHaveLength(1);
    await jobs[0]!.cancel();
    await flush();
    queue.dismiss(queue.rows[0]!.key);
    expect(queue.rows).toHaveLength(0);
  });
});
