// Sound of the server (D-038): one Opus track, played by an <audio> element apart from the video, so the browser
// never holds video back to line it up with the sound.

import { captureTimeFromRtp, type ClockSync } from './clock';

const SOUND_KEY = 'rcs.sound';

/** Whether sound is on for this browser; on unless the user turned it off. */
export function loadSound(): boolean {
  try {
    return localStorage.getItem(SOUND_KEY) !== 'off';
  } catch {
    return true;
  }
}

export function saveSound(on: boolean): void {
  try {
    localStorage.setItem(SOUND_KEY, on ? 'on' : 'off');
  } catch {
    // Private mode: the choice lasts until the page closes.
  }
}

/**
 * Asks for stereo Opus in the offer: Chrome decodes mono unless its own fmtp says stereo=1. Only the Opus payload
 * types are touched.
 */
export function withStereoOpus(sdp: string): string {
  const opus = new Set<string>();
  for (const m of sdp.matchAll(/^a=rtpmap:(\d+) opus\/48000\/2\r?$/gim)) opus.add(m[1]!);
  return sdp.replace(/^a=fmtp:(\d+) ([^\r\n]*)$/gm, (line, pt: string, params: string) => {
    if (!opus.has(pt)) return line;
    let p = params;
    if (!/(^|;)\s*stereo=/.test(p)) p += ';stereo=1';
    if (!/(^|;)\s*sprop-stereo=/.test(p)) p += ';sprop-stereo=1';
    return `a=fmtp:${pt} ${p}`;
  });
}

/** off: the user turned sound off; waiting: on, but the browser needs a tap or a click first; playing. */
export type SoundState = 'off' | 'waiting' | 'playing';

/** The <audio> element of the server's sound and the browser's autoplay rule. */
export class SoundPlayer {
  private readonly element: HTMLAudioElement;
  private enabled: boolean;
  private current: SoundState;
  private hasTrack = false;
  /** While a measurement runs on the server itself, nothing may come out of its speakers: it would echo. */
  private silenced = false;

  constructor(private readonly changed: (state: SoundState) => void) {
    this.element = document.createElement('audio');
    this.element.autoplay = true;
    this.enabled = loadSound();
    this.current = this.enabled ? 'waiting' : 'off';
    this.element.muted = !this.enabled;
    const unlock = () => {
      if (this.current === 'waiting') void this.play();
    };
    // The first tap, click or key of the page counts as the gesture the browser waits for.
    for (const type of ['pointerdown', 'keydown', 'touchend']) {
      document.addEventListener(type, unlock, { capture: true, passive: true });
    }
  }

  get state(): SoundState {
    return this.current;
  }

  get on(): boolean {
    return this.enabled;
  }

  /** The audio track of a new connection. */
  attach(track: MediaStreamTrack): void {
    this.element.srcObject = new MediaStream([track]);
    this.hasTrack = true;
    if (this.enabled) void this.play();
  }

  /** The track, for a measurement that listens to it without playing it. */
  get stream(): MediaStream | null {
    return this.element.srcObject instanceof MediaStream ? this.element.srcObject : null;
  }

  setEnabled(on: boolean): void {
    this.enabled = on;
    saveSound(on);
    this.element.muted = !on || this.silenced;
    if (!on) {
      this.set('off');
      return;
    }
    // A click on the sound button is itself the gesture.
    void this.play();
  }

  /** Mutes the element without changing the user's choice. Chrome still needs the element for WebAudio to hear. */
  silence(on: boolean): void {
    this.silenced = on;
    this.element.muted = on || !this.enabled;
    if (on) void this.element.play().catch(() => undefined);
  }

  private async play(): Promise<void> {
    if (!this.hasTrack) {
      this.set(this.enabled ? 'waiting' : 'off');
      return;
    }
    try {
      await this.element.play();
      if (this.enabled) this.set('playing');
    } catch {
      if (this.enabled) this.set('waiting');
    }
  }

  private set(state: SoundState): void {
    if (state === this.current) return;
    this.current = state;
    this.changed(state);
  }
}

type SyncSource = RTCRtpSynchronizationSource & { captureTimestamp?: number };

/**
 * Capture -> playout of the sound in ms, from the newest packet the receiver has played: its RTP time gives the
 * capture time on the server clock (`hello`: one epoch for video and sound, 48 kHz for sound).
 */
export function audioDelayMs(
  receiver: RTCRtpReceiver,
  clock: ClockSync | null,
  timeOrigin = performance.timeOrigin,
): number | null {
  const best = clock?.best();
  if (!clock || clock.rtpEpochMs === null || !best) return null;
  const source = receiver.getSynchronizationSources()[0] as SyncSource | undefined;
  if (!source) return null;
  const played = source.timestamp - timeOrigin;
  // No packets in silence: the newest one says nothing about the delay now.
  if (performance.now() - played > 200) return null;
  const captured = captureTimeFromRtp(source.rtpTimestamp, clock.rtpEpochMs, 48000, best.offsetMs, timeOrigin, played);
  const delay = played - captured;
  return Number.isFinite(delay) && delay > -50 && delay < 5000 ? delay : null;
}
