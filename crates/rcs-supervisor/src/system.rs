//! Windows side of the supervisor (D-025): the console session, starting the agent as SYSTEM in it, stopping it
//! through a named event, a job object so the agent never outlives the supervisor, and the Windows event log.

use std::os::windows::io::AsRawHandle;
use std::path::PathBuf;
use std::process::Child;
use std::time::Duration;

use tracing::{info, warn};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityImpersonation, SetTokenInformation, TOKEN_ACCESS_MASK,
    TOKEN_ADJUST_DEFAULT, TOKEN_ADJUST_SESSIONID, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE,
    TOKEN_QUERY, TokenPrimary, TokenSessionId,
};
use windows::Win32::System::EventLog::{
    DeregisterEventSource, EVENTLOG_ERROR_TYPE, EVENTLOG_INFORMATION_TYPE, EVENTLOG_WARNING_TYPE,
    REPORT_EVENT_TYPE, RegisterEventSourceW, ReportEventW,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateEventW, CreateProcessAsUserW,
    GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, PROCESS_INFORMATION, ResetEvent,
    STARTUPINFOW, SetEvent, TerminateProcess, WaitForSingleObject,
};
use windows::core::{HSTRING, PCWSTR, PWSTR};

use crate::supervise::{Agent, Event, Platform};

/// An owned kernel handle.
pub struct Owned(pub HANDLE);

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: the handle is owned by this value and closed once.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

// SAFETY: kernel handles may be used from any thread.
unsafe impl Send for Owned {}

/// The session of the physical console, None while Windows is switching it.
pub fn console_session() -> Option<u32> {
    // SAFETY: no pointers.
    let id = unsafe { WTSGetActiveConsoleSessionId() };
    (id != u32::MAX).then_some(id)
}

/// A command line for `CreateProcess`: arguments with spaces or quotes are quoted the way the C runtime splits them.
pub fn command_line(exe: &str, args: &[String]) -> String {
    let quote = |a: &str| {
        if !a.is_empty() && !a.contains([' ', '\t', '"']) {
            return a.to_string();
        }
        let mut out = String::from('"');
        let mut slashes = 0;
        for c in a.chars() {
            match c {
                '\\' => slashes += 1,
                '"' => {
                    out.extend(std::iter::repeat_n('\\', slashes * 2 + 1));
                    out.push('"');
                    slashes = 0;
                }
                _ => {
                    out.extend(std::iter::repeat_n('\\', slashes));
                    out.push(c);
                    slashes = 0;
                }
            }
        }
        out.extend(std::iter::repeat_n('\\', slashes * 2));
        out.push('"');
        out
    };
    std::iter::once(quote(exe))
        .chain(args.iter().map(|a| quote(a)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The manual-reset event the agent waits on to stop (`--stop-event`).
pub struct StopEvent {
    pub name: String,
    handle: Owned,
}

impl StopEvent {
    pub fn create(name: &str) -> Result<Self, String> {
        // SAFETY: the name outlives the call; default security: SYSTEM and administrators for a service.
        let handle = unsafe { CreateEventW(None, true, false, &HSTRING::from(name)) }
            .map_err(|e| format!("stop event {name}: {e}"))?;
        Ok(Self {
            name: name.into(),
            handle: Owned(handle),
        })
    }

    fn reset(&self) {
        // SAFETY: the handle is a live event.
        let _ = unsafe { ResetEvent(self.handle.0) };
    }

    fn set(&self) {
        // SAFETY: the handle is a live event.
        let _ = unsafe { SetEvent(self.handle.0) };
    }
}

/// A job that ends its processes when the supervisor's handle closes, also when the supervisor crashes.
pub fn kill_on_close_job() -> Result<Owned, String> {
    // SAFETY: no name, default security.
    let job = Owned(unsafe { CreateJobObjectW(None, None) }.map_err(|e| format!("job: {e}"))?);
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: the structure is the one this information class takes, with its size.
    unsafe {
        SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    }
    .map_err(|e| format!("job limits: {e}"))?;
    Ok(job)
}

/// One line in the Windows event log, for threads that do not own the platform.
pub fn report_text(id: u32, text: &str) {
    // SAFETY: the source name is a static string; the handle is deregistered below.
    let Ok(source) = (unsafe { RegisterEventSourceW(None, &HSTRING::from("RCS Supervisor")) })
    else {
        return;
    };
    let wide = HSTRING::from(text);
    let strings = [PCWSTR(wide.as_ptr())];
    // SAFETY: the source handle is live; the string array and its string outlive the call.
    let _ = unsafe {
        ReportEventW(
            source,
            EVENTLOG_INFORMATION_TYPE,
            0,
            id,
            None,
            0,
            Some(&strings),
            None,
        )
    };
    // SAFETY: the handle came from RegisterEventSourceW and is deregistered once.
    let _ = unsafe { DeregisterEventSource(source) };
}

/// A running agent: its process handle and the stop event it watches.
pub struct ProcessAgent {
    process: HANDLE,
    /// Keeps the process handle of a `Command` child alive; None for `CreateProcessAsUserW`, where `owned` does.
    _child: Option<Child>,
    _owned: Option<Owned>,
    stop: *const StopEvent,
}

// SAFETY: the stop event pointer refers to the platform, which outlives every agent it starts and is only used from
// the supervision thread.
unsafe impl Send for ProcessAgent {}

impl Agent for ProcessAgent {
    fn exited(&mut self) -> Option<u32> {
        // SAFETY: the handle is a live process handle.
        if unsafe { WaitForSingleObject(self.process, 0) } != WAIT_OBJECT_0 {
            return None;
        }
        let mut code = 0;
        // SAFETY: the out pointer refers to a live local.
        let _ = unsafe { GetExitCodeProcess(self.process, &mut code) };
        Some(code)
    }

    fn stop(&mut self, grace: Duration) {
        // SAFETY: the platform outlives the agent (see the Send impl).
        unsafe { &*self.stop }.set();
        let ms = grace.as_millis().min(u128::from(u32::MAX)) as u32;
        // SAFETY: the handle is a live process handle.
        if unsafe { WaitForSingleObject(self.process, ms) } != WAIT_OBJECT_0 {
            warn!("agent did not stop in time, ending it");
            // SAFETY: the handle has PROCESS_TERMINATE from creation.
            let _ = unsafe { TerminateProcess(self.process, 1) };
        }
    }
}

/// How the agent is started.
pub enum Launch {
    /// A service: SYSTEM in the console session.
    System,
    /// Development: the current user, this session, no service needed.
    CurrentUser,
}

pub struct WindowsPlatform {
    pub agent_exe: PathBuf,
    pub agent_args: Vec<String>,
    pub launch: Launch,
    stop: StopEvent,
    job: Owned,
    event_log: Option<HANDLE>,
}

impl WindowsPlatform {
    pub fn new(
        agent_exe: PathBuf,
        agent_args: Vec<String>,
        launch: Launch,
    ) -> Result<Self, String> {
        let name = match launch {
            Launch::System => r"Global\RcsAgentStop",
            Launch::CurrentUser => r"Local\RcsAgentStop",
        };
        // SAFETY: the source name is a static wide string; a missing registration still logs.
        let event_log =
            unsafe { RegisterEventSourceW(None, &HSTRING::from("RCS Supervisor")) }.ok();
        Ok(Self {
            agent_exe,
            agent_args,
            launch,
            stop: StopEvent::create(name)?,
            job: kill_on_close_job()?,
            event_log,
        })
    }

    fn args(&self) -> Vec<String> {
        let mut args = self.agent_args.clone();
        args.push("--stop-event".into());
        args.push(self.stop.name.clone());
        args
    }

    fn launch_as_system(&self, session: u32) -> Result<ProcessAgent, String> {
        let exe = self.agent_exe.to_string_lossy().to_string();
        let mut own = HANDLE::default();
        let access = TOKEN_ACCESS_MASK(
            TOKEN_DUPLICATE.0
                | TOKEN_QUERY.0
                | TOKEN_ADJUST_DEFAULT.0
                | TOKEN_ADJUST_SESSIONID.0
                | TOKEN_ASSIGN_PRIMARY.0,
        );
        // SAFETY: the pseudo handle needs no closing; the out pointer refers to a live local.
        unsafe { OpenProcessToken(GetCurrentProcess(), access, &mut own) }
            .map_err(|e| format!("own token: {e}"))?;
        let own = Owned(own);
        let mut token = HANDLE::default();
        // SAFETY: the source token is live; the out pointer refers to a live local.
        unsafe {
            DuplicateTokenEx(
                own.0,
                TOKEN_ACCESS_MASK(0x0200_0000), // MAXIMUM_ALLOWED
                None,
                SecurityImpersonation,
                TokenPrimary,
                &mut token,
            )
        }
        .map_err(|e| format!("duplicate token: {e}"))?;
        let token = Owned(token);
        // SAFETY: the value is a live u32 with its size, as TokenSessionId takes; needs SeTcbPrivilege (SYSTEM).
        unsafe {
            SetTokenInformation(
                token.0,
                TokenSessionId,
                (&raw const session).cast(),
                size_of::<u32>() as u32,
            )
        }
        .map_err(|e| format!("token session {session}: {e}"))?;
        let mut desktop: Vec<u16> = "winsta0\\default\0".encode_utf16().collect();
        let startup = STARTUPINFOW {
            cb: size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            ..Default::default()
        };
        let mut line: Vec<u16> = command_line(&exe, &self.args())
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let exe_w = HSTRING::from(exe.as_str());
        let mut info = PROCESS_INFORMATION::default();
        // SAFETY: every string is NUL-terminated and outlives the call; the command line buffer is writable as the
        // API requires; the out pointer refers to a live local.
        unsafe {
            CreateProcessAsUserW(
                Some(token.0),
                PCWSTR(exe_w.as_ptr()),
                Some(PWSTR(line.as_mut_ptr())),
                None,
                None,
                false,
                CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
                None,
                None,
                &startup,
                &mut info,
            )
        }
        .map_err(|e| format!("start agent in session {session}: {e}"))?;
        let _thread = Owned(info.hThread);
        let process = Owned(info.hProcess);
        // SAFETY: both handles are live.
        if let Err(e) = unsafe { AssignProcessToJobObject(self.job.0, process.0) } {
            warn!(error = %e, "agent not in the job");
        }
        Ok(ProcessAgent {
            process: process.0,
            _child: None,
            _owned: Some(process),
            stop: &raw const self.stop,
        })
    }

    fn launch_as_user(&self) -> Result<ProcessAgent, String> {
        let child = std::process::Command::new(&self.agent_exe)
            .args(self.args())
            .spawn()
            .map_err(|e| format!("start agent: {e}"))?;
        let process = HANDLE(child.as_raw_handle());
        // SAFETY: both handles are live.
        if let Err(e) = unsafe { AssignProcessToJobObject(self.job.0, process) } {
            warn!(error = %e, "agent not in the job");
        }
        Ok(ProcessAgent {
            process,
            _child: Some(child),
            _owned: None,
            stop: &raw const self.stop,
        })
    }
}

impl Drop for WindowsPlatform {
    fn drop(&mut self) {
        if let Some(h) = self.event_log.take() {
            // SAFETY: the handle came from RegisterEventSourceW and is deregistered once.
            let _ = unsafe { DeregisterEventSource(h) };
        }
    }
}

impl Platform for WindowsPlatform {
    type Agent = ProcessAgent;

    fn console_session(&mut self) -> Option<u32> {
        match self.launch {
            Launch::System => console_session(),
            // The current user's own session, whatever is on the console.
            Launch::CurrentUser => Some(0),
        }
    }

    fn launch(&mut self, session: u32) -> Result<ProcessAgent, String> {
        self.stop.reset();
        match self.launch {
            Launch::System => self.launch_as_system(session),
            Launch::CurrentUser => self.launch_as_user(),
        }
    }

    fn report(&mut self, event: &Event) {
        let (kind, id, text): (REPORT_EVENT_TYPE, u32, String) = match event {
            Event::Started { session } => (
                EVENTLOG_INFORMATION_TYPE,
                1,
                format!("Агент запущен в сессии {session}."),
            ),
            Event::Exited {
                session,
                code,
                ran,
                next_in,
            } => (
                EVENTLOG_WARNING_TYPE,
                2,
                format!(
                    "Агент в сессии {session} завершился с кодом {code} через {} с, перезапуск через {} с.",
                    ran.as_secs(),
                    next_in.as_secs()
                ),
            ),
            Event::LaunchFailed {
                session,
                error,
                next_in,
            } => (
                EVENTLOG_ERROR_TYPE,
                3,
                format!(
                    "Агент не запустился в сессии {session}: {error}. Повтор через {} с.",
                    next_in.as_secs()
                ),
            ),
            Event::SessionChanged { from, to } => (
                EVENTLOG_INFORMATION_TYPE,
                4,
                format!("Консольная сессия сменилась: {from} → {to}, агент перезапускается."),
            ),
            Event::Stopped => (
                EVENTLOG_INFORMATION_TYPE,
                5,
                "Супервизор остановлен.".into(),
            ),
        };
        info!(event = ?event, "supervisor");
        if let Some(h) = self.event_log {
            let wide = HSTRING::from(text.as_str());
            let strings = [PCWSTR(wide.as_ptr())];
            // SAFETY: the source handle is live; the string array and its string outlive the call.
            let _ = unsafe { ReportEventW(h, kind, 0, id, None, 0, Some(&strings), None) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_lines_quote_like_the_c_runtime() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            command_line(
                r"C:\Program Files\RCS\rcs-agent.exe",
                &args(&["--udp", "0.0.0.0:8443"])
            ),
            r#""C:\Program Files\RCS\rcs-agent.exe" --udp 0.0.0.0:8443"#
        );
        assert_eq!(
            command_line("a.exe", &args(&["", r#"say "hi""#, r"dir\"])),
            r#"a.exe "" "say \"hi\"" dir\"#
        );
        assert_eq!(
            command_line("a.exe", &args(&[r"C:\a b\"])),
            r#"a.exe "C:\a b\\""#
        );
    }

    #[test]
    fn there_is_a_console_session() {
        assert!(console_session().is_some());
    }
}
