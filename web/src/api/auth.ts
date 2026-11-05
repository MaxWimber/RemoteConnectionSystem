// Sign-in API from docs/PROTOCOL.md (Авторизация). The session lives in an HttpOnly cookie the page never sees.

export type AuthResult =
  | { ok: true; next?: 'totp'; expiresAt?: string }
  | { ok: false; error: string; retryAfterSec?: number };

export type MeResult = { state: 'signed-in'; expiresAt: string } | { state: 'totp' } | { state: 'signed-out' };

function api(base: string, path: string): string {
  const prefix = base.endsWith('/') ? base : `${base}/`;
  return `${prefix}api/auth/${path}`;
}

/** State-changing requests carry `X-RCS-Request`; the server also checks `Origin`. */
async function post(base: string, path: string, body?: unknown): Promise<Response> {
  return fetch(api(base, path), {
    method: 'POST',
    credentials: 'same-origin',
    cache: 'no-store',
    headers: { 'Content-Type': 'application/json', 'X-RCS-Request': '1' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
}

async function json(r: Response): Promise<Record<string, unknown>> {
  try {
    return (await r.json()) as Record<string, unknown>;
  } catch {
    return {};
  }
}

export async function toResult(r: Response): Promise<AuthResult> {
  const body = await json(r);
  if (r.ok) {
    return {
      ok: true,
      next: body['next'] === 'totp' ? 'totp' : undefined,
      expiresAt: typeof body['expires_at'] === 'string' ? body['expires_at'] : undefined,
    };
  }
  const retry = Number(r.headers.get('Retry-After'));
  return {
    ok: false,
    error: typeof body['error'] === 'string' ? body['error'] : `http_${r.status}`,
    retryAfterSec: Number.isFinite(retry) && retry > 0 ? retry : undefined,
  };
}

export async function me(base: string): Promise<MeResult> {
  const r = await fetch(api(base, 'me'), { credentials: 'same-origin', cache: 'no-store' });
  const body = await json(r);
  if (r.ok && body['authenticated'] === true) return { state: 'signed-in', expiresAt: String(body['expires_at'] ?? '') };
  if (r.status === 401 && body['next'] === 'totp') return { state: 'totp' };
  if (r.status === 401) return { state: 'signed-out' };
  throw new Error(`auth/me: HTTP ${r.status}`);
}

export const login = async (base: string, password: string) => toResult(await post(base, 'login', { password }));
export const totp = async (base: string, code: string) => toResult(await post(base, 'totp', { code }));
export const recovery = async (base: string, code: string) => toResult(await post(base, 'recovery', { code }));

export async function logout(base: string): Promise<void> {
  await post(base, 'logout');
}

/** A message for the person at the keyboard; the server never says which part was wrong. */
export function errorText(result: AuthResult): string {
  if (result.ok) return '';
  switch (result.error) {
    case 'invalid_credentials':
      return 'Неверный пароль или код.';
    case 'too_many_attempts': {
      const min = Math.max(1, Math.ceil((result.retryAfterSec ?? 900) / 60));
      return `Слишком много попыток. Повторите через ${min} мин.`;
    }
    case 'unauthorized':
      return 'Время на ввод кода истекло. Введите пароль снова.';
    case 'forbidden':
      return 'Запрос отклонён: откройте страницу по её основному адресу.';
    default:
      return `Ошибка сервера (${result.error}).`;
  }
}
