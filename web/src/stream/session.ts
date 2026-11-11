// The media session: a receive-only RTCPeerConnection with H.264 and Opus, reconnecting when it breaks.

import { ClockSync, sampleFromPong } from './clock';
import { me } from '../api/auth';
import { withStereoOpus } from './audio';
import { isWtOffer, type WtOffer } from './webtransport';
import { ClipboardSync, loadClipboardSharing, saveClipboardSharing } from '../clipboard/sync';
import { InputChannels } from '../input/channels';
import { decodeServer, type ServerMessage as CursorMessage } from '../input/protocol';
import { watchNetwork } from './network';
import {
  loadBitrateLimit,
  loadMode,
  parseServerStats,
  saveBitrateLimit,
  saveMode,
  settingsMessage,
  type ServerStats,
  type StreamMode,
} from './server-stats';
import { CLOSE_SESSION_ENDED, Signaling, signalUrl, type ServerMessage } from './signaling';

export type SessionState = 'idle' | 'connecting' | 'playing' | 'reconnecting' | 'replaced' | 'stopped';

export interface SessionEvents {
  state(state: SessionState, detail?: string): void;
  log(message: string): void;
  stream(stream: MediaStream): void;
  /** The sound of the server (D-038), a track of its own. */
  audio?(track: MediaStreamTrack, receiver: RTCRtpReceiver): void;
  /** The sign-in session ended (logout, expiry, a sign-in elsewhere): show the sign-in page. */
  signedOut(): void;
  /** The server's pointer: shape on control, position on input-fast. */
  cursor?(message: CursorMessage): void;
  /** The agent's rate control, once a second while video plays (D-029). */
  serverStats?(stats: ServerStats): void;
  /** The server offers the experimental WebTransport video path in `hello` (P9-4). */
  webtransport?(offer: WtOffer): void;
  /** Something to show the user for a while; empty clears it (D-040). */
  notice?(text: string): void;
  /** The bitrate limit the server applied, kbit/s. */
  settingsApplied?(maxKbps: number, adaptive: boolean, mode: string): void;
}

/** Wait before the n-th reconnect attempt: 1 s, 2 s, 4 s, then 5 s. */
export function backoffMs(attempt: number): number {
  return Math.min(5000, 1000 * 2 ** Math.max(0, attempt));
}

/**
 * Keeps H.264 with packetization-mode=1 first, which the server sends, and RTX: without it the server cannot resend a
 * lost packet, and with steady loss nearly every frame stays incomplete (found in P9-4, D-044).
 */
export function h264First(codecs: RTCRtpCodec[]): RTCRtpCodec[] {
  const h264 = codecs.filter((c) => c.mimeType.toLowerCase() === 'video/h264');
  const mode1 = h264.filter((c) => /packetization-mode=1/.test(c.sdpFmtpLine ?? ''));
  const rest = h264.filter((c) => !mode1.includes(c));
  const rtx = codecs.filter((c) => c.mimeType.toLowerCase() === 'video/rtx');
  return [...mode1, ...rest, ...rtx];
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
  private control: RTCDataChannel | null = null;
  private resumeTimer: number | undefined;
  private resumeAttempt = 0;
  private limitKbps: number | null = loadBitrateLimit();
  private modeChoice: StreamMode = loadMode();
  private audioRx: RTCRtpReceiver | null = null;
  private clipboardOn = loadClipboardSharing();
  private clipboardSync: ClipboardSync | null = null;

  constructor(
    private readonly base: string,
    private readonly events: SessionEvents,
  ) {}

  get peer(): RTCPeerConnection | null {
    return this.pc;
  }

  /** The shared clipboard of the current connection, while sharing is on and the channel is open (D-040). */
  get clipboard(): ClipboardSync | null {
    return this.clipboardSync?.open ? this.clipboardSync : null;
  }

  get clipboardSharing(): boolean {
    return this.clipboardOn;
  }

  /** Turns the shared clipboard on or off for this browser; the channel opens or closes at once. */
  setClipboardSharing(on: boolean): void {
    this.clipboardOn = on;
    saveClipboardSharing(on);
    if (!on) {
      this.clipboardSync?.close();
      this.clipboardSync = null;
    } else if (this.pc && !this.clipboardSync) {
      this.openClipboard(this.pc);
    }
    this.events.log(`общий буфер обмена ${on ? 'включён' : 'выключен'}`);
  }

  private openClipboard(pc: RTCPeerConnection): void {
    const channel = pc.createDataChannel('clipboard', { ordered: true });
    this.clipboardSync = new ClipboardSync(channel, {
      notice: (text) => this.events.notice?.(text),
      log: (m) => this.events.log(m),
    });
  }

  /** Moves the video of this connection to WebTransport or back to WebRTC (P9-4). */
  setVideoPath(path: 'webrtc' | 'webtransport'): void {
    if (this.control?.readyState === 'open') this.control.send(JSON.stringify({ t: 'transport', video: path }));
  }

  /** The WebTransport decoder needs an IDR (P9-4). */
  requestKeyframe(): void {
    if (this.control?.readyState === 'open') this.control.send(JSON.stringify({ t: 'keyframe' }));
  }

  /** The receiver of the sound, for its delay. */
  get audioReceiver(): RTCRtpReceiver | null {
    return this.audioRx;
  }

  /** The input channels of the current connection; they send nothing until open. */
  get input(): InputChannels | null {
    return this.inputChannels;
  }

  /** The bitrate limit chosen on this browser, kbit/s, or null for the server's. */
  get bitrateLimit(): number | null {
    return this.limitKbps;
  }

  /** Remembers the limit and sends it now if the control channel is open, else when it opens. */
  setBitrateLimit(kbps: number | null): void {
    this.limitKbps = kbps;
    saveBitrateLimit(kbps);
    if (this.control?.readyState === 'open') this.control.send(settingsMessage(kbps));
  }

  /** The stream mode chosen on this browser (D-030). */
  get mode(): StreamMode {
    return this.modeChoice;
  }

  setMode(mode: StreamMode): void {
    this.modeChoice = mode;
    saveMode(mode);
    if (this.control?.readyState === 'open') this.control.send(settingsMessage(this.limitKbps, mode));
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
    window.clearTimeout(this.resumeTimer);
    this.resumeAttempt = 0;
    this.clockSync = null;
    this.inputChannels = null;
    this.control = null;
    this.audioRx = null;
    this.clipboardSync = null;
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

  /**
   * The signaling socket closed. While the media is still connected only the socket comes back and asks the server
   * to keep the media session (D-031); otherwise everything starts again.
   */
  private signalingLost(reason: string, code: number): void {
    if (code === CLOSE_SESSION_ENDED) {
      this.endSignedOut();
      return;
    }
    if (this.stopped) return;
    if (this.pc?.connectionState !== 'connected') {
      this.scheduleReconnect(reason);
      return;
    }
    this.signaling = null;
    const wait = backoffMs(this.resumeAttempt++);
    this.events.log(`сигналинг оборвался (${reason}), видео идёт; новый сокет через ${wait} мс`);
    window.clearTimeout(this.resumeTimer);
    this.resumeTimer = window.setTimeout(() => void this.resumeSignaling(), wait);
  }

  private async resumeSignaling(): Promise<void> {
    if (this.stopped || !this.pc) return;
    if (this.pc.connectionState !== 'connected') {
      this.scheduleReconnect('медиасоединение тоже потеряно');
      return;
    }
    const signaling = new Signaling(
      signalUrl(window.location, this.base),
      (m) => void this.onSignal(m),
      (reason, code) => this.signalingLost(reason, code),
    );
    this.signaling = signaling;
    try {
      await signaling.open();
    } catch (e) {
      if (this.signaling !== signaling) return;
      if (await this.sessionGone()) return;
      this.signalingLost(String(e), 0);
      return;
    }
    if (this.signaling !== signaling) return;
    signaling.send({ type: 'resume' });
  }

  private async connect(): Promise<void> {
    this.teardown();
    this.events.state(this.attempt === 0 ? 'connecting' : 'reconnecting');
    const url = signalUrl(window.location, this.base);
    const signaling = new Signaling(
      url,
      (m) => void this.onSignal(m),
      (reason, code) => this.signalingLost(reason, code),
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
    // The sound comes on a track of its own, in a stream of its own: no lip sync that would delay the video.
    const sound = pc.addTransceiver('audio', { direction: 'recvonly' });
    // The capture time of each frame gives the browser capture -> display latency (requestVideoFrameCallback).
    for (const t of [transceiver, sound]) {
      const negotiable = t as RTCRtpTransceiver & {
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
    }
    const caps = RTCRtpReceiver.getCapabilities('video');
    if (caps && typeof transceiver.setCodecPreferences === 'function') {
      const codecs = h264First(caps.codecs);
      if (codecs.length > 0) transceiver.setCodecPreferences(codecs);
    }
    // The control channel carries the server clock now (PROTOCOL.md) and settings and input from phase 4.
    this.openControl(pc.createDataChannel('control', { ordered: true }));
    this.inputChannels = new InputChannels(pc, (m) => this.events.cursor?.(m));
    if (this.clipboardOn) this.openClipboard(pc);
    pc.ontrack = (e) => {
      const receiver = e.receiver as RTCRtpReceiver & {
        jitterBufferTarget?: number | null;
        playoutDelayHint?: number;
      };
      // Show frames and play sound as soon as they are decoded (P2-7, D-038).
      if ('jitterBufferTarget' in receiver) receiver.jitterBufferTarget = 0;
      if ('playoutDelayHint' in receiver) receiver.playoutDelayHint = 0;
      if (e.track.kind === 'audio') {
        this.audioRx = e.receiver;
        this.events.audio?.(e.track, e.receiver);
        return;
      }
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
    const created = await pc.createOffer();
    const offer = { type: created.type, sdp: withStereoOpus(created.sdp ?? '') };
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
    this.control = channel;
    channel.onopen = () => {
      // The server keeps the limit and the mode only for the connection: send them every time (D-029, D-030).
      channel.send(settingsMessage(this.limitKbps, this.modeChoice));
      ping();
    };
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
      if (m['t'] === 'stats') {
        const stats = parseServerStats(m);
        if (stats) this.events.serverStats?.(stats);
      } else if (m['t'] === 'settings' && typeof m['max_bitrate_kbps'] === 'number') {
        this.events.settingsApplied?.(m['max_bitrate_kbps'], m['adaptive'] === true, String(m['mode'] ?? ''));
      } else if (m['t'] === 'hello' && typeof m['rtp_epoch_ms'] === 'number') {
        clock.rtpEpochMs = m['rtp_epoch_ms'];
        if (typeof m['rtp_clock'] === 'number') clock.rtpClock = m['rtp_clock'];
        if (isWtOffer(m['webtransport'])) this.events.webtransport?.(m['webtransport']);
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
      case 'resumed':
        this.resumeAttempt = 0;
        this.events.log('сигналинг восстановлен, сеанс продолжен');
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
