//! The clipboard thread (D-040): the Windows clipboard to the browser and back, text only.
//!
//! A message-only window listens for clipboard changes (`AddClipboardFormatListener`). While a browser has its
//! `clipboard` channel open, every change that this thread did not make itself goes to the browser. Texts from the
//! browser are written here and acknowledged, so the client can send the paste keys after them. A text that already
//! went either way is not sent again: a browser on this same computer writes the text it got back into this
//! clipboard, and that change must not start a loop. The content is never
//! logged, only its size. No network or disk work happens on this thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::thread::JoinHandle;
use std::time::Duration;

use rcs_protocol::clipboard::MAX_TEXT;
use rcs_webrtc::FrameSink;
use tracing::{info, warn};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData,
    GetClipboardSequenceNumber, IsClipboardFormatAvailable, OpenClipboard,
    RemoveClipboardFormatListener, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, HWND_MESSAGE, MSG,
    MsgWaitForMultipleObjects, PM_REMOVE, PeekMessageW, QS_ALLINPUT, RegisterClassW,
    TranslateMessage, WINDOW_EX_STYLE, WM_CLIPBOARDUPDATE, WNDCLASSW, WS_OVERLAPPED,
};
use windows::core::w;

/// How long the thread sleeps when nothing happens; a text from the browser waits at most this long.
const WAIT_MS: u32 = 10;
/// Another program may hold the clipboard for a moment.
const OPEN_TRIES: u32 = 20;

/// Set by the window procedure on `WM_CLIPBOARDUPDATE`; only the clipboard thread has that window.
static CHANGED: AtomicBool = AtomicBool::new(false);

pub fn spawn(
    sink: FrameSink,
    incoming: Receiver<String>,
    enabled: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("clipboard".into())
        .spawn(move || run(&sink, &incoming, &enabled, &stop))
}

extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_CLIPBOARDUPDATE {
        CHANGED.store(true, Ordering::Relaxed);
        return LRESULT(0);
    }
    // SAFETY: default handling with the arguments the system passed in.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn create_window() -> windows::core::Result<HWND> {
    // SAFETY: plain Win32 calls; the class name and the window procedure live for the whole process.
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: w!("RcsAgentClipboard"),
            ..Default::default()
        };
        RegisterClassW(&class);
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("RcsAgentClipboard"),
            w!("rcs-agent clipboard"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(instance.into()),
            None,
        )
    }
}

/// Handles queued messages of this thread.
fn pump() {
    let mut msg = MSG::default();
    // SAFETY: the message structure is a live local; messages belong to this thread.
    unsafe {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn run(sink: &FrameSink, incoming: &Receiver<String>, enabled: &AtomicBool, stop: &AtomicBool) {
    let hwnd = match create_window() {
        Ok(h) => h,
        Err(e) => {
            warn!(error = %e, "clipboard: no window, no clipboard sharing");
            return;
        }
    };
    // SAFETY: the window belongs to this thread and lives until the end of `run`.
    if let Err(e) = unsafe { AddClipboardFormatListener(hwnd) } {
        warn!(error = %e, "clipboard: no change notifications");
    }
    let mut own_sequence = 0u32;
    // The last text that went to the browser or came from it.
    let mut last = String::new();
    let mut was_enabled = false;
    while !stop.load(Ordering::Relaxed) {
        // SAFETY: waits on this thread's message queue only.
        unsafe { MsgWaitForMultipleObjects(None, false, WAIT_MS, QS_ALLINPUT) };
        pump();
        let on = enabled.load(Ordering::Relaxed);
        if on != was_enabled {
            info!(enabled = on, "clipboard sharing");
            was_enabled = on;
            // What was copied before the browser turned sharing on stays here.
            CHANGED.store(false, Ordering::Relaxed);
        }
        while let Ok(text) = incoming.try_recv() {
            if !on {
                continue;
            }
            let total = text.len() as u32;
            match write_text(hwnd, &text) {
                Ok(sequence) => {
                    own_sequence = sequence;
                    sink.clipboard_applied(total);
                    last = text;
                }
                Err(e) => warn!(error = %e, bytes = total, "clipboard not written"),
            }
        }
        if CHANGED.swap(false, Ordering::Relaxed) && on {
            // SAFETY: a plain query.
            if unsafe { GetClipboardSequenceNumber() } == own_sequence {
                continue;
            }
            match read_text(hwnd) {
                Ok(Some(text)) if text.len() > MAX_TEXT => {
                    info!(
                        bytes = text.len(),
                        "clipboard text over the limit, not sent"
                    );
                    sink.clipboard_too_large(text.len().min(u32::MAX as usize) as u32);
                }
                Ok(Some(text)) if same_text(&text, &last) => {}
                Ok(Some(text)) => {
                    last.clone_from(&text);
                    sink.send_clipboard(text.into_bytes());
                }
                Ok(None) => {}
                Err(e) => warn!(error = %e, "clipboard not read"),
            }
        }
    }
    // SAFETY: the window was created on this thread above.
    unsafe {
        let _ = RemoveClipboardFormatListener(hwnd);
        let _ = DestroyWindow(hwnd);
    }
}

/// The clipboard, open until dropped.
struct Opened;

impl Opened {
    fn open(hwnd: HWND) -> windows::core::Result<Self> {
        let mut last = None;
        for _ in 0..OPEN_TRIES {
            // SAFETY: the window belongs to this thread.
            match unsafe { OpenClipboard(Some(hwnd)) } {
                Ok(()) => return Ok(Self),
                Err(e) => last = Some(e),
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Err(last.unwrap_or_else(windows::core::Error::from_thread))
    }
}

impl Drop for Opened {
    fn drop(&mut self) {
        // SAFETY: this thread opened the clipboard in `open`.
        let _ = unsafe { CloseClipboard() };
    }
}

/// The clipboard's text, or `None` when it holds no text.
fn read_text(hwnd: HWND) -> windows::core::Result<Option<String>> {
    let _open = Opened::open(hwnd)?;
    // SAFETY: the clipboard is open on this thread; the handle it returns stays valid until it closes, and the
    // locked memory is read only within its size.
    unsafe {
        if IsClipboardFormatAvailable(u32::from(CF_UNICODETEXT.0)).is_err() {
            return Ok(None);
        }
        let handle = GetClipboardData(u32::from(CF_UNICODETEXT.0))?;
        let memory = HGLOBAL(handle.0);
        let units = GlobalSize(memory) / 2;
        let data = GlobalLock(memory) as *const u16;
        if data.is_null() {
            return Err(windows::core::Error::from_thread());
        }
        let wide = std::slice::from_raw_parts(data, units);
        let len = wide.iter().position(|&c| c == 0).unwrap_or(units);
        let text = String::from_utf16_lossy(&wide[..len]);
        let _ = GlobalUnlock(memory);
        Ok(Some(text))
    }
}

/// The same text, whatever its line ends: the browser may give back LF for the CRLF it got.
fn same_text(a: &str, b: &str) -> bool {
    a.replace("\r\n", "\n") == b.replace("\r\n", "\n")
}

/// Windows programs expect CRLF; texts from Android and Linux come with LF only.
fn windows_lines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\n', "\r\n")
}

/// Puts `text` into the clipboard and returns the clipboard's sequence number after it.
fn write_text(hwnd: HWND, text: &str) -> windows::core::Result<u32> {
    let wide: Vec<u16> = windows_lines(text).encode_utf16().chain([0]).collect();
    // SAFETY: the memory is allocated for the whole text and NUL, filled while locked, and handed to the clipboard,
    // which owns it from then on; on failure it is freed here.
    unsafe {
        let memory = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2)?;
        let data = GlobalLock(memory) as *mut u16;
        if data.is_null() {
            let _ = GlobalFree(Some(memory));
            return Err(windows::core::Error::from_thread());
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), data, wide.len());
        let _ = GlobalUnlock(memory);
        let written = Opened::open(hwnd).and_then(|open| {
            EmptyClipboard()?;
            SetClipboardData(u32::from(CF_UNICODETEXT.0), Some(HANDLE(memory.0)))?;
            drop(open);
            Ok(())
        });
        if let Err(e) = written {
            let _ = GlobalFree(Some(memory));
            return Err(e);
        }
        Ok(GetClipboardSequenceNumber())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_text_back_with_other_line_ends_is_the_same() {
        assert!(same_text("a\r\nb", "a\nb"));
        assert!(!same_text("a\nb", "a b"));
    }

    #[test]
    fn lines_become_crlf_once() {
        assert_eq!(windows_lines("a\nb\r\nc\n"), "a\r\nb\r\nc\r\n");
        assert_eq!(windows_lines("без переводов"), "без переводов");
    }
}
