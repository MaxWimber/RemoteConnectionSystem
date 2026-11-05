import { expect, test } from 'vitest';
import { ABS_CAPTURE_TIME, backoffMs, h264First, withAbsCaptureTime } from './session';
import { parseServerMessage, signalUrl } from './signaling';

test('reconnect waits grow and stop at five seconds', () => {
  expect([0, 1, 2, 3, 10].map(backoffMs)).toEqual([1000, 2000, 4000, 5000, 5000]);
});

test('only H.264 is kept, packetization-mode 1 first', () => {
  const codecs = [
    { mimeType: 'video/VP8', clockRate: 90000 },
    { mimeType: 'video/H264', clockRate: 90000, sdpFmtpLine: 'packetization-mode=0;profile-level-id=42e01f' },
    { mimeType: 'video/H264', clockRate: 90000, sdpFmtpLine: 'packetization-mode=1;profile-level-id=64001f' },
  ];
  const result = h264First(codecs);
  expect(result).toHaveLength(2);
  expect(result[0]?.sdpFmtpLine).toContain('packetization-mode=1');
});

test('signaling url follows the page scheme and the base path', () => {
  const page = { protocol: 'https:', host: 'example.test:8444' } as Location;
  expect(signalUrl(page, '/RemoteConnectionSystem/')).toBe(
    'wss://example.test:8444/RemoteConnectionSystem/api/signal',
  );
  const local = { protocol: 'http:', host: '127.0.0.1:8080' } as Location;
  expect(signalUrl(local, '/RemoteConnectionSystem')).toBe('ws://127.0.0.1:8080/RemoteConnectionSystem/api/signal');
});

test('server messages of another version or shape are errors', () => {
  expect(parseServerMessage('{"v":1,"type":"answer","sdp":"v=0"}')).toEqual({ v: 1, type: 'answer', sdp: 'v=0' });
  expect(parseServerMessage('{"v":2,"type":"answer","sdp":"v=0"}')).toMatchObject({ code: 'protocol_mismatch' });
  expect(parseServerMessage('nonsense')).toMatchObject({ code: 'bad_message' });
  expect(parseServerMessage('{"v":1,"type":"session-replaced"}').type).toBe('session-replaced');
});

test('abs-capture-time is switched from stopped to recvonly, nothing else changes', () => {
  const list = [
    { uri: 'urn:ietf:params:rtp-hdrext:sdes:mid', direction: 'sendrecv' as const },
    { uri: ABS_CAPTURE_TIME, direction: 'stopped' as const },
    { uri: 'http://www.webrtc.org/experiments/rtp-hdrext/video-timing', direction: 'stopped' as const },
  ];
  const out = withAbsCaptureTime(list);
  expect(out.map((e) => e.direction)).toEqual(['sendrecv', 'recvonly', 'stopped']);
});
