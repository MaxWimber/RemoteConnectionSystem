//! `rcs-bench load`: screen content for the capture scenarios.
//!
//! A topmost popup window that never takes focus. `still` covers the screen with a fixed test pattern and does not
//! redraw it. `window` moves a 960x600 window with changing content along a curve once per desktop composition, like
//! a window being dragged. A click on the window ends it early. The result goes to `load.json` in the output folder.

use std::cell::RefCell;
use std::fs;
use std::time::{Duration, Instant};

use rcs_capture::dxgi;
use serde_json::json;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, CreateSolidBrush,
    DeleteDC, DeleteObject, EndPaint, FillRect, GetDC, HBITMAP, HBRUSH, HDC, HFONT, HGDIOBJ,
    InvalidateRect, PAINTSTRUCT, ReleaseDC, SRCCOPY, SelectObject, SetBkMode, SetTextColor,
    TRANSPARENT, TextOutW, UpdateWindow,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetSystemMetrics,
    HWND_TOPMOST, IDC_ARROW, LoadCursorW, MA_NOACTIVATE, MSG, PM_REMOVE, PeekMessageW,
    PostQuitMessage, RegisterClassW, SM_CXSCREEN, SM_CYSCREEN, SW_SHOWNOACTIVATE, SWP_NOACTIVATE,
    SWP_NOSIZE, SetWindowPos, ShowWindow, TranslateMessage, WM_DESTROY, WM_LBUTTONDOWN,
    WM_MOUSEACTIVATE, WM_PAINT, WM_QUIT, WM_RBUTTONDOWN, WNDCLASSW, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::w;

use crate::options::Options;

pub const REPORT_FILE: &str = "load.json";
const WINDOW_SIZE: (i32, i32) = (960, 600);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadKind {
    Still,
    Window,
}

impl LoadKind {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "still" => Some(Self::Still),
            "window" => Some(Self::Window),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Still => "still",
            Self::Window => "window",
        }
    }
}

/// 75% colour bars, as in the static test pattern.
const BARS: [u32; 7] = [
    rgb(191, 191, 191),
    rgb(191, 191, 0),
    rgb(0, 191, 191),
    rgb(0, 191, 0),
    rgb(191, 0, 191),
    rgb(191, 0, 0),
    rgb(0, 0, 191),
];

const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
}

/// Back buffer drawn by the main loop and copied to the window on `WM_PAINT`.
struct Canvas {
    dc: HDC,
    bitmap: HBITMAP,
    old_bitmap: HGDIOBJ,
    font: HFONT,
    black: HBRUSH,
    white: HBRUSH,
    grey: HBRUSH,
    width: i32,
    height: i32,
}

thread_local! {
    static CANVAS: RefCell<Option<Canvas>> = const { RefCell::new(None) };
}

fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
    RECT {
        left,
        top,
        right,
        bottom,
    }
}

impl Canvas {
    fn new(hwnd: HWND, width: i32, height: i32) -> Result<Self, String> {
        // SAFETY: the window is valid for the whole call; every handle created here is released in `Drop`.
        unsafe {
            let window_dc = GetDC(Some(hwnd));
            let dc = CreateCompatibleDC(Some(window_dc));
            let bitmap = CreateCompatibleBitmap(window_dc, width, height);
            ReleaseDC(Some(hwnd), window_dc);
            if dc.is_invalid() || bitmap.is_invalid() {
                return Err("cannot create the back buffer".into());
            }
            let old_bitmap = SelectObject(dc, bitmap.into());
            let font = CreateFontW(
                56,
                0,
                0,
                0,
                700,
                0,
                0,
                0,
                Default::default(),
                Default::default(),
                Default::default(),
                Default::default(),
                0,
                w!("Consolas"),
            );
            SelectObject(dc, font.into());
            SetBkMode(dc, TRANSPARENT);
            SetTextColor(dc, COLORREF(0));
            Ok(Self {
                dc,
                bitmap,
                old_bitmap,
                font,
                black: CreateSolidBrush(COLORREF(0)),
                white: CreateSolidBrush(COLORREF(rgb(255, 255, 255))),
                grey: CreateSolidBrush(COLORREF(BARS[0])),
                width,
                height,
            })
        }
    }

    fn fill(&self, area: RECT, brush: HBRUSH) {
        // SAFETY: the DC and the brush belong to this canvas and are alive.
        unsafe { FillRect(self.dc, &area, brush) };
    }

    /// Bars on the top two thirds, a grey ramp below them, a black band at the bottom.
    fn draw_background(&self) {
        let (w, h) = (self.width, self.height);
        let bar = w / BARS.len() as i32;
        for (i, &colour) in BARS.iter().enumerate() {
            let left = i as i32 * bar;
            let right = if i == BARS.len() - 1 { w } else { left + bar };
            // SAFETY: the brush lives until DeleteObject below.
            let brush = unsafe { CreateSolidBrush(COLORREF(colour)) };
            self.fill(rect(left, 0, right, h * 2 / 3), brush);
            // SAFETY: the brush is no longer selected anywhere.
            let _ = unsafe { DeleteObject(brush.into()) };
        }
        let mut x = 0;
        while x < w {
            let v = (x * 255 / (w - 1).max(1)) as u8;
            // SAFETY: as above.
            let brush = unsafe { CreateSolidBrush(COLORREF(rgb(v, v, v))) };
            self.fill(rect(x, h * 2 / 3, x + 2, h * 5 / 6), brush);
            // SAFETY: as above.
            let _ = unsafe { DeleteObject(brush.into()) };
            x += 2;
        }
        self.fill(rect(0, h * 5 / 6, w, h), self.black);
    }

    /// The counter in the top left corner and a square crossing the bottom band.
    fn draw_dynamic(&self, frame: u64) {
        let (w, h) = (self.width, self.height);
        self.fill(rect(0, 0, w.min(560), 96), self.grey);
        let text: Vec<u16> = format!("RCS load {frame}").encode_utf16().collect();
        // SAFETY: the DC is alive and has the font selected.
        let _ = unsafe { TextOutW(self.dc, 24, 20, &text) };
        let band = rect(0, h * 5 / 6, w, h);
        self.fill(band, self.black);
        let side = (band.bottom - band.top - 16).max(8);
        let x = ((frame as i32).wrapping_mul(16)).rem_euclid((w - side).max(1));
        self.fill(
            rect(x, band.top + 8, x + side, band.top + 8 + side),
            self.white,
        );
    }
}

impl Drop for Canvas {
    fn drop(&mut self) {
        // SAFETY: the handles were created in `new` and are released once.
        unsafe {
            SelectObject(self.dc, self.old_bitmap);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteObject(self.font.into());
            let _ = DeleteObject(self.black.into());
            let _ = DeleteObject(self.white.into());
            let _ = DeleteObject(self.grey.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            // SAFETY: called on the window thread with a valid window.
            let dc = unsafe { BeginPaint(hwnd, &mut paint) };
            CANVAS.with_borrow(|canvas| {
                if let Some(c) = canvas {
                    // SAFETY: both DCs are alive for the call.
                    let _ =
                        unsafe { BitBlt(dc, 0, 0, c.width, c.height, Some(c.dc), 0, 0, SRCCOPY) };
                }
            });
            // SAFETY: pairs with BeginPaint above.
            let _ = unsafe { EndPaint(hwnd, &paint) };
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

pub fn run(options: &Options) -> Result<(), String> {
    let name = options.kind.as_deref().unwrap_or("window");
    let kind = LoadKind::parse(name).ok_or_else(|| format!("unknown load kind: {name}"))?;
    let _ = dxgi::set_dpi_awareness();
    fs::create_dir_all(&options.out)
        .map_err(|e| format!("cannot create {}: {e}", options.out.display()))?;

    // SAFETY: plain Win32 calls; the class name and the window procedure live for the whole process.
    let (hwnd, screen) = unsafe {
        let instance = GetModuleHandleW(None).map_err(|e| e.to_string())?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: w!("RcsBenchLoad"),
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 {
            return Err("cannot register the window class".into());
        }
        let screen = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
        let size = match kind {
            LoadKind::Still => screen,
            LoadKind::Window => WINDOW_SIZE,
        };
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            w!("RcsBenchLoad"),
            w!("rcs-bench load"),
            WS_POPUP,
            0,
            0,
            size.0,
            size.1,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .map_err(|e| format!("cannot create the window: {e}"))?;
        (hwnd, screen)
    };
    let (width, height) = match kind {
        LoadKind::Still => screen,
        LoadKind::Window => WINDOW_SIZE,
    };
    let canvas = Canvas::new(hwnd, width, height)?;
    canvas.draw_background();
    canvas.draw_dynamic(0);
    CANVAS.set(Some(canvas));
    // SAFETY: the window is valid.
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        let _ = UpdateWindow(hwnd);
    }

    let duration = Duration::from_secs(u64::from(options.seconds));
    let started = Instant::now();
    let mut frames = 0u64;
    let mut flush_failures = 0u64;
    let mut closed_by_click = false;
    while started.elapsed() < duration {
        if !pump() {
            closed_by_click = true;
            break;
        }
        match kind {
            LoadKind::Still => std::thread::sleep(Duration::from_millis(50)),
            LoadKind::Window => {
                frames += 1;
                let t = started.elapsed().as_secs_f64();
                let (free_x, free_y) = (f64::from(screen.0 - width), f64::from(screen.1 - height));
                let x = (free_x / 2.0 * (1.0 + (t * 1.7).sin())) as i32;
                let y = (free_y / 2.0 * (1.0 + (t * 1.1 + 0.5).sin())) as i32;
                CANVAS.with_borrow(|c| {
                    if let Some(c) = c {
                        c.draw_dynamic(frames);
                    }
                });
                // SAFETY: the window is valid; the calls run on its thread.
                unsafe {
                    let _ = SetWindowPos(
                        hwnd,
                        Some(HWND_TOPMOST),
                        x,
                        y,
                        0,
                        0,
                        SWP_NOSIZE | SWP_NOACTIVATE,
                    );
                    let _ = InvalidateRect(Some(hwnd), None, false);
                    let _ = UpdateWindow(hwnd);
                }
                // One step per desktop composition, so the window moves at the display rate.
                // SAFETY: no arguments.
                if unsafe { DwmFlush() }.is_err() {
                    flush_failures += 1;
                    std::thread::sleep(Duration::from_millis(16));
                }
            }
        }
    }
    let elapsed = started.elapsed().as_secs_f64();
    // SAFETY: the window is valid and destroyed once.
    let _ = unsafe { DestroyWindow(hwnd) };
    CANVAS.set(None);

    let report = json!({
        "kind": kind.name(),
        "seconds": (elapsed * 1000.0).round() / 1000.0,
        "frames": frames,
        "fps": if elapsed > 0.0 { ((frames as f64 / elapsed) * 100.0).round() / 100.0 } else { 0.0 },
        "dwm_flush_failures": flush_failures,
        "closed_by_click": closed_by_click,
        "window": { "width": width, "height": height },
        "screen": { "width": screen.0, "height": screen.1 },
    });
    let path = options.out.join(REPORT_FILE);
    let text = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
    fs::write(&path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    if closed_by_click {
        return Err("the load window was closed by a click".into());
    }
    Ok(())
}
