import { expect, test } from 'vitest';
import {
  loadBitrateLimit,
  loadMode,
  mbps,
  modeLabel,
  parseServerStats,
  saveBitrateLimit,
  saveMode,
  settingsMessage,
} from './server-stats';

test('a stats message is read, missing reports stay null', () => {
  const s = parseServerStats({
    t: 'stats',
    adaptive: true,
    target_kbps: 32400,
    estimate_kbps: null,
    sent_kbps: 6100,
    out_kbps: 6400,
    audio_kbps: 131,
    loss: null,
    rtt_ms: 1.4,
    min_kbps: 2000,
    max_kbps: 50000,
    rate_changes: 3,
    mode: 'game',
    active_mode: 'weak',
  });
  expect(s).toEqual({
    adaptive: true,
    targetKbps: 32400,
    estimateKbps: null,
    sentKbps: 6100,
    outKbps: 6400,
    audioKbps: 131,
    loss: null,
    rttMs: 1.4,
    minKbps: 2000,
    maxKbps: 50000,
    rateChanges: 3,
    mode: 'game',
    activeMode: 'weak',
  });
});

test('modes are read and kept, anything unknown is «Игра»', () => {
  const store = new Map<string, string>();
  const storage = {
    getItem: (k: string) => store.get(k) ?? null,
    setItem: (k: string, v: string) => void store.set(k, v),
  };
  expect(loadMode(storage)).toBe('game');
  saveMode('weak', storage);
  expect(loadMode(storage)).toBe('weak');
  store.set('rcs.streamMode', '4k');
  expect(loadMode(storage)).toBe('game');
  expect(modeLabel('desktop')).toBe('Рабочий стол');
  const old = parseServerStats({ t: 'stats', target_kbps: 1, sent_kbps: 1, out_kbps: 1, min_kbps: 1, max_kbps: 2 });
  expect(old?.activeMode).toBe('game');
});

test('other or broken messages are not stats', () => {
  expect(parseServerStats({ t: 'pong', id: 1 })).toBeNull();
  expect(parseServerStats({ t: 'stats', target_kbps: 'x' })).toBeNull();
});

test('the limit is kept only for known choices and survives a refusing storage', () => {
  const store = new Map<string, string>();
  const storage = {
    getItem: (k: string) => store.get(k) ?? null,
    setItem: (k: string, v: string) => void store.set(k, v),
    removeItem: (k: string) => void store.delete(k),
  };
  expect(loadBitrateLimit(storage)).toBeNull();
  saveBitrateLimit(10000, storage);
  expect(loadBitrateLimit(storage)).toBe(10000);
  store.set('rcs.maxBitrateKbps', '12345');
  expect(loadBitrateLimit(storage)).toBeNull();
  saveBitrateLimit(null, storage);
  expect(store.size).toBe(0);
  const refusing = {
    getItem: () => {
      throw new Error('denied');
    },
    setItem: () => {
      throw new Error('denied');
    },
    removeItem: () => {
      throw new Error('denied');
    },
  };
  expect(loadBitrateLimit(refusing)).toBeNull();
  expect(() => saveBitrateLimit(5000, refusing)).not.toThrow();
});

test('settings and numbers for the panel', () => {
  expect(JSON.parse(settingsMessage(20000))).toEqual({ t: 'settings', max_bitrate_kbps: 20000 });
  expect(JSON.parse(settingsMessage(null))).toEqual({ t: 'settings', max_bitrate_kbps: null });
  expect(JSON.parse(settingsMessage(null, 'desktop'))).toEqual({
    t: 'settings',
    max_bitrate_kbps: null,
    mode: 'desktop',
  });
  expect(mbps(32400)).toBe('32,4');
  expect(mbps(null)).toBe('—');
});
