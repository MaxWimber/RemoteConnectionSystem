// The media session: a receive-only RTCPeerConnection with H.264, reconnecting when it breaks.

import { ClockSync, sampleFromPong } from './clock';
import { me } from '../api/auth';
import { InputChannels } from '../input/channels';
import { decodeServer, type ServerMessage as CursorMessage } from '../input/protocol';
import { watchNetwork } from './network';
import { CLOSE_SESSION_ENDED, Signaling, signalUrl, type ServerMessage } from './signaling';

export type SessionState = 'idle' | 'connecting' | 'playing' | 'reconnecting' | 'replaced' | 'stopped';

export interface SessionEvents {
  state(state: SessionState, detail?: string): void;
  log(message: string): void;
  stream(stream: MediaStream): void;
  /** The sign-in session ended (logout, expiry, a sign-in elsewhere): show the sign-in page. */
  signedOut(): void;
  /** The server's pointer: shape on control, position on input-fast. */
  cursor?(message: CursorMessage): void;
}

/** Wait before the n-th reconnect attempt: 1 s, 2 s, 4 s, then 5 s. */
export function backoffMs(attempt: number): number {
  return Math.min(5000, 1000 * 2 ** Math.max(0, attempt));
}

/** Keeps only H.264 and puts packetization-mode=1 first, which the server sends. */
export function h264First(codecs: RTCRtpCodec[]): RTCRtpCodec[] {
  const h264 = codecs.filter((c) => c.mimeType.toLowerCase() === 'video/h264');
  const mode1 = h264.filter((c) => /packetization-mode=1/.test(c.sdpFmtpLine ?? ''));
  const rest = h264.filter((c) => !mode1.includes(c));
  return [...mode1, ...rest];
}

export interface HeaderExtension {
  uri: string;
  direction: RTCRtpTransceiverDirection;
}

export const ABS_CAPTURE_TIME = 'http://www.webrtc.org/experiments/rtp-hdrext/abs-capture-time';

/** Turns on receiving abs-capture-time, which Chrome offers stopped; the rest stays as it is. */
export function withAbsCaptureTime<T extends HeaderExtension>(list: T[]): T[] {
  return list.map((e) => (e.uri === ABS_CAPTURE_TIME && e.direction === 'stopped' ? { ...e, direction: 'recvonly' } : e));
}

export class StreamSession {
  private pc: RTCPeerConnection | null = null;
  private signaling: Signaling | null = null;
  private attempt = 0;
  private timer: number | undefined;
  private stopped = true;
  private disconnectTimer: number | undefined;
  private pingTimer: number | undefined;
  private clockSync: ClockSync | null = null;
  private inputChannels: InputChannels | null = null;
  private unwatch: (() => void) | undefined;

  constructor(
    private readonly base: string,
    private readonly events: SessionEvents,
  ) {}

  get peer(): RTCPeerConnection | null {
    return this.pc;
  }

  /** The input channels of the current connection; they send nothing until open. */
  get input(): InputChannels | null {
    return this.inputChannels;
  }

  /** The server clock of the current session, once the control channel has answered. */
  get clock(): ClockSync | null {
    return this.clockSync;
  }

  start(): void {
    this.stopped = false;
    this.attempt = 0;
    this.unwatch?.();
    this.unwatch = watchNetwork((m) => this.events.log(m));
    void this.connect();
  }

  stop(): void {
    this.stopped = true;
    this.finish();
    this.events.state('stopped');
  }

  /** Stops for good: no media, no reconnects, no network watching. */
  private finish(): void {
    this.teardown();
    this.unwatch?.();
    this.unwatch = undefined;
  }

  private teardown(): void {
    window.clearTimeout(this.timer);
    window.clearTimeout(this.disconnectTimer);
    window.clearTimeout(this.pingTimer);
    this.clockSync = null;
    this.inputChannels = null;
    this.signaling?.close();
    this.signaling = null;
    this.pc?.close();
    this.pc = null;
  }

  private endSignedOut(): void {
    if (this.stopped) return;
    this.stopped = true;
    this.finish();
    this.events.log('сессия входа завершена');
    this.events.signedOut();
  }

  private async sessionGone(): Promise<boolean> {
    try {
      if ((await me(this.base)).state !== 'signed-in') {
        this.endSignedOut();
        return true;
      }
    } catch {
      // The server is not answering; keep trying to reconnect.
    }
    return false;
  }

  private scheduleReconnect(reason: string): void {
    if (this.stopped) return;
    this.teardown();
    const wait = backoffMs(this.attempt++);
    this.events.state('reconnecting', reason);
    this.events.log(`переподключение через ${wait} мс: ${reason}`);
    this.timer = window.setTimeout(() => void this.connect(), wait);
  }

  private async connect(): Promise<void> {
    this.teardown();
    this.events.state(this.attempt === 0 ? 'connecting' : 'reconnecting');
    const url = signalUrl(window.location, this.base);
    const signaling = new Signaling(
      url,
      (m) => void this.onSignal(m),
      (reason, code) => (code === CLOSE_SESSION_ENDED ? this.endSignedOut() : this.scheduleReconnect(reason)),
    );
    this.signaling = signaling;
    try {
      await signaling.open();
    } catch (e) {
      // A refused WebSocket looks the same for a dead server and for an ended session; ask which one.
      if (await this.sessionGone()) return;
      this.scheduleReconnect(String(e));
      return;
    }
    const pc = new RTCPeerConnection({ bundlePolicy: 'max-bundle' });
    this.pc = pc;
    const transceiver = pc.addTransceiver('video', { direction: 'recvonly' });
    // The capture time of each frame gives the browser capture -> display latency (requestVideoFrameCallback).
    const negotiable = transceiver as RTCRtpTransceiver & {
      getHeaderExtensionsToNegotiate?: () => HeaderExtension[];
      setHeaderExtensionsToNegotiate?: (list: HeaderExtension[]) => void;
    };
    if (negotiable.getHeaderExtensionsToNegotiate && negotiable.setHeaderExtensionsToNegotiate) {
      try {
        negotiable.setHeaderExtensionsToNegotiate(withAbsCaptureTime(negotiable.getHeaderExtensionsToNegotiate()));
      } catch (e) {
        this.events.log(`abs-capture-time: ${String(e)}`);
      }
    }
    const caps = RTCRtpReceiver.getCapabilities('video');
    if (caps && typeof transceiver.setCodecPreferences === 'function') {
      const codecs = h264First(caps.codecs);
      if (codecs.length > 0) transceiver.setCodecPreferences(codecs);
    }
    // The control channel carries the server clock now (PROTOCOL.md) and settings and input from phase 4.
    this.openControl(pc.createDataChannel('control', { ordered: true }));
    this.inputChannels = new InputChannels(pc, (m) => this.events.cursor?.(m));
    pc.ontrack = (e) => {
      const receiver = e.receiver as RTCRtpReceiver & {
        jitterBufferTarget?: number | null;
        playoutDelayHint?: number;
      };
      // Show frames as soon as they are decoded (P2-7).
      if ('jitterBufferTarget' in receiver) receiver.jitterBufferTarget = 0;
      if ('playoutDelayHint' in receiver) receiver.playoutDelayHint = 0;
      this.events.stream(e.streams[0] ?? new MediaStream([e.track]));
    };
    pc.onconnectionstatechange = () => {
      const s = pc.connectionState;
      this.events.log(`connectionState: ${s}`);
      if (s === 'connected') {
        this.attempt = 0;
        window.clearTimeout(this.disconnectTimer);
        this.events.state('playing');
      } else if (s === 'failed') {
        this.scheduleReconnect('соединение не удалось');
      } else if (s === 'disconnected') {
        // Short gaps recover by themselves; a long one means the path is gone.
        window.clearTimeout(this.disconnectTimer);
        this.disconnectTimer = window.setTimeout(() => {
          if (this.pc === pc && pc.connectionState !== 'connected') {
            this.scheduleReconnect('соединение пропало');
          }
        }, 3000);
      }
    };
    const offer = await pc.createOffer();
    await pc.setLocalDescription(offer);
    // The server is ICE-lite and learns our address from the checks, so the offer goes without candidates.
    signaling.send({ type: 'offer', sdp: pc.localDescription?.sdp ?? offer.sdp });
    this.events.log('offer отправлен');
  }

  /** Ten quick pings right away, then one every five seconds; the fastest exchange sets the clock offset. */
  private openControl(channel: RTCDataChannel): void {
    const clock = new ClockSync();
    this.clockSync = clock;
    const sent = new Map<number, number>();
    let id = 0;
    const ping = () => {
      if (channel.readyState !== 'open') return;
      id += 1;
      const now = performance.now();
      sent.set(id, now);
      channel.send(JSON.stringify({ t: 'ping', id, client_ts: now }));
      this.pingTimer = window.setTimeout(ping, id < 10 ? 200 : 5000);
    };
    channel.binaryType = 'arraybuffer';
    channel.onopen = ping;
    channel.onmessage = (e) => {
      if (e.data instanceof ArrayBuffer) {
        const m = decodeServer(new Uint8Array(e.data));
        if (!('error' in m)) this.events.cursor?.(m);
        return;
      }
      if (typeof e.data !== 'string') return;
      let m: Record<string, unknown>;
      try {
        m = JSON.parse(e.data) as Record<string, unknown>;
      } catch {
        return;
      }
      if (m['t'] === 'hello' && typeof m['rtp_epoch_ms'] === 'number') {
        clock.rtpEpochMs = m['rtp_epoch_ms'];
        if (typeof m['rtp_clock'] === 'number') clock.rtpClock = m['rtp_clock'];
      } else if (m['t'] === 'pong' && typeof m['id'] === 'number' && typeof m['server_ms'] === 'number') {
        const at = sent.get(m['id']);
        sent.delete(m['id']);
        if (at !== undefined) clock.add(sampleFromPong(at, performance.now(), m['server_ms'], performance.timeOrigin));
      }
    };
  }
  private async onSignal(m: ServerMessage): Promise<void> {
    const pc = this.pc;
    switch (m.type) {
      case 'answer':
        if (!pc) return;
        try {
          await pc.setRemoteDescription({ type: 'answer', sdp: m.sdp });
          this.events.log('answer получен');
        } catch (e) {
          this.scheduleReconnect(`answer не принят: ${String(e)}`);
        }
        return;
      case 'session-replaced':
        this.stopped = true;
        this.finish();
        this.events.state('replaced');
        this.events.log('сеанс занят другим окном');
        return;
      case 'error':
        this.events.log(`ошибка сервера: ${m.code}`);
        if (m.code === 'session_ended') {
          this.endSignedOut();
          return;
        }
        this.scheduleReconnect(`ошибка сервера: ${m.code}`);
        return;
      case 'pong':
        return;
    }
  }
}
