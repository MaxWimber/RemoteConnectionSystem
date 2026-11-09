//! `rcs-bench avsync`: the source of the sound and picture measurement (P9-1, D-038).
//!
//! A 400x400 window in the top-left corner of the screen is black and turns white for 100 ms once a second, at the
//! moment the Windows mixer takes the period that holds a 5 ms click. The client's «Замер звука» finds the flash in
//! the shown frames and the click in the received sound. The window never takes focus; a click on it ends the run.
//! At the same time the tool listens to the loopback of the same device, as the agent does, and reports how far each
//! click passed the mixer after its flash was drawn: the share of the measured skew that comes from this source.
//! The result goes to `avsync.json` in the output folder.

use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rcs_capture::dxgi;
use serde_json::json;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, InvalidateRect, PAINTSTRUCT,
    UpdateWindow,
};
use windows::Win32::Media::Audio::{
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioClient, IAudioRenderClient, IMMDeviceEnumerator,
    MMDeviceEnumerator, WAVEFORMATEX, eConsole, eRender,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, IDC_ARROW, LoadCursorW,
    MA_NOACTIVATE, MSG, PM_REMOVE, PeekMessageW, PostQuitMessage, RegisterClassW,
    SW_SHOWNOACTIVATE, ShowWindow, TranslateMessage, WM_DESTROY, WM_LBUTTONDOWN, WM_MOUSEACTIVATE,
    WM_PAINT, WM_QUIT, WM_RBUTTONDOWN, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::w;

use crate::options::Options;

pub const REPORT_FILE: &str = "avsync.json";
const SIDE: i32 = 400;
const RATE: u64 = 48_000;
/// The click: 5 ms of a fading 2 kHz tone.
const CLICK_FRAMES: u64 = 240;
const CLICK_LEVEL: f32 = 0.5;
const FLASH: Duration = Duration::from_millis(100);
/// The render buffer: 20 ms in 100 ns units, short so a written click reaches the mixer soon.
const BUFFER_100NS: i64 = 200_000;

static WHITE: AtomicBool = AtomicBool::new(false);

extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            let colour = if WHITE.load(Ordering::Relaxed) {
                0x00ff_ffff
            } else {
                0
            };
            // SAFETY: called on the window thread with a valid window; the brush is deleted right after use.
            unsafe {
                let dc = BeginPaint(hwnd, &mut paint);
                let brush = CreateSolidBrush(COLORREF(colour));
                let area = RECT {
                    left: 0,
                    top: 0,
                    right: SIDE,
                    bottom: SIDE,
                };
                FillRect(dc, &area, brush);
                let _ = DeleteObject(brush.into());
                let _ = EndPaint(hwnd, &paint);
            }
            LRESULT(0)
        }
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_DESTROY => {
            // SAFETY: plain message queue call on the window thread.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        // SAFETY: default handling with the arguments the system passed in.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Handles queued messages, returns false once the window asked to quit.
fn pump() -> bool {
    let mut msg = MSG::default();
    // SAFETY: the message structure is a live local; messages belong to this thread.
    unsafe {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            if msg.message == WM_QUIT {
                return false;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    true
}

fn repaint(hwnd: HWND, white: bool) {
    WHITE.store(white, Ordering::Relaxed);
    // SAFETY: the window is valid; painting happens on this thread.
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
        let _ = UpdateWindow(hwnd);
    }
}

/// Sample `n` of the output: a click at the start of every second after the first.
fn sample(n: u64) -> f32 {
    let k = n % RATE;
    if n < RATE || k >= CLICK_FRAMES {
        return 0.0;
    }
    let fade = 1.0 - k as f32 / CLICK_FRAMES as f32;
    (k as f32 * 2.0 * std::f32::consts::PI * 2000.0 / RATE as f32).sin() * CLICK_LEVEL * fade
}

struct Render {
    client: IAudioClient,
    render: IAudioRenderClient,
    size: u32,
}

fn open_render() -> Result<Render, String> {
    let e = |what: &str, err: windows::core::Error| format!("{what}: {err}");
    // SAFETY: COM is initialised on this thread; plain WASAPI calls, the format lives through `Initialize`.
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|x| e("enumerator", x))?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .map_err(|x| e("default output", x))?;
        let client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .map_err(|x| e("activate", x))?;
        let format = WAVEFORMATEX {
            wFormatTag: 3,
            nChannels: 2,
            nSamplesPerSec: RATE as u32,
            nAvgBytesPerSec: RATE as u32 * 8,
            nBlockAlign: 8,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                BUFFER_100NS,
                0,
                &format,
                None,
            )
            .map_err(|x| e("initialize", x))?;
        let render: IAudioRenderClient = client.GetService().map_err(|x| e("render client", x))?;
        let size = client.GetBufferSize().map_err(|x| e("buffer size", x))?;
        Ok(Render {
            client,
            render,
            size,
        })
    }
}

pub fn run(options: &Options) -> Result<(), String> {
    let _ = dxgi::set_dpi_awareness();
    fs::create_dir_all(&options.out)
        .map_err(|e| format!("cannot create {}: {e}", options.out.display()))?;
    // SAFETY: COM for this thread, left at the end of `run`.
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
        .ok()
        .map_err(|e| format!("COM: {e}"))?;
    let result = flash_and_click(options);
    // SAFETY: pairs with CoInitializeEx above; every COM object of the run is dropped by now.
    unsafe { CoUninitialize() };
    let (flashes, heard) = result?;
    // Each flash against the first click heard at the loopback after it, at most half a second on: by the mixer
    // position the agent takes as capture time, and by the moment the samples could be read.
    let ms = |a: Instant, b: Instant| {
        if a >= b {
            a.duration_since(b).as_secs_f64() * 1000.0
        } else {
            -(b.duration_since(a).as_secs_f64() * 1000.0)
        }
    };
    let pairs: Vec<(f64, f64)> = flashes
        .iter()
        .filter_map(|f| {
            heard
                .iter()
                .find(|(c, _)| {
                    *c + Duration::from_millis(100) >= *f && *c < *f + Duration::from_millis(500)
                })
                .map(|(c, read)| (ms(*c, *f), ms(*read, *f)))
        })
        .collect();
    let spread = |mut v: Vec<f64>| {
        v.sort_by(f64::total_cmp);
        let at = |q: f64| {
            v.get(((v.len() as f64 * q) as usize).min(v.len().saturating_sub(1)))
                .copied()
        };
        json!({ "p50": at(0.5), "p95": at(0.95), "min": v.first(), "max": v.last() })
    };
    let skews = pairs.iter().map(|p| p.0).collect::<Vec<_>>();
    let reads = pairs.iter().map(|p| p.1).collect::<Vec<_>>();
    let report = json!({
        "clicks": flashes.len(),
        "heard_at_loopback": heard.len(),
        "pairs": pairs.len(),
        "loopback_after_flash_ms": spread(skews),
        "read_after_flash_ms": spread(reads),
        "seconds": options.seconds,
        "side_px": SIDE,
    });
    let path = options.out.join(REPORT_FILE);
    fs::write(&path, report.to_string()).map_err(|e| format!("{}: {e}", path.display()))?;
    println!("{report}");
    Ok(())
}

/// Onsets of clicks at the loopback of the default device, timed by the mixer position like the agent's capture.
fn listen(stop: Arc<AtomicBool>) -> std::thread::JoinHandle<Vec<(Instant, Instant)>> {
    std::thread::spawn(move || {
        let mut heard = Vec::new();
        let Ok(_com) = rcs_audio::ComApartment::enter() else {
            return heard;
        };
        let Ok(mut loopback) = rcs_audio::Loopback::open() else {
            return heard;
        };
        let quiet = RATE as usize * 3 / 10;
        let mut since_loud = quiet;
        while !stop.load(Ordering::Relaxed) {
            let _ = loopback.read(|samples, frames, at| {
                for k in 0..frames {
                    let v = samples.map_or(0.0, |s| s[2 * k].abs().max(s[2 * k + 1].abs()));
                    if v > 0.2 {
                        if since_loud >= quiet {
                            heard.push((
                                at + Duration::from_nanos(k as u64 * 1_000_000_000 / RATE),
                                Instant::now(),
                            ));
                        }
                        since_loud = 0;
                    } else {
                        since_loud += 1;
                    }
                }
            });
            std::thread::sleep(Duration::from_millis(2));
        }
        heard
    })
}

type Heard = Vec<(Instant, Instant)>;

fn flash_and_click(options: &Options) -> Result<(Vec<Instant>, Heard), String> {
    // SAFETY: plain Win32 calls; the class name and the window procedure live for the whole process.
    let hwnd = unsafe {
        let instance = GetModuleHandleW(None).map_err(|e| e.to_string())?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: w!("RcsBenchAvSync"),
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 {
            return Err("cannot register the window class".into());
        }
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            w!("RcsBenchAvSync"),
            w!("rcs-bench avsync"),
            WS_POPUP,
            0,
            0,
            SIDE,
            SIDE,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .map_err(|e| format!("cannot create the window: {e}"))?;
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        let _ = UpdateWindow(hwnd);
        hwnd
    };
    let stop = Arc::new(AtomicBool::new(false));
    let listener = listen(stop.clone());
    let out = open_render();
    let flashes = out.and_then(|r| play(&r, hwnd, options.seconds));
    stop.store(true, Ordering::Relaxed);
    let heard = listener.join().unwrap_or_default();
    // SAFETY: the window belongs to this thread.
    let _ = unsafe { DestroyWindow(hwnd) };
    flashes.map(|f| (f, heard))
}

/// Plays the clicks and flashes the square; returns when each flash was drawn.
fn play(r: &Render, hwnd: HWND, seconds: u32) -> Result<Vec<Instant>, String> {
    let e = |what: &str, err: windows::core::Error| format!("{what}: {err}");
    // SAFETY: the client is initialised.
    unsafe { r.client.Start() }.map_err(|x| e("start", x))?;
    let duration = Duration::from_secs(u64::from(seconds));
    let started = Instant::now();
    let mut written = 0u64;
    let mut next_click = RATE;
    let mut flashes = Vec::new();
    let mut flash_off: Option<Instant> = None;
    while started.elapsed() < duration && pump() {
        // SAFETY: the stream is started; the buffer is ours between GetBuffer and ReleaseBuffer and holds
        // `free` frames of 2 x f32.
        let padding = unsafe {
            let padding = r.client.GetCurrentPadding().map_err(|x| e("padding", x))?;
            let free = r.size.saturating_sub(padding);
            if free > 0 {
                let data = r.render.GetBuffer(free).map_err(|x| e("buffer", x))? as *mut f32;
                let buffer = std::slice::from_raw_parts_mut(data, free as usize * 2);
                for (i, frame) in buffer.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                    let v = sample(written + i as u64);
                    frame[0] = v;
                    frame[1] = v;
                }
                r.render
                    .ReleaseBuffer(free, 0)
                    .map_err(|x| e("release", x))?;
                written += u64::from(free);
            }
            r.client.GetCurrentPadding().map_err(|x| e("padding", x))?
        };
        // The mixer has taken everything but the padding: once that passes the click, the click is being mixed.
        let mixed = written - u64::from(padding);
        if mixed > next_click {
            repaint(hwnd, true);
            let drawn = Instant::now();
            flashes.push(drawn);
            flash_off = Some(drawn + FLASH);
            next_click += RATE;
        }
        if flash_off.is_some_and(|t| Instant::now() >= t) {
            flash_off = None;
            repaint(hwnd, false);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    // SAFETY: stopping the started stream.
    let _ = unsafe { r.client.Stop() };
    Ok(flashes)
}
