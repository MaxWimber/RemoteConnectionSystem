import { describe, expect, it } from 'vitest';
import { pairEvents } from './avsync';
import { withStereoOpus } from './audio';

const offer = [
  'v=0',
  'm=video 9 UDP/TLS/RTP/SAVPF 96 97',
  'a=rtpmap:96 H264/90000',
  'a=fmtp:96 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f',
  'm=audio 9 UDP/TLS/RTP/SAVPF 111 63',
  'a=rtpmap:111 opus/48000/2',
  'a=fmtp:111 minptime=10;useinbandfec=1',
  'a=rtpmap:63 red/48000/2',
  'a=fmtp:63 111/111',
  '',
].join('\r\n');

describe('withStereoOpus', () => {
  it('asks for stereo on the Opus payload type only', () => {
    const out = withStereoOpus(offer);
    expect(out).toContain('a=fmtp:111 minptime=10;useinbandfec=1;stereo=1;sprop-stereo=1\r\n');
    expect(out).toContain('a=fmtp:63 111/111\r\n');
    expect(out).toContain('a=fmtp:96 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n');
  });

  it('leaves an offer that already says stereo as it is', () => {
    const once = withStereoOpus(offer);
    expect(withStereoOpus(once)).toBe(once);
  });

  it('does nothing without Opus', () => {
    const video = 'v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\na=fmtp:96 packetization-mode=1\r\n';
    expect(withStereoOpus(video)).toBe(video);
  });
});

describe('pairEvents', () => {
  it('pairs each flash with the nearest click and says how late the sound is', () => {
    expect(pairEvents([1000, 2000, 3000], [1030, 1990, 3500])).toEqual([30, -10]);
  });

  it('ignores clicks far from any flash', () => {
    expect(pairEvents([1000], [2000])).toEqual([]);
  });
});
