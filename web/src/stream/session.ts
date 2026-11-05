// The media session: a receive-only RTCPeerConnection with H.264, reconnecting when it breaks.

import { Signaling, signalUrl, type ServerMessage } from './signaling';

export type SessionState = 'idle' | 'connecting' | 'playing' | 'reconnecting' | 'replaced' | 'stopped';

export interface SessionEvents {
  state(state: SessionState, detail?: string): void;
  log(message: string): void;
  stream(stream: MediaStream): void;
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

export class StreamSession {
  private pc: RTCPeerConnection | null = null;
  private signaling: Signaling | null = null;
  private attempt = 0;
  private timer: number | undefined;
  private stopped = true;
  private disconnectTimer: number | undefined;

  constructor(
    private readonly base: string,
    private readonly events: SessionEvents,
  ) {}

  get peer(): RTCPeerConnection | null {
    return this.pc;
  }

  start(): void {
    this.stopped = false;
    this.attempt = 0;
    void this.connect();
  }

  stop(): void {
    this.stopped = true;
    this.teardown();
    this.events.state('stopped');
  }

  private teardown(): void {
    window.clearTimeout(this.timer);
    window.clearTimeout(this.disconnectTimer);
    this.signaling?.close();
    this.signaling = null;
    this.pc?.close();
    this.pc = null;
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
      (reason) => this.scheduleReconnect(reason),
    );
    this.signaling = signaling;
    try {
      await signaling.open();
    } catch (e) {
      this.scheduleReconnect(String(e));
      return;
    }
    const pc = new RTCPeerConnection({ bundlePolicy: 'max-bundle' });
    this.pc = pc;
    const transceiver = pc.addTransceiver('video', { direction: 'recvonly' });
    const caps = RTCRtpReceiver.getCapabilities('video');
    if (caps && typeof transceiver.setCodecPreferences === 'function') {
      const codecs = h264First(caps.codecs);
      if (codecs.length > 0) transceiver.setCodecPreferences(codecs);
    }
    // Opened now so the SCTP association exists; input and control use it from phase 4.
    pc.createDataChannel('control', { ordered: true });
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
        this.teardown();
        this.events.state('replaced');
        this.events.log('сеанс занят другим окном');
        return;
      case 'error':
        this.events.log(`ошибка сервера: ${m.code}`);
        this.scheduleReconnect(`ошибка сервера: ${m.code}`);
        return;
      case 'pong':
        return;
    }
  }
}
