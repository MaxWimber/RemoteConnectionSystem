// WebSocket signaling from docs/PROTOCOL.md.

export const PROTOCOL_VERSION = 1;

export type ServerMessage =
  | { v: number; type: 'answer'; sdp: string }
  | { v: number; type: 'session-replaced' }
  | { v: number; type: 'resumed' }
  | { v: number; type: 'error'; code: string }
  | { v: number; type: 'pong'; id: number };

export function signalUrl(location: Location, base: string): string {
  const scheme = location.protocol === 'https:' ? 'wss:' : 'ws:';
  const prefix = base.endsWith('/') ? base : `${base}/`;
  return `${scheme}//${location.host}${prefix}api/signal`;
}

/** Parses a server message; anything not from this protocol version is reported as an error. */
export function parseServerMessage(text: string): ServerMessage {
  let value: unknown;
  try {
    value = JSON.parse(text);
  } catch {
    return { v: PROTOCOL_VERSION, type: 'error', code: 'bad_message' };
  }
  const m = value as Record<string, unknown>;
  if (m['v'] !== PROTOCOL_VERSION || typeof m['type'] !== 'string') {
    return { v: PROTOCOL_VERSION, type: 'error', code: 'protocol_mismatch' };
  }
  switch (m['type']) {
    case 'answer':
      return typeof m['sdp'] === 'string'
        ? { v: PROTOCOL_VERSION, type: 'answer', sdp: m['sdp'] }
        : { v: PROTOCOL_VERSION, type: 'error', code: 'bad_message' };
    case 'session-replaced':
      return { v: PROTOCOL_VERSION, type: 'session-replaced' };
    case 'resumed':
      return { v: PROTOCOL_VERSION, type: 'resumed' };
    case 'error':
      return { v: PROTOCOL_VERSION, type: 'error', code: String(m['code'] ?? 'unknown') };
    case 'pong':
      return { v: PROTOCOL_VERSION, type: 'pong', id: Number(m['id'] ?? 0) };
    default:
      return { v: PROTOCOL_VERSION, type: 'error', code: 'bad_message' };
  }
}

/** Close code the server uses when the sign-in session ends: go back to sign-in, do not reconnect. */
export const CLOSE_SESSION_ENDED = 4401;

export class Signaling {
  private socket: WebSocket | null = null;

  constructor(
    private readonly url: string,
    private readonly onMessage: (m: ServerMessage) => void,
    private readonly onClose: (reason: string, code: number) => void,
  ) {}

  open(): Promise<void> {
    return new Promise((resolve, reject) => {
      const socket = new WebSocket(this.url);
      this.socket = socket;
      let opened = false;
      socket.onopen = () => {
        opened = true;
        resolve();
      };
      socket.onmessage = (e) => {
        if (typeof e.data === 'string') this.onMessage(parseServerMessage(e.data));
      };
      socket.onclose = (e) => {
        if (!opened) reject(new Error(`signaling closed (${e.code})`));
        else this.onClose(`signaling closed (${e.code})`, e.code);
      };
    });
  }

  send(message: Record<string, unknown>): void {
    this.socket?.send(JSON.stringify({ v: PROTOCOL_VERSION, ...message }));
  }

  close(): void {
    const socket = this.socket;
    this.socket = null;
    if (socket) {
      socket.onclose = null;
      socket.close();
    }
  }
}
