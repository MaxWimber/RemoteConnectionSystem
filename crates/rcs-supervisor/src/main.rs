//! Windows service that starts the agent in the console session (phase 6, D-025).
//!
//!     rcs-supervisor service -- AGENT_ARGS          what the service control manager runs
//!     rcs-supervisor run --as-user [--for S] -- AGENT_ARGS
//!                                                   development: the agent as the current user, no service
//!
//! The agent is `rcs-agent.exe` next to this program unless `--agent` names another.

mod sas;
mod supervise;
mod system;

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Mutex;
use std::sync::mpsc::{Sender, channel};
use std::time::{Duration, Instant};

use rcs_protocol::BUILD_ID;
use tracing::{Level, error, info};
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::{define_windows_service, service_dispatcher};

use crate::supervise::{Command, supervise};
use crate::system::{Launch, WindowsPlatform};

const SERVICE_NAME: &str = "rcs-supervisor";

struct Options {
    mode: String,
    agent: PathBuf,
    agent_args: Vec<String>,
    log: PathBuf,
    run_for: Option<Duration>,
}

fn program_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))
        .unwrap_or_default()
}

fn data_logs() -> PathBuf {
    std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join("RCS")
        .join("logs")
}

fn parse(args: &[String]) -> Result<Options, String> {
    let (own, agent_args) = match args.iter().position(|a| a == "--") {
        Some(i) => (&args[..i], args[i + 1..].to_vec()),
        None => (args, Vec::new()),
    };
    let mode = own.first().cloned().ok_or("service or run")?;
    let mut options = Options {
        agent: program_dir().join("rcs-agent.exe"),
        agent_args,
        log: if mode == "service" {
            data_logs()
        } else {
            PathBuf::from("out").join("supervisor")
        },
        run_for: None,
        mode,
    };
    let mut rest = own[1..].iter();
    let mut as_user = false;
    while let Some(flag) = rest.next() {
        match flag.as_str() {
            "--as-user" => as_user = true,
            "--agent" => options.agent = rest.next().ok_or("--agent needs a path")?.into(),
            "--log" => options.log = rest.next().ok_or("--log needs a directory")?.into(),
            "--for" => {
                let s: u64 = rest
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or("--for needs seconds")?;
                options.run_for = Some(Duration::from_secs(s));
            }
            other => return Err(format!("unknown option {other}")),
        }
    }
    match options.mode.as_str() {
        "service" => {}
        "run" if as_user => {}
        "run" => return Err("run needs --as-user: SYSTEM launches only work as a service".into()),
        other => return Err(format!("unknown mode {other}")),
    }
    Ok(options)
}

fn init_logging(dir: &PathBuf) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let path = dir.join("rcs-supervisor.jsonl");
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("cannot open log {}: {e}", path.display()))?;
    tracing_subscriber::fmt()
        .json()
        .with_max_level(Level::INFO)
        .with_writer(Mutex::new(file))
        .try_init()
        .map_err(|e| format!("cannot start logging: {e}"))?;
    std::panic::set_hook(Box::new(|info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        error!(panic = %info, %backtrace, "panic");
    }));
    Ok(())
}

fn run(
    options: &Options,
    launch: Launch,
    commands: &std::sync::mpsc::Receiver<Command>,
) -> Result<(), String> {
    info!(build = BUILD_ID, agent = %options.agent.display(), args = ?options.agent_args, "supervisor started");
    if matches!(launch, Launch::System) {
        // Ctrl+Alt+Del for the agent (P6-7); the thread ends with the process.
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        sas::serve(stop, |sent| {
            if sent {
                system::report_text(1, "Ctrl+Alt+Del отправлен по запросу агента.");
            }
        })
        .map_err(|e| format!("sas thread: {e}"))?;
    }
    let mut platform =
        WindowsPlatform::new(options.agent.clone(), options.agent_args.clone(), launch)?;
    supervise(&mut platform, commands, Instant::now);
    info!("supervisor stopped");
    Ok(())
}

define_windows_service!(ffi_service_main, service_main);

fn service_main(_scm_args: Vec<OsString>) {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let options = match parse(&args) {
        Ok(o) => o,
        Err(e) => {
            error!(error = %e, "bad service command line");
            return;
        }
    };
    if let Err(e) = run_service(&options) {
        error!(error = %e, "service failed");
    }
}

fn run_service(options: &Options) -> Result<(), String> {
    let (sender, commands) = channel();
    let control: Mutex<Sender<Command>> = Mutex::new(sender);
    let handler = move |event| -> ServiceControlHandlerResult {
        let send = |c| {
            if let Ok(s) = control.lock() {
                let _ = s.send(c);
            }
        };
        match event {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                send(Command::Stop);
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::SessionChange(_) => {
                send(Command::SessionChange);
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };
    let status =
        service_control_handler::register(SERVICE_NAME, handler).map_err(|e| e.to_string())?;
    let set = |state, accepted, code| {
        status.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: accepted,
            exit_code: ServiceExitCode::Win32(code),
            checkpoint: 0,
            wait_hint: Duration::from_secs(10),
            process_id: None,
        })
    };
    set(
        ServiceState::Running,
        ServiceControlAccept::STOP
            | ServiceControlAccept::SHUTDOWN
            | ServiceControlAccept::SESSION_CHANGE,
        0,
    )
    .map_err(|e| e.to_string())?;
    let result = run(options, Launch::System, &commands);
    let code = u32::from(result.is_err());
    let _ = set(ServiceState::Stopped, ServiceControlAccept::empty(), code);
    result
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let options = match parse(&args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!(
                "{e}\nUsage: rcs-supervisor service -- AGENT_ARGS | run --as-user [--for S] [--agent PATH] [--log DIR] -- AGENT_ARGS"
            );
            return ExitCode::from(2);
        }
    };
    if let Err(e) = init_logging(&options.log) {
        eprintln!("{e}");
        return ExitCode::FAILURE;
    }
    if options.mode == "service" {
        return match service_dispatcher::start(SERVICE_NAME, ffi_service_main) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                error!(error = %e, "not started by the service control manager");
                eprintln!("Ошибка: {e}. Этот режим запускает только диспетчер служб.");
                ExitCode::FAILURE
            }
        };
    }
    let (sender, commands) = channel();
    if let Some(after) = options.run_for {
        std::thread::spawn(move || {
            std::thread::sleep(after);
            let _ = sender.send(Command::Stop);
        });
    } else {
        // Keeps the channel open until the process is ended with Ctrl+C.
        std::mem::forget(sender);
    }
    match run(&options, Launch::CurrentUser, &commands) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!(error = %e, "supervisor failed");
            eprintln!("Ошибка: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn agent_arguments_follow_the_double_dash() {
        let o = parse(&args(&[
            "service",
            "--",
            "--udp",
            "0.0.0.0:8443",
            "--input",
            "sendinput",
        ]))
        .unwrap();
        assert_eq!(o.mode, "service");
        assert_eq!(
            o.agent_args,
            ["--udp", "0.0.0.0:8443", "--input", "sendinput"]
        );
        assert!(o.agent.ends_with("rcs-agent.exe"));
        let o = parse(&args(&[
            "run",
            "--as-user",
            "--for",
            "20",
            "--agent",
            "x.exe",
            "--",
            "-v",
        ]))
        .unwrap();
        assert_eq!(
            (o.run_for, o.agent, o.agent_args),
            (
                Some(Duration::from_secs(20)),
                PathBuf::from("x.exe"),
                args(&["-v"])
            )
        );
    }

    #[test]
    fn system_launches_need_the_service() {
        assert!(parse(&args(&["run"])).is_err());
        assert!(parse(&args(&["start"])).is_err());
        assert!(parse(&args(&[])).is_err());
    }
}
