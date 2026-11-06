//! Ctrl+Alt+Del (P6-7, D-025): the agent asks through `\\.\pipe\rcs-supervisor`, the supervisor calls `SendSAS`.
//!
//! Only SYSTEM may open the pipe, and the agent runs as SYSTEM. One byte in: 1 for Ctrl+Alt+Del; one byte out: 0 when
//! sent. `SendSAS` from a service needs the `SoftwareSASGeneration` policy, which only the user turns on.

use std::fs::File;
use std::io::{Read, Write};
use std::os::windows::io::FromRawHandle;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tracing::{info, warn};
use windows::Win32::Foundation::{ERROR_PIPE_CONNECTED, HLOCAL, LocalFree};
use windows::Win32::Security::Authentication::Identity::SendSAS;
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::core::{HSTRING, w};

pub const PIPE: &str = r"\\.\pipe\rcs-supervisor";
pub const REQUEST_SAS: u8 = 1;
pub const DONE: u8 = 0;

/// Serves Ctrl+Alt+Del requests on its own thread until `stop`. Each answered request is reported with `report`.
pub fn serve(stop: Arc<AtomicBool>, report: impl Fn(bool) + Send + 'static) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("sas".into())
        .spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match accept() {
                    Ok(mut client) => {
                        let mut request = [0u8; 1];
                        if client.read_exact(&mut request).is_err() || request[0] != REQUEST_SAS {
                            continue;
                        }
                        // SAFETY: no pointers; FALSE means "as the service", which the policy must allow.
                        unsafe { SendSAS(false) };
                        // SendSAS reports nothing; the policy decides whether the screen appears.
                        info!("Ctrl+Alt+Del sent");
                        let _ = client.write_all(&[DONE]);
                        report(true);
                    }
                    Err(e) => {
                        warn!(error = %e, "sas pipe");
                        report(false);
                        std::thread::sleep(std::time::Duration::from_secs(1));
                    }
                }
            }
        })
        .map(|_| ())
}

/// Waits for one client of the pipe; only SYSTEM may connect.
fn accept() -> Result<File, String> {
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: the SDDL string is a static wide string; the descriptor is freed below.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            w!("D:P(A;;GA;;;SY)"),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
    }
    .map_err(|e| format!("pipe security: {e}"))?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: false.into(),
    };
    // SAFETY: the name and the attributes outlive the call.
    let handle = unsafe {
        CreateNamedPipeW(
            &HSTRING::from(PIPE),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            16,
            16,
            0,
            Some(&attributes),
        )
    };
    // SAFETY: the descriptor was allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW and the pipe
    // keeps its own copy.
    unsafe { LocalFree(Some(HLOCAL(descriptor.0))) };
    if handle.is_invalid() {
        return Err(format!(
            "cannot create {PIPE}: {}",
            windows::core::Error::from_thread()
        ));
    }
    // SAFETY: the handle is valid and owned by nothing else; File closes it once.
    let file = unsafe { File::from_raw_handle(handle.0) };
    // SAFETY: the handle is valid; a blocking connect waits for one client.
    if let Err(e) = unsafe { ConnectNamedPipe(handle, None) }
        && e.code() != ERROR_PIPE_CONNECTED.to_hresult()
    {
        return Err(format!("connect: {e}"));
    }
    Ok(file)
}
