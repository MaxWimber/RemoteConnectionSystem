// File API from docs/PROTOCOL.md («Файлы»). Paths are relative to files.root and use '/'.

export interface Entry {
  name: string;
  type: 'file' | 'dir' | 'link';
  size: number;
  modified_at: string;
}

export class ApiError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    readonly body: Record<string, unknown> = {},
  ) {
    super(code);
  }
}

export function apiUrl(base: string, path: string): string {
  const prefix = base.endsWith('/') ? base : `${base}/`;
  return `${prefix}api/${path}`;
}

export function joinPath(dir: string, name: string): string {
  return dir ? `${dir}/${name}` : name;
}

export function parentPath(path: string): string {
  const i = path.lastIndexOf('/');
  return i < 0 ? '' : path.slice(0, i);
}

async function body(r: Response): Promise<Record<string, unknown>> {
  try {
    return (await r.json()) as Record<string, unknown>;
  } catch {
    return {};
  }
}

export async function check(r: Response): Promise<Record<string, unknown>> {
  const b = await body(r);
  if (!r.ok) throw new ApiError(r.status, typeof b['error'] === 'string' ? b['error'] : `http_${r.status}`, b);
  return b;
}

export function getJson(base: string, path: string, signal?: AbortSignal): Promise<Response> {
  return fetch(apiUrl(base, path), { credentials: 'same-origin', cache: 'no-store', signal });
}

/** State-changing requests carry `X-RCS-Request`; the server also checks `Origin`. */
export function send(
  base: string,
  path: string,
  method: string,
  payload?: unknown,
  signal?: AbortSignal,
): Promise<Response> {
  return fetch(apiUrl(base, path), {
    method,
    credentials: 'same-origin',
    cache: 'no-store',
    headers: { 'Content-Type': 'application/json', 'X-RCS-Request': '1' },
    body: payload === undefined ? undefined : JSON.stringify(payload),
    signal,
  });
}

const q = (path: string) => `?path=${encodeURIComponent(path)}`;

export async function list(base: string, path: string): Promise<Entry[]> {
  const b = await check(await getJson(base, `files/list${q(path)}`));
  return (b['entries'] as Entry[]) ?? [];
}

export const downloadUrl = (base: string, path: string) => apiUrl(base, `files/download${q(path)}`);

export async function mkdir(base: string, path: string): Promise<void> {
  await check(await send(base, 'files/mkdir', 'POST', { path }));
}

export async function rename(base: string, from: string, to: string): Promise<void> {
  await check(await send(base, 'files/rename', 'POST', { from, to }));
}

/** Moves to `.rcs-trash`; returns where it went. */
export async function remove(base: string, path: string): Promise<string> {
  const b = await check(await send(base, 'files/delete', 'POST', { path }));
  return String(b['trash'] ?? '');
}

export async function hash(base: string, path: string): Promise<string> {
  const b = await check(await getJson(base, `files/hash${q(path)}`));
  return String(b['sha256'] ?? '');
}

/** A message for the person; codes come from the server. */
export function errorText(e: unknown): string {
  return e instanceof ApiError ? codeText(e.code) : `Сеть: ${String(e)}`;
}

export function codeText(code: string): string {
  switch (code) {
    case 'busy':
      return 'Этот файл уже загружается: в другой строке или вкладке.';
    case 'another_writer':
      return 'Ту же загрузку пишет другая вкладка, эта остановлена.';
    case 'exists':
      return 'Такое имя уже есть.';
    case 'not_found':
      return 'Не найдено: возможно, уже удалено или перемещено.';
    case 'forbidden':
      return 'Так нельзя: корень, корзина или перенос в себя.';
    case 'link':
      return 'Это ссылка или соединение: открывать их запрещено.';
    case 'path_mismatch':
      return 'Путь не совпал с настоящим (короткое имя или подмена).';
    case 'access_denied':
      return 'Нет доступа у службы rcs-web.';
    case 'disk_full':
      return 'На диске не хватает места.';
    case 'too_large':
      return 'Файл больше разрешённого предела.';
    case 'files_unavailable':
      return 'Файловый API выключен: папка files.root не прошла проверку.';
    case 'not_a_directory':
      return 'Это не папка.';
    case 'is_a_directory':
      return 'Это папка.';
    case 'unauthorized':
      return 'Сессия закончилась, войдите снова.';
    default:
      return code.startsWith('path_') ? `Недопустимое имя (${code}).` : `Ошибка (${code}).`;
  }
}
