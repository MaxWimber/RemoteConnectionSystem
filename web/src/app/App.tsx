import { useEffect, useRef, useState } from 'preact/hooks';
import { logout } from '../api/auth';
import { InputController } from '../input/controller';
import { CursorView } from '../input/cursor';
import { probeInputLatency } from '../input/probe';
import { TouchController, loadMode, type TouchMode } from '../input/touch';
import { buildReport, download, installErrorCapture, logEvent, recordSample } from '../stream/diagnostics';
import { LatencyMeter, type LatencySummary } from '../stream/latency';
import { StreamSession, type SessionState } from '../stream/session';
import { summarize, toMap, type StatsMap, type StreamSummary } from '../stream/stats';
import { appTitle } from './title';

const STATS_INTERVAL_MS = 1000;
/** A phone or tablet: no mouse, so touch gestures and the screen keyboard. */
const touchDevice = typeof matchMedia === 'function' && matchMedia('(pointer: coarse)').matches;

interface WakeLockLike {
  release(): Promise<void>;
}
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

interface Props {
  /** Called when the sign-in session is over; the reason is shown on the sign-in page. */
  onSignedOut(reason: string): void;
}

export function App({ onSignedOut }: Props) {
  const video = useRef<HTMLVideoElement>(null);
  const cursorImage = useRef<HTMLImageElement>(null);
  const cursor = useRef<CursorView | null>(null);
  const keyboardField = useRef<HTMLTextAreaElement>(null);
  const touch = useRef<TouchController | null>(null);
  const [touchMode, setTouchMode] = useState<TouchMode>(loadMode());
  const [probe, setProbe] = useState('');
  const session = useRef<StreamSession | null>(null);
  const lastStats = useRef<StatsMap | null>(null);
  const agentStatus = useRef<unknown>(null);
  const [state, setState] = useState<SessionState>('idle');
  const [detail, setDetail] = useState<string>('');
  const [showStats, setShowStats] = useState(false);
  const [summary, setSummary] = useState<StreamSummary | null>(null);
  const [latency, setLatency] = useState<LatencySummary | null>(null);
  const meter = useRef<LatencyMeter | null>(null);
  const input = useRef<InputController | null>(null);

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
      signedOut: () => onSignedOut('Сессия завершена: выход, истечение срока или вход с другого устройства.'),
      cursor: (m) => cursor.current?.handle(m),
    });
    session.current = s;
    s.start();
    if (video.current) {
      meter.current = new LatencyMeter(video.current, () => s.clock);
      meter.current.start();
      const controller = new InputController(video.current, () => s.input, logEvent);
      input.current = controller;
      controller.attach();
      if (touchDevice && keyboardField.current) {
        touch.current = new TouchController(video.current, keyboardField.current, () => s.input, logEvent);
        touch.current.attach();
      }
      if (cursorImage.current) {
        const view = new CursorView(
          video.current,
          cursorImage.current,
          () => controller.pointerLocked || (touch.current?.drawsPointer ?? false),
        );
        cursor.current = view;
        document.addEventListener('pointerlockchange', () => view.render());
        window.addEventListener('resize', () => view.render());
      }
    }
    return () => {
      touch.current?.detach();
      input.current?.detach();
      meter.current?.stop();
      s.stop();
    };
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
        const l = meter.current?.summary() ?? null;
        if (s) {
          recordSample(s, l);
          setSummary(s);
        }
        setLatency(l);
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

  // Keep the phone's screen on while the picture plays (P4-10).
  useEffect(() => {
    if (state !== 'playing') return;
    const nav = navigator as Navigator & { wakeLock?: { request(type: 'screen'): Promise<WakeLockLike> } };
    if (!nav.wakeLock) return;
    let lock: WakeLockLike | null = null;
    let done = false;
    const take = () => {
      if (document.visibilityState !== 'visible') return;
      nav
        .wakeLock!.request('screen')
        .then((l) => {
          if (done) void l.release();
          else lock = l;
        })
        .catch((e: unknown) => logEvent(`wake lock: ${String(e)}`));
    };
    take();
    // The lock ends when the page is hidden; take it again when it comes back.
    document.addEventListener('visibilitychange', take);
    return () => {
      done = true;
      document.removeEventListener('visibilitychange', take);
      void lock?.release();
    };
  }, [state]);

  const fullscreen = () => {
    const el = document.documentElement;
    if (document.fullscreenElement) {
      void document.exitFullscreen();
      return;
    }
    void el
      .requestFullscreen({ navigationUI: 'hide' })
      .then(() => {
        // A phone turns to landscape for a 16:9 picture, where the browser allows it (P4-10).
        const orientation = screen.orientation as ScreenOrientation & { lock?: (o: string) => Promise<void> };
        if (touchDevice && orientation.lock) {
          orientation.lock('landscape').catch((e: unknown) => logEvent(`orientation: ${String(e)}`));
        }
      })
      .catch((e) => logEvent(`fullscreen: ${String(e)}`));
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

  const runProbe = async () => {
    const v = video.current;
    if (!v || probe.startsWith('замер…')) return;
    const trials = 30;
    setProbe(`замер… 0/${trials}`);
    try {
      const r = await probeInputLatency(v, () => session.current?.input ?? null, trials, (n) =>
        setProbe(`замер… ${n}/${trials}`),
      );
      logEvent(`замер ввода: ${JSON.stringify(r)}`);
      setProbe(
        `нажатие→показ p50 ${fmt(r.p50)} / p95 ${fmt(r.p95)} мс · мин ${fmt(r.min)} · макс ${fmt(r.max)} · ` +
          `без ответа ${r.misses} из ${r.trials}`,
      );
    } catch (e) {
      setProbe(`замер не удался: ${String(e)}`);
    }
  };

  const reconnect = () => session.current?.start();
  const signOut = async () => {
    input.current?.detach();
    session.current?.stop();
    try {
      await logout(import.meta.env.BASE_URL);
    } catch (e) {
      logEvent(`logout: ${String(e)}`);
    }
    onSignedOut('Вы вышли.');
  };
  const agent = agentStatus.current as Record<string, unknown> | null;
  const message = stateText[state];

  return (
    <div class="screen">
      <video ref={video} class="video" autoplay muted playsInline disablePictureInPicture />
      <img ref={cursorImage} class="cursor-overlay" alt="" hidden />
      <textarea
        ref={keyboardField}
        class="keyboard-field"
        autocomplete="off"
        autocapitalize="off"
        spellcheck={false}
        aria-label="Ввод текста на сервер"
      />
      {message && (
        <div class="status">
          <div>{message}</div>
          {detail && <div class="detail">{detail}</div>}
          {(state === 'replaced' || state === 'stopped') && <button onClick={reconnect}>Подключиться</button>}
        </div>
      )}
      <div class="toolbar" aria-label={appTitle}>
        <button onClick={fullscreen} title="Полный экран">⛶</button>
        {touchDevice ? (
          <>
            <button
              onClick={() => {
                const mode = touch.current?.toggle();
                if (mode) setTouchMode(mode);
                cursor.current?.render();
              }}
              title={touchMode === 'touchpad' ? 'Режим тачпада: нажмите для прямого касания' : 'Прямое касание: нажмите для тачпада'}
            >
              {touchMode === 'touchpad' ? '▭' : '☝'}
            </button>
            <button onClick={() => touch.current?.showKeyboard()} title="Клавиатура">
              ⌨
            </button>
          </>
        ) : (
          <button onClick={() => input.current?.capturePointer()} title="Захватить мышь для игр (Esc отпускает)">
            🖱
          </button>
        )}
        <button onClick={() => setShowStats(!showStats)} title="Статистика">ℹ</button>
        <button onClick={() => void saveDiagnostics()} title="Скачать диагностику">⤓</button>
        <button onClick={() => void signOut()} title="Выйти">⏻</button>
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
            захват→показ p50 {fmt(latency?.captureToDisplayP50 ?? null)} / p95 {fmt(latency?.captureToDisplayP95 ?? null)}{' '}
            мс · захват→приём {fmt(latency?.captureToReceiveP50 ?? null)} мс · приём→показ{' '}
            {fmt(latency?.receiveToDisplayP50 ?? null)} мс · часы ±{fmt(latency?.clockErrorMs ?? null)} мс
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
          {agent && (
            <div class="detail">
              ввод: {String(agent['input'] ?? '—')} · событий {String(agent['input_events'] ?? '—')} · отброшено{' '}
              {String(agent['input_dropped'] ?? '—')} · ошибок {String(agent['input_errors'] ?? '—')}
            </div>
          )}
          <div class="probe">
            <button onClick={() => void runProbe()} title="Нужна страница input-test.html во весь экран на сервере">
              Замер ввода
            </button>{' '}
            {probe}
          </div>
          {cursor.current && (
            <div class="detail">{cursor.current.describe()}</div>
          )}
        </div>
      )}
    </div>
  );
}
