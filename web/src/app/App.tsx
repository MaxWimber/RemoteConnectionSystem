import { useEffect, useRef, useState } from 'preact/hooks';
import { logout } from '../api/auth';
import { FileManager } from '../files/FileManager';
import { browserQueue, useQueue } from '../files/queue';
import { InputController } from '../input/controller';
import { CursorView } from '../input/cursor';
import { GamepadPoller } from '../input/gamepads';
import { ScreenPad } from '../gamepad-ui/screen-pad';
import { probeInputLatency } from '../input/probe';
import { TouchController } from '../input/touch';
import {
  buildReport,
  download,
  installErrorCapture,
  logEvent,
  recordSample,
  recordServerStats,
} from '../stream/diagnostics';
import { audioDelayMs, SoundPlayer, type SoundState } from '../stream/audio';
import { probeAvSync } from '../stream/avsync';
import { LatencyMeter, type LatencySummary } from '../stream/latency';
import { StreamSession, type SessionState } from '../stream/session';
import { summarize, toMap, type StatsMap, type StreamSummary } from '../stream/stats';
import {
  BITRATE_LIMITS,
  MODES,
  loadBitrateLimit,
  loadMode,
  mbps,
  modeLabel,
  type ServerStats,
  type StreamMode,
} from '../stream/server-stats';
import { appTitle } from './title';

const STATS_INTERVAL_MS = 1000;
/** A phone or tablet: no mouse, so touches and the screen keyboard. */
const touchDevice = typeof matchMedia === 'function' && matchMedia('(pointer: coarse)').matches;

interface WakeLockLike {
  release(): Promise<void>;
}
const AGENT_INTERVAL_MS = 2000;
/** Seconds of sound delay samples behind the p50 in the panel. */
const AUDIO_DELAY_SAMPLES = 30;

const soundTitle: Record<SoundState, string> = {
  off: 'Звук выключен',
  waiting: 'Звук включится после касания или щелчка',
  playing: 'Выключить звук',
};
const soundIcon: Record<SoundState, string> = { off: '🔇', waiting: '🔈', playing: '🔊' };

function median(values: number[]): number | null {
  if (values.length === 0) return null;
  const sorted = [...values].sort((a, b) => a - b);
  return sorted[Math.floor(sorted.length / 2)]!;
}

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
  const [probe, setProbe] = useState('');
  const [avProbe, setAvProbe] = useState('');
  const sound = useRef<SoundPlayer | null>(null);
  const [soundState, setSoundState] = useState<SoundState>('waiting');
  const audioDelays = useRef<number[]>([]);
  const gamepads = useRef<GamepadPoller | null>(null);
  const [notice, setNotice] = useState('');
  const screenEl = useRef<HTMLDivElement>(null);
  const screenPad = useRef<ScreenPad | null>(null);
  const [padShown, setPadShown] = useState(false);
  const [filesShown, setFilesShown] = useState(false);
  const [uploads] = useState(() => browserQueue(import.meta.env.BASE_URL));
  useQueue(uploads);
  const sending = uploads.progress();
  const session = useRef<StreamSession | null>(null);
  const lastStats = useRef<StatsMap | null>(null);
  const agentStatus = useRef<unknown>(null);
  const [state, setState] = useState<SessionState>('idle');
  const [detail, setDetail] = useState<string>('');
  const [showStats, setShowStats] = useState(false);
  const [summary, setSummary] = useState<StreamSummary | null>(null);
  const [latency, setLatency] = useState<LatencySummary | null>(null);
  const [serverStats, setServerStats] = useState<ServerStats | null>(null);
  const [limitKbps, setLimitKbps] = useState<number | null>(() => loadBitrateLimit());
  const [fixedRate, setFixedRate] = useState(false);
  const [mode, setMode] = useState<StreamMode>(() => loadMode());
  const meter = useRef<LatencyMeter | null>(null);
  const input = useRef<InputController | null>(null);

  // A leaving page cancels nothing: the server keeps the uploads and the same file resumes after a reload.
  useEffect(() => {
    const warn = (e: BeforeUnloadEvent) => {
      if (uploads.active) e.preventDefault();
    };
    window.addEventListener('beforeunload', warn);
    return () => window.removeEventListener('beforeunload', warn);
  }, [uploads]);

  useEffect(() => {
    installErrorCapture();
    const player = new SoundPlayer((st) => {
      setSoundState(st);
      logEvent(`звук: ${st}`);
    });
    sound.current = player;
    setSoundState(player.state);
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
      audio: (track) => {
        audioDelays.current = [];
        player.attach(track);
      },
      signedOut: () => onSignedOut('Сессия завершена: выход, истечение срока или вход с другого устройства.'),
      cursor: (m) => cursor.current?.handle(m),
      serverStats: (st) => {
        setServerStats(st);
        recordServerStats(st);
      },
      settingsApplied: (maxKbps, adaptive, applied) => {
        setFixedRate(!adaptive);
        logEvent(
          `настройки на сервере: режим ${applied}, предел ${maxKbps} кбит/с${adaptive ? '' : ', битрейт фиксированный'}`,
        );
      },
    });
    session.current = s;
    s.start();
    if (video.current) {
      meter.current = new LatencyMeter(video.current, () => s.clock);
      meter.current.start();
      const controller = new InputController(video.current, () => s.input, logEvent);
      input.current = controller;
      controller.attach();
      // P5-2: a gamepad that the server cannot take is said so, the picture goes on.
      const checkGamepads = async () => {
        try {
          const r = await fetch(`${import.meta.env.BASE_URL}api/status`, { cache: 'no-store' });
          const status = r.ok ? ((await r.json()) as Record<string, unknown>) : null;
          const g = String(status?.['gamepad'] ?? '');
          if (g && g !== 'vigem' && g !== 'record') {
            setNotice(`Геймпад не передаётся: ${g}`);
            logEvent(`геймпад: ${g}`);
            window.setTimeout(() => setNotice(''), 8000);
          }
        } catch (e) {
          logEvent(`геймпад: статус: ${String(e)}`);
        }
      };
      const poller = new GamepadPoller(() => s.input, logEvent, () => void checkGamepads());
      gamepads.current = poller;
      poller.attach();
      if (touchDevice && screenEl.current) {
        const pad = new ScreenPad(screenEl.current, () => poller.wake());
        screenPad.current = pad;
        poller.addSource(() => pad.read());
      }
      if (touchDevice && keyboardField.current) {
        touch.current = new TouchController(video.current, keyboardField.current, () => s.input);
        touch.current.attach();
      }
      if (cursorImage.current) {
        const view = new CursorView(video.current, cursorImage.current, () =>
          controller.pointerLocked ? 'locked' : 'css',
        );
        cursor.current = view;
        document.addEventListener('pointerlockchange', () => view.render());
        window.addEventListener('resize', () => view.render());
      }
    }
    return () => {
      screenPad.current?.destroy();
      gamepads.current?.detach();
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
        const rx = session.current?.audioReceiver;
        const delay = rx ? audioDelayMs(rx, session.current?.clock ?? null) : null;
        if (delay !== null) {
          audioDelays.current.push(delay);
          if (audioDelays.current.length > AUDIO_DELAY_SAMPLES) audioDelays.current.shift();
        }
        if (l) l.audioDelayP50 = median(audioDelays.current);
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

  const runAvProbe = async () => {
    const v = video.current;
    const stream = sound.current?.stream;
    if (!v || avProbe.startsWith('замер…')) return;
    if (!stream) {
      setAvProbe('нет звуковой дорожки');
      return;
    }
    setAvProbe('замер… 20 с');
    // On the server itself the sound would come back through the loopback: listen without playing.
    sound.current?.silence(true);
    try {
      const r = await probeAvSync(v, stream, import.meta.env.BASE_URL, 20, (f, c) =>
        setAvProbe(`замер… вспышек ${f}, щелчков ${c}`),
      );
      logEvent(`замер звука и видео: ${JSON.stringify(r)}`);
      setAvProbe(
        `звук позже видео: p50 ${fmt(r.p50)} / p95 ${fmt(r.p95)} мс · мин ${fmt(r.min)} · макс ${fmt(r.max)} · ` +
          `пар ${r.samples.length} из ${r.flashes} вспышек, щелчков ${r.clicks} · вывод WebAudio ${fmt(r.outputLatencyMs)} мс`,
      );
    } catch (e) {
      setAvProbe(`замер не удался: ${String(e)}`);
    } finally {
      sound.current?.silence(false);
    }
  };

  /** The on-screen pad needs ViGEmBus too: say so when it is shown. */
  const checkPadServer = async () => {
    try {
      const r = await fetch(`${import.meta.env.BASE_URL}api/status`, { cache: 'no-store' });
      const g = r.ok ? String(((await r.json()) as Record<string, unknown>)['gamepad'] ?? '') : '';
      if (g && g !== 'vigem' && g !== 'record') {
        setNotice(`Геймпад не передаётся: ${g}`);
        window.setTimeout(() => setNotice(''), 8000);
      }
    } catch {
      // The pad still shows; the status panel tells more.
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
    <div class="screen" ref={screenEl}>
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
      {notice && <div class="notice">{notice}</div>}
      <div class="toolbar" aria-label={appTitle}>
        <button onClick={fullscreen} title="Полный экран">⛶</button>
        <button
          onClick={() => session.current?.input?.send({ type: 'special', code: 1 })}
          title="Ctrl+Alt+Del: экран безопасности Windows"
        >
          🔐
        </button>
        {touchDevice ? (
          <>
            <button onClick={() => touch.current?.showKeyboard()} title="Клавиатура">
              ⌨
            </button>
            <button
              onClick={() => {
                const shown = screenPad.current?.toggle() ?? false;
                setPadShown(shown);
                if (shown) void checkPadServer();
              }}
              title={padShown ? 'Скрыть экранный геймпад' : 'Экранный геймпад'}
            >
              🎮
            </button>
          </>
        ) : (
          <button onClick={() => input.current?.capturePointer()} title="Захватить мышь для игр (Esc отпускает)">
            🖱
          </button>
        )}
        <button
          onClick={() => setFilesShown(!filesShown)}
          title={sending.count > 0 ? `Файлы: загружается ${sending.count}` : 'Файлы'}
        >
          📁
          {sending.count > 0 && (
            <span class="files-badge">{sending.size > 0 ? Math.floor((sending.sent / sending.size) * 100) : 0}%</span>
          )}
        </button>
        <button
          onClick={() => sound.current?.setEnabled(soundState === 'off')}
          title={soundTitle[soundState]}
          aria-pressed={soundState !== 'off'}
        >
          {soundIcon[soundState]}
        </button>
        <button onClick={() => setShowStats(!showStats)} title="Статистика">ℹ</button>
        <button onClick={() => void saveDiagnostics()} title="Скачать диагностику">⤓</button>
        <button onClick={() => void signOut()} title="Выйти">⏻</button>
      </div>
      {filesShown && <FileManager base={import.meta.env.BASE_URL} uploads={uploads} onClose={() => setFilesShown(false)} />}
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
            звук: {soundState === 'playing' ? 'играет' : soundState === 'off' ? 'выключен' : 'ждёт касания'} · захват→выдача p50{' '}
            {fmt(latency?.audioDelayP50 ?? null)} мс · позже видео на{' '}
            {fmt(
              latency?.audioDelayP50 != null && latency.captureToDisplayP50 != null
                ? latency.audioDelayP50 - latency.captureToDisplayP50
                : null,
            )}{' '}
            мс · {serverStats?.audioKbps ?? '—'} кбит/с
          </div>
          <div>
            потеряно {summary?.packetsLost ?? 0} · NACK {summary?.nackCount ?? 0} · PLI {summary?.pliCount ?? 0} ·
            сброшено {summary?.framesDropped ?? 0} · фризы {summary?.freezeCount ?? 0}
          </div>
          <div>
            сервер: цель {mbps(serverStats?.targetKbps ?? null)} · оценка {mbps(serverStats?.estimateKbps ?? null)}{' '}
            · отправлено {mbps(serverStats?.sentKbps ?? null)} Мбит/с · потери{' '}
            {serverStats?.loss == null ? '—' : `${(serverStats.loss * 100).toFixed(1)}%`} · RTT{' '}
            {fmt(serverStats?.rttMs ?? null)} мс
          </div>
          <div class="limit">
            <label>
              режим{' '}
              <select
                value={mode}
                onChange={(e) => {
                  const m = (e.currentTarget as HTMLSelectElement).value as StreamMode;
                  setMode(m);
                  session.current?.setMode(m);
                }}
              >
                {MODES.map((m) => (
                  <option key={m.mode} value={m.mode}>
                    {m.label}
                  </option>
                ))}
              </select>
            </label>{' '}
            {serverStats && serverStats.activeMode !== serverStats.mode && (
              <span class="detail">сейчас «{modeLabel(serverStats.activeMode)}»: сеть слабая</span>
            )}
          </div>
          <div class="limit">
            <label>
              предел битрейта{' '}
              <select
                value={limitKbps === null ? '' : String(limitKbps)}
                onChange={(e) => {
                  const v = (e.currentTarget as HTMLSelectElement).value;
                  const kbps = v === '' ? null : Number(v);
                  setLimitKbps(kbps);
                  session.current?.setBitrateLimit(kbps);
                }}
              >
                {BITRATE_LIMITS.map((k) => (
                  <option key={String(k)} value={k === null ? '' : String(k)}>
                    {k === null ? 'без предела' : `${k / 1000} Мбит/с`}
                  </option>
                ))}
              </select>
            </label>{' '}
            {serverStats && (
              <span class="detail">
                {fixedRate || !serverStats.adaptive
                  ? 'битрейт фиксированный'
                  : `диапазон ${mbps(serverStats.minKbps)}–${mbps(serverStats.maxKbps)} Мбит/с`}
              </span>
            )}
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
          <div class="probe">
            <button onClick={() => void runAvProbe()} title="Нужно окно rcs-bench avsync на сервере">
              Замер звука
            </button>{' '}
            {avProbe}
          </div>
          {agent && (
            <div class="detail">
              геймпад: сервер {String(agent['gamepad'] || '—')} · состояний {String(agent['gamepad_events'] ?? '—')} ·
              в браузере {gamepads.current?.connected ?? 0}
            </div>
          )}
          {cursor.current && (
            <div class="detail">{cursor.current.describe()}</div>
          )}
        </div>
      )}
    </div>
  );
}
