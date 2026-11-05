import { expect, test } from 'vitest';
import { errorText, toResult } from './auth';

const reply = (status: number, body: unknown, headers: Record<string, string> = {}) =>
  new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json', ...headers } });

test('the password step asks for TOTP next', async () => {
  expect(await toResult(reply(200, { next: 'totp' }))).toEqual({ ok: true, next: 'totp', expiresAt: undefined });
});

test('a block says how long to wait, rounded up to minutes', async () => {
  const r = await toResult(reply(429, { error: 'too_many_attempts' }, { 'Retry-After': '61' }));
  expect(r).toEqual({ ok: false, error: 'too_many_attempts', retryAfterSec: 61 });
  expect(errorText(r)).toContain('2 мин');
});

test('a wrong password and a wrong code read the same', async () => {
  const r = await toResult(reply(401, { error: 'invalid_credentials' }));
  expect(errorText(r)).toBe('Неверный пароль или код.');
});

test('a body that is not JSON still gives an error code', async () => {
  const r = await toResult(new Response('oops', { status: 502 }));
  expect(r).toEqual({ ok: false, error: 'http_502', retryAfterSec: undefined });
});
