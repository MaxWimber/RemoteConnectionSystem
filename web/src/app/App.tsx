import { useEffect, useRef, useState } from 'preact/hooks';
import { buildReport, download, installErrorCapture, logEvent, recordSample } from '../stream/diagnostics';
import { StreamSession, type SessionState } from '../stream/session';
import { summarize, toMap, type StatsMap, type StreamSummary } from '../stream/stats';
import { appTitle } from './title';

const STATS_INTERVAL_MS = 1000;
const AGENT_INTERVAL_MS = 2000;

const stateText: Record<SessionState, string> = {
  idle: 'Не подключено',
  connecting: 'Подключение…',
  playing: '',
  reconnecting: 'Переподключение…',
  replaced: 'Сеанс открыт в другом окне',
  stopped: 'Отключено',
};

function fmt(value: number | null, digits = 1, unit = ''): string {
  return value === null ? '—' : `${value.toFixed(digits)}${unit}`;
}

export function App() {
  const video = useRef<HTMLVideoElement>(null);
  const session = useRef<StreamSession | null>(null);
  const lastStats = useRef<StatsMap | null>(null);
  const agentStatus = useRef<unknown>(null);
  const [state, setState] = useState<SessionState>('idle');
  const [detail, setDetail] = useState<string>('');
  const [showStats, setShowStats] = useState(false);
  const [summary, setSummary] = useState<StreamSummary | null>(null);

  useEffect(() => {
    installErrorCapture();
    const s = new StreamSession(import.meta.env.BASE_URL, {
      state: (next, why) => {
        setState(next);
        setDetail(why ?? '');
        logEvent(`state ${next}${why ? `: ${why}` : ''}`);
      },
      log: logEvent,
      stream: (stream) => {
        if (video.current) video.current.srcObject = stream;
      },
    });
    session.current = s;
    s.start();
    return () => s.stop();
  }, []);

  // getStats once a second: the panel, and samples for the diagnostics file.
  useEffect(() => {
    const id = window.setInterval(async () => {
      const pc = session.current?.peer;
      if (!pc) return;
      try {
        const now = toMap(await pc.getStats());
        const s = summarize(now, lastStats.current ?? undefined);
        lastStats.current = now;
        if (s) {
          recordSample(s);
          setSummary(s);
        }
      } catch (e) {
        logEvent(`getStats: ${String(e)}`);
      }
    }, STATS_INTERVAL_MS);
    return () => window.clearInterval(id);
  }, []);

  useEffect(() => {
    if (!showStats) return;
    const load = async () => {
      try {
        const r = await fetch(`${import.meta.env.BASE_URL}api/status`, { cache: 'no-store' });
        agentStatus.current = r.ok ? await r.json() : { error: r.status };
      } catch (e) {
        agentStatus.current = { error: String(e) };
      }
    };
    void load();
    const id = window.setInterval(load, AGENT_INTERVAL_MS);
    return () => window.clearInterval(id);
  }, [showStats]);

  const fullscreen = () => {
    const el = document.documentElement;
    if (document.fullscreenElement) void document.exitFullscreen();
    else void el.requestFullscreen({ navigationUI: 'hide' }).catch((e) => logEvent(`fullscreen: ${String(e)}`));
  };

  const saveDiagnostics = async () => {
    let stats: StatsMap | null = lastStats.current;
    const pc = session.current?.peer;
    if (pc) {
      try {
        stats = toMap(await pc.getStats());
      } catch {
        // keep the last sample
      }
    }
    try {
      const r = await fetch(`${import.meta.env.BASE_URL}api/status`, { cache: 'no-store' });
      agentStatus.current = r.ok ? await r.json() : { error: r.status };
    } catch (e) {
      agentStatus.current = { error: String(e) };
    }
    download(buildReport(stats, agentStatus.current));
  };

  const reconnect = () => session.current?.start();
  const agent = agentStatus.current as Record<string, unknown> | null;
  const message = stateText[state];

  return (
    <div class="screen">
      <video ref={video} class="video" autoplay muted playsInline disablePictureInPicture />
      {message && (
        <div class="status">
          <div>{message}</div>
          {detail && <div class="detail">{detail}</div>}
          {(state === 'replaced' || state === 'stopped') && <button onClick={reconnect}>Подключиться</button>}
        </div>
      )}
      <div class="toolbar" aria-label={appTitle}>
        <button onClick={fullscreen} title="Полный экран">⛶</button>
        <button onClick={() => setShowStats(!showStats)} title="Статистика">ℹ</button>
        <button onClick={() => void saveDiagnostics()} title="Скачать диагностику">⤓</button>
      </div>
      {showStats && (
        <div class="stats">
          <div>
            {summary?.width ?? '—'}×{summary?.height ?? '—'} · {fmt(summary?.fps ?? null, 0)} к/с ·{' '}
            {fmt(summary?.bitrateMbps ?? null)} Мбит/с
          </div>
          <div>
            буфер {fmt(summary?.jitterBufferMs ?? null)} мс · декод. {fmt(summary?.decodeMs ?? null)} мс · обработка{' '}
            {fmt(summary?.processingMs ?? null)} мс · RTT {fmt(summary?.rttMs ?? null)} мс
          </div>
          <div>
            потеряно {summary?.packetsLost ?? 0} · NACK {summary?.nackCount ?? 0} · PLI {summary?.pliCount ?? 0} ·
            сброшено {summary?.framesDropped ?? 0} · фризы {summary?.freezeCount ?? 0}
          </div>
          <div class="detail">
            {summary?.decoder ?? '—'}
            {summary?.powerEfficientDecoder ? ' (аппаратный)' : ''} · {summary?.codec ?? '—'}
          </div>
          {agent && (
            <div class="detail">
              сервер: {String(agent['encoder'] ?? '—')} · отправлено {String(agent['frames_sent'] ?? '—')} · сброшено
              очередью {String(agent['frames_dropped_queue'] ?? '—')} · запросов IDR{' '}
              {String(agent['keyframe_requests'] ?? '—')}
            </div>
          )}
        </div>
      )}
    </div>
  );
}
