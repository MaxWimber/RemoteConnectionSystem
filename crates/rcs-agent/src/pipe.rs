//! The agent's end of `\\.\pipe\rcs-agent`: one client at a time, length-prefixed JSON (`rcs_protocol::agent`).
//!
//! The pipe allows only its owner (the user who runs the agent) and SYSTEM, refuses remote clients, and is created
//! as the first instance of its name, so another process cannot take the name first. Until phase 6 `rcs-web` runs
//! as the same user (`docs/SECURITY.md`).

use std::fs::File;
use std::os::windows::io::FromRawHandle;

use rcs_protocol::agent::{Request, Response, read_message, write_message};
use tracing::{debug, warn};
use windows::Win32::Foundation::{ERROR_PIPE_CONNECTED, HLOCAL, LocalFree};
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

const BUFFER: u32 = 64 * 1024;

/// Owner and SYSTEM get full access, nobody else gets any; `P` keeps inherited entries out.
struct Security(PSECURITY_DESCRIPTOR);

impl Security {
    fn new() -> Result<Self, String> {
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: the SDDL string is a static wide string; the out pointer is a live local, freed in Drop.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                w!("D:P(A;;GA;;;OW)(A;;GA;;;SY)"),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
        }
        .map_err(|e| format!("pipe security: {e}"))?;
        Ok(Self(descriptor))
    }
}

impl Drop for Security {
    fn drop(&mut self) {
        // SAFETY: the descriptor was allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe { LocalFree(Some(HLOCAL(self.0.0))) };
    }
}

fn create(name: &HSTRING, security: &Security) -> Result<File, String> {
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: security.0.0,
        bInheritHandle: false.into(),
    };
    // SAFETY: the name and the attributes outlive the call; the handle is owned by the returned File.
    let handle = unsafe {
        CreateNamedPipeW(
            name,
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            BUFFER,
            BUFFER,
            0,
            Some(&attributes),
        )
    };
    if handle.is_invalid() {
        return Err(format!(
            "cannot create {name}: {}",
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

/// Serves clients one after another until creating the pipe fails.
pub fn serve(name: &str, mut handle: impl FnMut(Request) -> Response) -> Result<(), String> {
    let security = Security::new()?;
    let name = HSTRING::from(name);
    loop {
        let mut file = create(&name, &security)?;
        debug!("pipe client connected");
        loop {
            match read_message::<Request>(&mut file) {
                Ok(Some(request)) => {
                    let response = handle(request);
                    if let Err(e) = write_message(&mut file, &response) {
                        warn!(error = %e, "pipe reply failed");
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    warn!(error = %e, "pipe request failed");
                    break;
                }
            }
        }
        debug!("pipe client gone");
    }
}
