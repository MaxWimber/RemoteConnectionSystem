//! Reading a line from the console, with the echo off for passwords.

use std::io::{BufRead, Write};

use windows::Win32::System::Console::{
    CONSOLE_MODE, ENABLE_ECHO_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE, SetConsoleMode,
};

/// Turns the echo back on when dropped, also on an error or a panic.
struct EchoOff {
    handle: windows::Win32::Foundation::HANDLE,
    mode: CONSOLE_MODE,
}

impl Drop for EchoOff {
    fn drop(&mut self) {
        // SAFETY: the handle is the process's standard input console, and `mode` is the mode read from it.
        unsafe {
            let _ = SetConsoleMode(self.handle, self.mode);
        }
    }
}

fn echo_off() -> Option<EchoOff> {
    // SAFETY: GetStdHandle has no preconditions; the handle is not owned and is not closed.
    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) }.ok()?;
    let mut mode = CONSOLE_MODE::default();
    // SAFETY: `mode` is a valid out pointer; the call fails when input is not a console, e.g. a pipe.
    unsafe { GetConsoleMode(handle, &mut mode) }.ok()?;
    // SAFETY: as above; the guard restores the previous mode.
    unsafe { SetConsoleMode(handle, mode & !ENABLE_ECHO_INPUT) }.ok()?;
    Some(EchoOff { handle, mode })
}

pub fn read_line(prompt: &str, hidden: bool) -> std::io::Result<String> {
    eprint!("{prompt}");
    std::io::stderr().flush()?;
    let guard = if hidden { echo_off() } else { None };
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    if guard.is_some() {
        eprintln!();
    }
    drop(guard);
    while line.ends_with(['\n', '\r']) {
        line.pop();
    }
    // Windows PowerShell 5.1 starts piped UTF-8 input with a byte order mark.
    Ok(line
        .strip_prefix('\u{feff}')
        .map(str::to_string)
        .unwrap_or(line))
}
