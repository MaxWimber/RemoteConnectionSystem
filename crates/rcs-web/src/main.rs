//! `rcs-web`: the web client, sign-in, signaling and, from phase 7, files.
//!
//! Listens on loopback only; Caddy publishes it (docs/SECURITY.md). Without a password and TOTP set up locally
//! with `rcs-web set-password` and `rcs-web enroll-totp` it does not start. `rcs-web service` is what the service
//! control manager runs under `NT SERVICE\rcs-web` (D-025).

mod agent;
mod app;
mod audit;
mod auth;
mod console;
mod secrets;
mod signal;
mod totp;
mod util;

#[cfg(test)]
mod tests;

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use rcs_protocol::BUILD_ID;
use rcs_protocol::agent::PIPE_NAME;
use serde_json::json;
use tracing::{error, info};
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::{define_windows_service, service_dispatcher};

use crate::app::{AppState, Settings};
use crate::audit::Audit;
use crate::auth::Policy;
use crate::secrets::{MAX_PASSWORD_BYTES, MIN_PASSWORD_CHARS, RECOVERY_CODES, Store};

const USAGE: &str = "\
Usage:
  rcs-web [--listen ADDR] [--static DIR] [--pipe NAME] [--base PATH] [--origin URL] [--secrets DIR] [--audit FILE]
  rcs-web set-password [--secrets DIR] [--audit FILE]
  rcs-web enroll-totp [--secrets DIR] [--audit FILE]
  rcs-web service [options as above] [--log FILE]   run by the service control manager only

  --listen   loopback address, default 127.0.0.1:8080
  --static   the built web client, default web/dist
  --base     URL prefix, default /RemoteConnectionSystem
  --origin   the public origin browsers use, default https://maxwimber.ru:8444
  --secrets  default %ProgramData%\\RCS\\secrets
  --audit    default %ProgramData%\\RCS\\logs\\audit.log
  --log      the service's log, default %ProgramData%\\RCS\\logs\\rcs-web.jsonl";

const SERVICE_NAME: &str = "rcs-web";

struct Options {
    listen: SocketAddr,
    static_dir: PathBuf,
    pipe: String,
    base: String,
    origin: String,
    secrets: PathBuf,
    audit: PathBuf,
    log: PathBuf,
}

fn parse(args: &[String]) -> Result<Options, String> {
    let mut options = Options {
        listen: SocketAddr::from(([127, 0, 0, 1], 8080)),
        static_dir: PathBuf::from("web").join("dist"),
        pipe: PIPE_NAME.into(),
        base: "/RemoteConnectionSystem".into(),
        origin: "https://maxwimber.ru:8444".into(),
        secrets: secrets::default_dir(),
        audit: audit::default_path(),
        log: audit::default_path().with_file_name("rcs-web.jsonl"),
    };
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        let value = rest.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--listen" => options.listen = value.parse().map_err(|e| format!("{flag}: {e}"))?,
            "--static" => options.static_dir = PathBuf::from(value),
            "--pipe" => options.pipe = value.clone(),
            "--base" => options.base = value.trim_end_matches('/').to_string(),
            "--origin" => options.origin = value.trim_end_matches('/').to_string(),
            "--secrets" => options.secrets = PathBuf::from(value),
            "--audit" => options.audit = PathBuf::from(value),
            "--log" => options.log = PathBuf::from(value),
            _ => return Err(format!("unknown option {flag}")),
        }
    }
    if !options.base.starts_with('/') {
        return Err("--base must start with /".into());
    }
    if !(options.origin.starts_with("https://") || options.origin.starts_with("http://"))
        || !options.origin.is_ascii()
    {
        return Err("--origin must look like https://host:port".into());
    }
    // Only Caddy may publish rcs-web (docs/SECURITY.md); a LAN or public address is refused.
    if !options.listen.ip().is_loopback() {
        return Err(
            "--listen must be a loopback address: rcs-web is published through Caddy only".into(),
        );
    }
    Ok(options)
}

fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("runtime: {e}"))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (command, rest) = match args.first().map(String::as_str) {
        Some(c @ ("set-password" | "enroll-totp" | "service")) => (c, &args[1..]),
        Some("--help" | "-h" | "help") => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        _ => ("serve", &args[..]),
    };
    let options = match parse(rest) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let result = match command {
        "set-password" => set_password(&options),
        "enroll-totp" => enroll_totp(&options),
        "service" => log_to_file(&options.log).and_then(|()| {
            service_dispatcher::start(SERVICE_NAME, ffi_service_main)
                .map_err(|e| format!("{e}: this mode is started by the service control manager"))
        }),
        _ => {
            tracing_subscriber::fmt()
                .json()
                .with_max_level(tracing::Level::INFO)
                .init();
            runtime().and_then(|rt| rt.block_on(serve(options, std::future::pending())))
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Ошибка: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Refuses to start until both secrets are set and readable.
fn check_secrets(store: &Store) -> Result<(), String> {
    store.prepare()?;
    let file = store.load()?;
    let hash = file
        .password_hash
        .as_deref()
        .ok_or("пароль не задан: выполните rcs-web set-password")?;
    secrets::check_hash(hash)?;
    if file.totp_dpapi.is_none() {
        return Err("TOTP не настроен: выполните rcs-web enroll-totp".into());
    }
    let secret = file.totp_secret()?;
    if secret.len() < totp::SECRET_BYTES {
        return Err("секрет TOTP слишком короткий: выполните rcs-web enroll-totp".into());
    }
    Ok(())
}

fn log_to_file(path: &PathBuf) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("log {}: {e}", path.display()))?;
    tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::INFO)
        .with_writer(Mutex::new(file))
        .try_init()
        .map_err(|e| format!("logging: {e}"))?;
    std::panic::set_hook(Box::new(|info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        error!(panic = %info, %backtrace, "panic");
    }));
    Ok(())
}

define_windows_service!(ffi_service_main, service_main);

fn service_main(_scm_args: Vec<OsString>) {
    let args: Vec<String> = std::env::args().skip(2).collect();
    if let Err(e) = parse(&args).and_then(run_service) {
        error!(error = %e, "service failed");
    }
}

/// Serves until the service control manager says stop; the open connections close gracefully.
fn run_service(options: Options) -> Result<(), String> {
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let stop = Mutex::new(Some(stop));
    let handler = move |event| -> ServiceControlHandlerResult {
        match event {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                if let Some(s) = stop.lock().ok().and_then(|mut s| s.take()) {
                    let _ = s.send(());
                }
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
        ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        0,
    )
    .map_err(|e| e.to_string())?;
    let result = runtime().and_then(|rt| {
        rt.block_on(serve(options, async {
            let _ = stopped.await;
        }))
    });
    if let Err(e) = &result {
        error!(error = %e, "rcs-web stopped");
    }
    let _ = set(
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
        u32::from(result.is_err()),
    );
    result
}

async fn serve(
    options: Options,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), String> {
    let store = Store::new(options.secrets.clone());
    check_secrets(&store)?;
    let audit = Audit::open(options.audit.clone())?;
    audit.event("started", None, json!({ "build": BUILD_ID }));
    let state = Arc::new(AppState::new(Settings {
        pipe: options.pipe.clone(),
        base: options.base.clone(),
        origin: options.origin.clone(),
        policy: Policy::default(),
        store,
        audit,
    }));
    app::spawn_sweeper(state.clone());
    let router = app::router(state, &options.static_dir, &options.base);
    let listener = tokio::net::TcpListener::bind(options.listen)
        .await
        .map_err(|e| format!("listen {}: {e}", options.listen))?;
    info!(listen = %options.listen, base = %options.base, origin = %options.origin, "rcs-web started");
    println!(
        "rcs-web {BUILD_ID}: http://{}{}/ for {}",
        options.listen, options.base, options.origin
    );
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await
    .map_err(|e| format!("server: {e}"))
}

/// Appends to an existing audit log only. These commands may run elevated, and a log they created would belong to
/// Administrators, which `rcs-web` under the user's account could then not write.
fn audit_best_effort(options: &Options, event: &str) {
    if !options.audit.exists() {
        return;
    }
    if let Ok(audit) = Audit::open(options.audit.clone()) {
        audit.event(event, None, json!({}));
        // The writer thread flushes before the process exits only if given a moment.
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

fn set_password(options: &Options) -> Result<(), String> {
    let store = Store::new(options.secrets.clone());
    store.prepare()?;
    println!(
        "Пароль для входа в RemoteConnectionSystem: не меньше {MIN_PASSWORD_CHARS} символов. Ввод не отображается."
    );
    let first = console::read_line("Пароль: ", true).map_err(|e| e.to_string())?;
    let chars = first.chars().count();
    if chars < MIN_PASSWORD_CHARS {
        return Err(format!(
            "пароль короче {MIN_PASSWORD_CHARS} символов ({chars})"
        ));
    }
    if first.len() > MAX_PASSWORD_BYTES {
        return Err(format!("пароль длиннее {MAX_PASSWORD_BYTES} байт"));
    }
    let second = console::read_line("Ещё раз: ", true).map_err(|e| e.to_string())?;
    if first != second {
        return Err("пароли не совпали".into());
    }
    let hash = secrets::hash_password(&first)?;
    let mut file = store.load()?;
    file.password_hash = Some(hash);
    store.save(&file)?;
    audit_best_effort(options, "password_changed");
    println!("Пароль сохранён в {}.", store.dir().display());
    println!("Он действует сразу. Уже открытая сессия остаётся до выхода или истечения.");
    if file.totp_dpapi.is_none() {
        println!("Следующий шаг: rcs-web enroll-totp");
    }
    Ok(())
}

fn print_qr(text: &str) -> Result<(), String> {
    use qrcode::QrCode;
    use qrcode::render::unicode::Dense1x2;
    let code = QrCode::new(text.as_bytes()).map_err(|e| e.to_string())?;
    let picture = code
        .render::<Dense1x2>()
        .dark_color(Dense1x2::Light)
        .light_color(Dense1x2::Dark)
        .quiet_zone(true)
        .build();
    println!("{picture}");
    Ok(())
}

fn enroll_totp(options: &Options) -> Result<(), String> {
    let store = Store::new(options.secrets.clone());
    store.prepare()?;
    let secret = util::random::<{ totp::SECRET_BYTES }>();
    let account = options
        .origin
        .split("://")
        .nth(1)
        .and_then(|h| h.split(':').next())
        .unwrap_or("rcs");
    let uri = totp::otpauth_uri(&secret, account);
    println!(
        "Отсканируйте QR-код приложением для одноразовых кодов (Google Authenticator, Aegis и другие)."
    );
    print_qr(&uri)?;
    println!(
        "Если QR не читается, введите ключ вручную: {}",
        totp::base32(&secret)
    );
    println!("Тип: по времени, 6 цифр, 30 секунд, SHA-1.");
    let mut matched = None;
    for _ in 0..3 {
        let code =
            console::read_line("Текущий код из приложения: ", false).map_err(|e| e.to_string())?;
        let now = totp::step_of(util::unix_seconds(SystemTime::now()));
        matched = totp::verify(&secret, &code, now, None);
        if matched.is_some() {
            break;
        }
        println!("Код не подошёл. Проверьте время на телефоне и попробуйте снова.");
    }
    let step = matched.ok_or("код не подтверждён, секрет не сохранён")?;
    let codes: Vec<String> = (0..RECOVERY_CODES)
        .map(|_| secrets::new_recovery_code())
        .collect();
    let mut file = store.load()?;
    file.set_totp_secret(&secret)?;
    file.totp_last_step = Some(step);
    file.set_recovery_codes(&codes);
    store.save(&file)?;
    audit_best_effort(options, "totp_enrolled");
    println!();
    println!("TOTP сохранён. Коды восстановления, каждый действует один раз:");
    for code in &codes {
        println!("  {code}");
    }
    println!("Запишите их и храните отдельно от телефона. Больше они не показываются.");
    println!("Прежний секрет TOTP и прежние коды восстановления больше не действуют.");
    if file.password_hash.is_none() {
        println!("Следующий шаг: rcs-web set-password");
    }
    Ok(())
}
