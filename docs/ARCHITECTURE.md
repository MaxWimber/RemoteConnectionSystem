# Архитектура

## Схема

```text
Браузер (Chrome/Edge на ПК, Chrome на Android)
  │ 8444/TCP HTTPS и WSS: вход, сигналинг, файлы
  │ 8443/UDP WebRTC: видео H.264 и каналы данных
  ▼
Роутер: проброс 80/TCP, 8444/TCP, 8443/UDP
  ▼
┌─ Сервер ──────────────────────────────────────────────────────────┐
│ Caddy (служба): TLS, HSTS, прокси /RemoteConnectionSystem          │
│   ▼ 127.0.0.1:8080                                                 │
│ rcs-web (служба, NT SERVICE\rcs-web)                               │
│   авторизация, сигналинг, файлы, раздача веб-клиента               │
│   ▼ \\.\pipe\rcs-agent (ACL: только rcs-web)                       │
│ rcs-agent (SYSTEM, консольная сессия)                              │
│   захват → кодирование → WebRTC на 8443/UDP; приём и эмуляция ввода│
│   ▲ запуск и перезапуск                                            │
│ rcs-supervisor (служба, SYSTEM, сессия 0)                          │
└────────────────────────────────────────────────────────────────────┘
```

## Процессы

| Процесс | Учётная запись | Доступ из сети | Ответственность |
|---|---|---|---|
| Caddy | Отдельная служба | 80/TCP, 8444/TCP | TLS, сертификаты, прокси, заголовки |
| `rcs-web` | `NT SERVICE\rcs-web` | Только `127.0.0.1:8080` | Вход, сессии, сигналинг, файлы, статика |
| `rcs-agent` | SYSTEM в консольной сессии | 8443/UDP | Захват, кодирование, WebRTC, ввод |
| `rcs-supervisor` | SYSTEM, сессия 0 | Нет | Запуск агента, отслеживание сессий, Secure Attention Sequence |

До фазы 6 `rcs-agent` и `rcs-web` запускаются вручную как обычные процессы пользователя. На ПК разработки так
остаётся всегда: службы туда не устанавливаются, сокеты открываются только на `127.0.0.1`.

## Поток кадра

```text
AcquireNextFrame            текстура BGRA на GPU
  ▼
CopyResource                в пул из 4 текстур GPU, кадр Desktop Duplication сразу освобождается
  ▼
AMF VideoConverter          BGRA → NV12, BT.709, limited range, на GPU
  ▼
AMF VideoEncoder            H.264, Annex B, сервер дописывает цвет в VUI (D-017)
  ▼
Bounded-очередь             ёмкость 2, поток захвата не ждёт
  ▼
RTP-пакетизация, SRTP       поток WebRTC
  ▼
UDP
  ▼
Браузер: приём → декодирование → <video>
```

Требования:

- Устройство D3D11 создаётся на адаптере RX 560. Захват и AMF используют одно устройство.
- У потока захвата и кодирования приоритет MMCSS `Games`. Пробуждение таймера настраивается замером.
- На время захвата разрешение таймера 1 мс (`timeBeginPeriod`), иначе короткие тайм-ауты просыпаются с шагом 15,6 мс.
- Захват берёт новые кадры не чаще частоты потока (D-019). Кредит растёт со скоростью потока до одного кадра, кадр
  берётся при кредите от 0,9 и стоит 1. Источник с частотой дисплея не ждёт, игра без vsync даёт ровно частоту потока.
- `AcquireNextFrame` сообщает о кадре только при изменении экрана. Повтор последнего кадра выполняется
  с минимальной частотой 15 FPS, значение уточняется в фазе 1.
- Смена режима экрана, `DXGI_ERROR_ACCESS_LOST` и `DXGI_ERROR_DEVICE_REMOVED` приводят к пересозданию конвейера.
- После пересоздания или потери пакетов (PLI, FIR) кодировщик выдаёт IDR.
- На этапах ставятся отметки времени. Гистограммы доступны в `rcs-bench` и панели статистики.

## Поток ввода

```text
DOM-события, Gamepad API
  ▼
web/src/input            нормализация, объединение событий до частоты кадра
  ▼
input-fast / input-reliable
  ▼
rcs-agent: поток ввода (приоритет выше обычного)
  ▼
InputSink: SendInput, InjectTouchInput
GamepadBackend: ViGEmBus
```

Движение мыши и геймпад не проходят через поток захвата. Правила пакетов находятся в [PROTOCOL.md](PROTOCOL.md).

## Сессии и рабочие столы

`rcs-supervisor` работает под SYSTEM в сессии 0 и не может захватывать экран. Поэтому он:

1. Получает активную сессию через `WTSGetActiveConsoleSessionId`.
2. Дублирует свой токен и задаёт `TokenSessionId` этой сессии.
3. Запускает `rcs-agent` через `CreateProcessAsUserW` на `winsta0\default`.
4. На `SERVICE_CONTROL_SESSIONCHANGE` перезапускает агента в новой сессии.
5. При аварийном завершении перезапускает агента с нарастающей паузой.

Агент перед созданием захвата и после `DXGI_ERROR_ACCESS_LOST` вызывает `OpenInputDesktop` и
`SetThreadDesktop`, чтобы работать на рабочем столе, который принимает ввод: `Default` или `Winlogon`.
`SendSAS` выполняется из `rcs-supervisor`, агент передаёт запрос по локальному каналу.

## Выбор кодировщика

Настройка `media.encoder`: `auto`, `amf`, `mf`. Значение `auto` пробует AMF, затем Media Foundation.

- AMF загружается во время работы через `LoadLibrary("amfrt64.dll")`. Поэтому код собирается и запускается на ПК без AMD.
- Если `amf` указан явно, но недоступен, агент завершается с понятной ошибкой, а не переключается молча.
- В журнал и статистику всегда пишется фактический бэкенд, адаптер и версия драйвера.
- `rcs_encode::open` открывает бэкенд, оба реализуют трейт `Encoder` и сами дописывают цвет в VUI (D-017).
- `amf`: конвертер AMF BGRA → NV12 и кодировщик Low Latency с LCVBR (D-016, D-018).
- `mf`: видеопроцессор D3D11 BGRA → NV12, затем аппаратный H.264 MFT того же адаптера через `IMFDXGIDeviceManager`.
  Программные MFT не используются, им нужен кадр в оперативной памяти.

## Диагностика

Сервер проверяется только по логам, поэтому диагностика является частью продукта.

- Все компоненты пишут структурированные журналы `tracing` в JSON с ротацией и номером сборки в каждой строке.
- Обработчик паники пишет обратную трассировку в журнал, тестовые пакеты содержат PDB.
- `rcs-bench probe` выводит адаптеры, режимы, драйвер, версию AMF и возможности кодировщика в JSON.
- `rcs-bench capture` ведёт настоящий конвейер захвата и кодирования, пишет `.h264` и отчёт с этапами и гистограммами.
- `rcs-bench scenarios` по очереди запускает нагрузку (`rcs-bench load` или ffplay) и `capture` для каждого сценария.
- `rcs-bench matrix` за один запуск перебирает набор настроек и пишет отчёт по каждой.
- Клиент получает кнопку «Скачать диагностику»: `getStats`, ошибки консоли, версия браузера, сведения об устройстве.
- `scripts/collect-logs.ps1` собирает архив без секретов. Состав указан в [TESTING.md](TESTING.md).

## Rust-воркспейс

```text
crates/
  rcs-protocol/    форматы сообщений, кодирование и разбор, без зависимостей от Windows
  rcs-capture/     трейт Capture, реализация DXGI
  rcs-encode/      трейт Encoder, бэкенды amf и mf
  rcs-input/       трейты InputSink и GamepadBackend, реализации SendInput и ViGEm
  rcs-webrtc/      обёртка над выбранной WebRTC-библиотекой
  rcs-agent/       сборка конвейера, сессия WebRTC, поток ввода
  rcs-web/         axum, авторизация, сигналинг, файлы
  rcs-supervisor/  служба Windows
  rcs-bench/       замеры захвата и кодирования
native/amf_shim/   C API поверх C++ AMF
third_party/amf/   заголовки AMF SDK на фиксированном теге
web/               Vite, TypeScript, Preact
deploy/            Caddyfile, install.ps1, шаблон config.toml
scripts/           check.ps1, package.ps1, collect-logs.ps1, сценарии проверки фаз
server-logs/       логи с сервера, не попадают в git
dist/              собранные пакеты, не попадают в git
docs/              документация
```

Зависимости направлены строго вниз: `rcs-agent` знает `rcs-capture`, `rcs-encode`, `rcs-input`,
`rcs-webrtc` и `rcs-protocol`. Эти крейты не знают друг о друге, кроме типа кадра, вынесенного в
`rcs-capture`. Только `rcs-encode` собирает `native/amf_shim` через крейт `cc`.

## Границы трейтов

Сигнатуры являются ориентиром и уточняются при реализации.

```rust
pub trait Capture {
    fn next_frame(&mut self, timeout: Duration) -> Result<CaptureEvent, CaptureError>;
    fn size(&self) -> (u32, u32);
}

pub enum CaptureEvent<'a> {
    Frame(CapturedFrame<'a>),
    Timeout,
    Reinitialize,
}

pub trait Encoder {
    // pts растёт с каждым вызовом, в том числе для повторов. Буфер `out` переиспользуется между кадрами.
    fn encode(&mut self, frame: &CapturedFrame<'_>, pts: i64, force_idr: bool) -> Result<SubmitStats, EncodeError>;
    fn poll(&mut self, out: &mut EncodedFrame) -> Result<bool, EncodeError>;
    fn set_bitrate(&mut self, bits_per_second: u32) -> Result<(), EncodeError>;
    fn backend(&self) -> &'static str;          // "amf" или "mf"
    fn settings(&self) -> Option<String>;       // фактические настройки, JSON
}

pub trait InputSink {
    fn handle(&mut self, event: &InputEvent) -> Result<(), InputError>;
    fn release_all(&mut self) -> Result<(), InputError>;
}

pub trait GamepadBackend {
    fn update(&mut self, pad: u8, state: &GamepadState) -> Result<(), InputError>;
    fn disconnect(&mut self, pad: u8) -> Result<(), InputError>;
}
```

`EncodedFrame` содержит данные Annex B, признак ключевого кадра и отметки времени этапов.

## Обмен между `rcs-web` и агентом

Именованный канал `\\.\pipe\rcs-agent`, сообщения в JSON с длиной в начале. ACL разрешает доступ только
`NT SERVICE\rcs-web` и SYSTEM. На этапах разработки до фазы 6 используется текущий пользователь.

| Запрос | Ответ | Назначение |
|---|---|---|
| `OpenSession { session_id, sdp_offer }` | `SessionOpened { sdp_answer }` | Запуск медиасессии |
| `CloseSession { session_id }` | `SessionClosed` | Завершение при выходе или истечении сессии |
| `Status` | `AgentStatus` | Состояние для интерфейса |

Новая `OpenSession` завершает предыдущую. Канал доступен только `rcs-web`, а `rcs-web` принимает SDP
только от авторизованной сессии.

## Веб-клиент

```text
web/src/
  app/         маршруты: вход, поток, файлы, настройки
  api/         HTTP и WebSocket
  stream/      RTCPeerConnection, <video>, статистика
  input/       мышь, клавиатура, touch, gamepad, scancodes.ts, кодирование протокола
  gamepad-ui/  экранный геймпад и редактор раскладки
  files/       менеджер файлов и докачка
```

- Vite: `base: '/RemoteConnectionSystem/'`. Все адреса строятся от этого префикса.
- Обработчики ввода не проходят через состояние Preact. Интерфейс не должен влиять на частоту ввода.
- Поддерживаются Pointer Lock, Keyboard Lock и Fullscreen на ПК, Wake Lock и Pointer Events на Android.

## Низкая задержка в браузере

Проверяются в фазах 2 и 8, в браузере применяется то, что поддерживается:

- `RTCRtpReceiver.jitterBufferTarget = 0`.
- RTP-расширение `playout-delay` со значениями 0, если его поддерживает WebRTC-библиотека.
- `requestVideoFrameCallback` для отметок приёма и показа.
- `getStats`: `jitterBufferDelay`, `totalDecodeTime`, `framesDropped`, `framesPerSecond`, `nackCount`.

## Настройка

`%ProgramData%\RCS\config.toml`:

```toml
[server]
listen = "127.0.0.1:8080"
base_path = "/RemoteConnectionSystem"
public_origin = "https://maxwimber.ru:8444"

[media]
encoder = "auto"
public_ip = "SERVER_PUBLIC_IP"
udp_port = 8443
width = 1920
height = 1080
fps = 60
bitrate_mbps = 30

[files]
root = "D:\\RCS-Storage"
max_upload_gib = 200
```

Каталог секретов: `%ProgramData%\RCS\secrets`, правила в [SECURITY.md](SECURITY.md).
