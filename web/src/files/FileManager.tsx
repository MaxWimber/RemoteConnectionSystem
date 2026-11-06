import { useEffect, useRef, useState } from 'preact/hooks';
import {
  downloadUrl,
  errorText,
  hash,
  joinPath,
  list,
  mkdir,
  parentPath,
  remove,
  rename,
  type Entry,
  codeText,
} from './api';
import { formatDate, formatEta, formatSize, formatSpeed } from './format';
import { browserDeps, Upload, type UploadView } from './upload';

const TRASH = '.rcs-trash';

interface Props {
  base: string;
  onClose(): void;
}

interface Row {
  key: number;
  upload: Upload;
  view: UploadView;
}

const phaseText: Record<UploadView['phase'], string> = {
  starting: 'ожидание',
  uploading: '',
  retrying: 'обрыв, повтор',
  completing: 'проверка SHA-256',
  done: 'готово',
  error: 'ошибка',
  cancelled: 'отменено',
};

function displayName(e: Entry, dir: string): string {
  return dir === '' && e.name === TRASH ? 'Корзина' : e.name;
}

function icon(e: Entry): string {
  if (e.type === 'link') return '🔗';
  if (e.type === 'dir') return e.name === TRASH ? '🗑' : '📁';
  return '📄';
}

/** Files under files.root (docs/PROTOCOL.md «Файлы»): browse, upload with resume, download, rename, move, trash. */
export function FileManager({ base, onClose }: Props) {
  const [dir, setDir] = useState('');
  const [entries, setEntries] = useState<Entry[]>([]);
  const [loading, setLoading] = useState(false);
  const [message, setMessage] = useState('');
  const [hashes, setHashes] = useState<Record<string, string>>({});
  const [rows, setRows] = useState<Row[]>([]);
  const [dragging, setDragging] = useState(false);
  const picker = useRef<HTMLInputElement>(null);
  const nextKey = useRef(1);
  const queue = useRef<Row[]>([]);
  const running = useRef(false);
  const dirRef = useRef(dir);
  dirRef.current = dir;
  const inTrash = dir === TRASH || dir.startsWith(`${TRASH}/`);

  const refresh = async (path = dirRef.current) => {
    setLoading(true);
    try {
      setEntries(await list(base, path));
      setMessage('');
    } catch (e) {
      setMessage(errorText(e));
    } finally {
      setLoading(false);
    }
  };

  const open = (path: string) => {
    setDir(path);
    setHashes({});
    void refresh(path);
  };

  useEffect(() => {
    void refresh('');
  }, []);

  // A leaving page cancels nothing: the server keeps the uploads and the same file resumes after a reload.
  useEffect(() => {
    const warn = (e: BeforeUnloadEvent) => {
      if (queue.current.some((r) => r.upload.active)) e.preventDefault();
    };
    window.addEventListener('beforeunload', warn);
    return () => window.removeEventListener('beforeunload', warn);
  }, []);

  const update = (key: number, view: UploadView) =>
    setRows((all) => all.map((r) => (r.key === key ? { ...r, view } : r)));

  /** One upload at a time: they share the disk and the uplink. */
  const pump = async () => {
    if (running.current) return;
    running.current = true;
    try {
      for (;;) {
        const row = queue.current.find((r) => r.upload.view.phase === 'starting');
        if (!row) break;
        await row.upload.run();
        if (row.upload.view.phase === 'done' && parentPath(row.view.path) === dirRef.current) void refresh();
      }
    } finally {
      running.current = false;
    }
  };

  const enqueue = (files: FileList | File[]) => {
    const deps = browserDeps(base, async (name) => confirm(`«${name}» уже есть в этой папке. Заменить?`));
    const added: Row[] = [];
    for (const f of Array.from(files)) {
      const path = joinPath(dirRef.current, f.name);
      const same = queue.current.find(
        (r) => r.upload.active && r.view.path === path && r.view.size === f.size,
      );
      if (same) {
        setMessage(`«${f.name}» уже загружается.`);
        continue;
      }
      const key = nextKey.current++;
      const upload = new Upload(f, path, deps, (v) => update(key, v));
      added.push({ key, upload, view: { ...upload.view } });
    }
    queue.current = [...queue.current, ...added];
    setRows((all) => [...all, ...added]);
    void pump();
  };

  const act = async (what: () => Promise<unknown>) => {
    try {
      await what();
      await refresh();
    } catch (e) {
      setMessage(errorText(e));
    }
  };

  const newFolder = () => {
    const name = prompt('Имя новой папки');
    if (name) void act(() => mkdir(base, joinPath(dir, name)));
  };
  const renameEntry = (e: Entry) => {
    const name = prompt('Новое имя', e.name);
    if (name && name !== e.name) void act(() => rename(base, joinPath(dir, e.name), joinPath(dir, name)));
  };
  const moveEntry = (e: Entry) => {
    const target = prompt('В какую папку перенести? Путь от корня хранилища, пусто — корень.', inTrash ? '' : dir);
    if (target === null) return;
    const clean = target.replace(/^\/+|\/+$/g, '');
    void act(() => rename(base, joinPath(dir, e.name), joinPath(clean, e.name)));
  };
  const deleteEntry = (e: Entry) => {
    if (confirm(`Переместить «${e.name}» в корзину?`)) void act(() => remove(base, joinPath(dir, e.name)));
  };
  const hashEntry = async (e: Entry) => {
    setHashes((h) => ({ ...h, [e.name]: 'считается…' }));
    try {
      const sha = await hash(base, joinPath(dir, e.name));
      setHashes((h) => ({ ...h, [e.name]: sha }));
    } catch (err) {
      setHashes((h) => ({ ...h, [e.name]: errorText(err) }));
    }
  };

  const onDrop = (e: DragEvent) => {
    e.preventDefault();
    setDragging(false);
    const items = Array.from(e.dataTransfer?.items ?? []);
    const folders = items.filter((i) => i.webkitGetAsEntry?.()?.isDirectory).length;
    const files = Array.from(e.dataTransfer?.files ?? []).filter(
      (_, n) => !items[n]?.webkitGetAsEntry?.()?.isDirectory,
    );
    if (folders > 0) setMessage('Папки целиком не загружаются: перетащите файлы из них.');
    if (inTrash) {
      setMessage('В корзину загружать нельзя.');
      return;
    }
    if (files.length > 0) enqueue(files);
  };

  const crumbs = dir === '' ? [] : dir.split('/');

  return (
    <div
      class={`files${dragging ? ' dragging' : ''}`}
      data-local-keys
      onDragOver={(e) => {
        e.preventDefault();
        setDragging(true);
      }}
      onDragLeave={(e) => {
        if (e.currentTarget === e.target) setDragging(false);
      }}
      onDrop={onDrop}
    >
      <div class="files-head">
        <nav class="crumbs">
          <button class="crumb" onClick={() => open('')}>
            Хранилище
          </button>
          {crumbs.map((c, i) => (
            <span key={i}>
              <span>/</span>
              <button class="crumb" onClick={() => open(crumbs.slice(0, i + 1).join('/'))}>
                {i === 0 && c === TRASH ? 'Корзина' : c}
              </button>
            </span>
          ))}
        </nav>
        <div class="files-actions">
          {!inTrash && (
            <>
              <button onClick={() => picker.current?.click()} title="Загрузить файлы">
                ⬆
              </button>
              <button onClick={newFolder} title="Новая папка">
                ＋📁
              </button>
            </>
          )}
          <button onClick={() => void refresh()} title="Обновить">
            ⟳
          </button>
          <button onClick={onClose} title="Закрыть">
            ✕
          </button>
        </div>
        <input
          ref={picker}
          type="file"
          multiple
          hidden
          onChange={(e) => {
            const input = e.currentTarget;
            if (input.files?.length) enqueue(input.files);
            input.value = '';
          }}
        />
      </div>
      {message && <div class="files-message">{message}</div>}
      {inTrash && <div class="files-message">Корзина: удаление здесь только вручную на сервере, вернуть — «⇄».</div>}
      {rows.length > 0 && (
        <div class="uploads">
          {rows.map(({ key, upload, view }) => {
            const pct = view.size > 0 ? (view.sent / view.size) * 100 : 100;
            const left = view.speed > 0 ? (view.size - view.sent) / view.speed : Infinity;
            return (
              <div class={`upload ${view.phase}`} key={key}>
                <div class="upload-line">
                  <span class="upload-name" title={view.path}>
                    {view.name}
                  </span>
                  {upload.active ? (
                    <button onClick={() => void upload.cancel()} title="Отменить">
                      ✕
                    </button>
                  ) : (
                    <button onClick={() => setRows((all) => all.filter((r) => r.key !== key))} title="Убрать">
                      ✓
                    </button>
                  )}
                </div>
                <progress max={100} value={pct} />
                <div class="detail">
                  {formatSize(view.sent)} из {formatSize(view.size)} · {pct.toFixed(0)}%
                  {view.phase === 'uploading' && view.speed > 0 && ` · ${formatSpeed(view.speed)} · ${formatEta(left)}`}
                  {phaseText[view.phase] && ` · ${phaseText[view.phase]}`}
                  {view.phase === 'retrying' && view.retryInSec ? ` через ${view.retryInSec} с` : ''}
                  {view.error && `: ${codeText(view.error)}`}
                </div>
                {view.sha256 && <div class="detail sha">SHA-256 {view.sha256}</div>}
              </div>
            );
          })}
        </div>
      )}
      <div class="files-list">
        {dir !== '' && (
          <div class="entry" onClick={() => open(parentPath(dir))}>
            <span class="entry-icon">↩</span>
            <span class="entry-name">..</span>
          </div>
        )}
        {loading && entries.length === 0 && <div class="files-message">Загрузка…</div>}
        {!loading && entries.length === 0 && <div class="files-message">Пусто. Перетащите файлы сюда.</div>}
        {entries.map((e) => (
          <div class="entry" key={e.name}>
            <span class="entry-icon">{icon(e)}</span>
            {e.type === 'dir' ? (
              <button class="entry-name link" onClick={() => open(joinPath(dir, e.name))}>
                {displayName(e, dir)}
              </button>
            ) : e.type === 'file' ? (
              <a class="entry-name" href={downloadUrl(base, joinPath(dir, e.name))} download={e.name}>
                {e.name}
              </a>
            ) : (
              <span class="entry-name" title="Ссылки не открываются">
                {e.name}
              </span>
            )}
            <span class="entry-size">{e.type === 'file' ? formatSize(e.size) : ''}</span>
            <span class="entry-date">{formatDate(e.modified_at)}</span>
            {e.type !== 'link' && !(dir === '' && e.name === TRASH) && (
              <span class="entry-actions">
                {e.type === 'file' && (
                  <button onClick={() => void hashEntry(e)} title="SHA-256 на сервере">
                    #
                  </button>
                )}
                {!inTrash && (
                  <button onClick={() => renameEntry(e)} title="Переименовать">
                    ✎
                  </button>
                )}
                <button onClick={() => moveEntry(e)} title={inTrash ? 'Вернуть из корзины' : 'Переместить'}>
                  ⇄
                </button>
                {!inTrash && (
                  <button onClick={() => deleteEntry(e)} title="В корзину">
                    🗑
                  </button>
                )}
              </span>
            )}
            {hashes[e.name] && <div class="detail sha">SHA-256 {hashes[e.name]}</div>}
          </div>
        ))}
      </div>
    </div>
  );
}
