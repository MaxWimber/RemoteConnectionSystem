import { describe, expect, it } from 'vitest';
import { sha256Hex, Upload, type Deps, type UploadView } from './upload';

const enc = new TextEncoder();

/** The upload half of docs/PROTOCOL.md in memory, with breaks on request. */
class FakeServer {
  files = new Map<string, Uint8Array>();
  uploads = new Map<string, { path: string; size: number; overwrite: boolean; data: number[] }>();
  chunk = 4;
  /** PUTs that fail with a network error before reaching the server. */
  failPuts = new Set<number>();
  /** `complete` happens but its answer is lost. */
  loseComplete = false;
  puts = 0;
  deletes = 0;
  private next = 1;

  fetch = async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
    const url = new URL(String(input), 'https://rcs.test');
    const path = url.pathname.replace(/^.*\/api\//, '');
    const method = init?.method ?? 'GET';
    const json = (status: number, body: unknown) =>
      new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } });
    if (init?.signal?.aborted) throw new DOMException('aborted', 'AbortError');
    if (path === 'uploads' && method === 'POST') {
      const b = JSON.parse(String(init?.body)) as { path: string; size: number; overwrite: boolean };
      if (this.files.has(b.path) && !b.overwrite) return json(409, { error: 'exists' });
      const id = `id${this.next++}`;
      this.uploads.set(id, { ...b, data: [] });
      return json(201, { id, chunk_size: this.chunk, received: 0 });
    }
    const m = /^uploads\/([^/]+)(\/complete)?$/.exec(path);
    const u = m ? this.uploads.get(m[1]!) : undefined;
    if (!m || !u) return json(404, { error: 'not_found' });
    if (m[2] && method === 'POST') {
      if (u.data.length !== u.size) return json(409, { error: 'incomplete', received: u.data.length });
      const bytes = new Uint8Array(u.data);
      this.files.set(u.path, bytes);
      this.uploads.delete(m[1]!);
      if (this.loseComplete) {
        this.loseComplete = false;
        throw new TypeError('network');
      }
      return json(200, { path: u.path, sha256: await sha256Hex(bytes.buffer) });
    }
    if (method === 'GET') return json(200, { received: u.data.length, size: u.size });
    if (method === 'DELETE') {
      this.deletes++;
      this.uploads.delete(m[1]!);
      return new Response(null, { status: 204 });
    }
    if (method === 'PUT') {
      this.puts++;
      if (this.failPuts.has(this.puts)) throw new TypeError('Failed to fetch');
      const offset = Number(url.searchParams.get('offset'));
      if (offset !== u.data.length) return json(409, { error: 'offset_mismatch', received: u.data.length });
      const data = new Uint8Array(init!.body as ArrayBuffer);
      const sha = (init!.headers as Record<string, string>)['X-Chunk-SHA256'];
      if (sha !== (await sha256Hex(data.slice().buffer))) return json(422, { error: 'chunk_hash_mismatch' });
      u.data.push(...data);
      return json(200, { received: u.data.length });
    }
    return json(405, { error: 'method' });
  };
}

function deps(server: FakeServer, overwrite = true, store = new Map<string, string>()): Deps {
  let t = 0;
  return {
    base: '/RemoteConnectionSystem/',
    fetch: server.fetch as typeof fetch,
    digest: sha256Hex,
    sleep: async () => {},
    now: () => (t += 100),
    store: { get: (k) => store.get(k) ?? null, set: (k, v) => void store.set(k, v), remove: (k) => void store.delete(k) },
    confirmOverwrite: async () => overwrite,
  };
}

function file(text: string, name = 'a.txt'): File {
  return new File([enc.encode(text)], name, { lastModified: 1000 });
}

async function run(server: FakeServer, f: File, path: string, d = deps(server)) {
  const views: UploadView[] = [];
  const u = new Upload(f, path, d, (v) => views.push(v));
  await u.run();
  return { u, views, phases: views.map((v) => v.phase) };
}

describe('upload', () => {
  it('sends chunks in order and gets the server hash', async () => {
    const s = new FakeServer();
    const { u } = await run(s, file('hello, world!'), 'Docs/a.txt');
    expect(u.view.phase).toBe('done');
    expect(new TextDecoder().decode(s.files.get('Docs/a.txt'))).toBe('hello, world!');
    expect(u.view.sha256).toBe(await sha256Hex(enc.encode('hello, world!').buffer));
    expect(s.puts).toBe(4);
    expect(u.view.sent).toBe(13);
  });

  it('waits out a break and goes on from what the server has', async () => {
    const s = new FakeServer();
    s.failPuts = new Set([2, 3]);
    const { u, phases } = await run(s, file('0123456789abcdef'), 'b.bin');
    expect(phases).toContain('retrying');
    expect(u.view.phase).toBe('done');
    expect(new TextDecoder().decode(s.files.get('b.bin'))).toBe('0123456789abcdef');
  });

  it('resumes after a reload when the same file is picked again', async () => {
    const s = new FakeServer();
    s.uploads.set('old', { path: 'c.bin', size: 10, overwrite: false, data: [...enc.encode('0123')] });
    const store = new Map([['rcs-upload:c.bin:10:1000', 'old']]);
    const { u } = await run(s, file('0123456789'), 'c.bin', deps(s, true, store));
    expect(u.view.phase).toBe('done');
    expect(s.puts).toBe(1); // the rest of the file in one default-sized chunk
    expect(store.size).toBe(0);
    expect(new TextDecoder().decode(s.files.get('c.bin'))).toBe('0123456789');
  });

  it('a forgotten saved upload starts over', async () => {
    const s = new FakeServer();
    const store = new Map([['rcs-upload:d.bin:3:1000', 'gone']]);
    const { u } = await run(s, file('xyz'), 'd.bin', deps(s, true, store));
    expect(u.view.phase).toBe('done');
  });

  it('asks before replacing a file', async () => {
    const s = new FakeServer();
    s.files.set('e.txt', enc.encode('old'));
    const declined = await run(s, file('new'), 'e.txt', deps(s, false));
    expect(declined.u.view.phase).toBe('cancelled');
    expect(new TextDecoder().decode(s.files.get('e.txt'))).toBe('old');
    const accepted = await run(s, file('new'), 'e.txt', deps(s, true));
    expect(accepted.u.view.phase).toBe('done');
    expect(new TextDecoder().decode(s.files.get('e.txt'))).toBe('new');
  });

  it('a lost answer to complete still ends as done', async () => {
    const s = new FakeServer();
    s.loseComplete = true;
    const { u } = await run(s, file('abcdefgh'), 'f.bin');
    expect(u.view.phase).toBe('done');
    expect(new TextDecoder().decode(s.files.get('f.bin'))).toBe('abcdefgh');
  });

  it('cancel stops and removes the server copy', async () => {
    const s = new FakeServer();
    s.failPuts = new Set([1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
    const d = deps(s);
    let u: Upload | null = null;
    d.sleep = async () => {
      await u?.cancel();
    };
    u = new Upload(file('0123456789'), 'g.bin', d, () => {});
    await u.run();
    expect(u.view.phase).toBe('cancelled');
    expect(s.deletes).toBe(1);
    expect(s.uploads.size).toBe(0);
  });

  it('a refused request is an error, not a retry', async () => {
    const s = new FakeServer();
    const d = deps(s);
    d.fetch = (async () =>
      new Response(JSON.stringify({ error: 'disk_full' }), { status: 507 })) as typeof fetch;
    const u = new Upload(file('x'), 'h.bin', d, () => {});
    await u.run();
    expect(u.view.phase).toBe('error');
    expect(u.view.error).toBe('disk_full');
  });
});
