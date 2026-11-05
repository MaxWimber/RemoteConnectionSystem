//! Client of `\\.\pipe\rcs-agent`: one request at a time, a new pipe connection per request.

use std::time::Duration;

use rcs_protocol::agent::{Request, Response, body_len, decode, encode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::ClientOptions;
use tokio::sync::Mutex;
use windows::Win32::Foundation::ERROR_PIPE_BUSY;

const ATTEMPTS: u32 = 20;
const RETRY: Duration = Duration::from_millis(50);
const TIMEOUT: Duration = Duration::from_secs(10);

pub struct Client {
    name: String,
    /// The agent serves one client at a time; this keeps our own requests in line.
    turn: Mutex<()>,
}

impl Client {
    pub fn new(name: String) -> Self {
        Self {
            name,
            turn: Mutex::new(()),
        }
    }

    pub async fn call(&self, request: Request) -> Result<Response, String> {
        let _turn = self.turn.lock().await;
        tokio::time::timeout(TIMEOUT, self.exchange(&request))
            .await
            .map_err(|_| "the agent did not answer in time".to_string())?
    }

    async fn exchange(&self, request: &Request) -> Result<Response, String> {
        let mut attempt = 0;
        let mut pipe = loop {
            match ClientOptions::new().open(&self.name) {
                Ok(pipe) => break pipe,
                Err(e)
                    if e.raw_os_error() == Some(ERROR_PIPE_BUSY.0 as i32) && attempt < ATTEMPTS =>
                {
                    attempt += 1;
                    tokio::time::sleep(RETRY).await;
                }
                Err(e) => return Err(format!("agent pipe: {e}")),
            }
        };
        let bytes = encode(request).map_err(|e| e.to_string())?;
        pipe.write_all(&bytes)
            .await
            .map_err(|e| format!("agent pipe: {e}"))?;
        let mut prefix = [0u8; 4];
        pipe.read_exact(&mut prefix)
            .await
            .map_err(|e| format!("agent pipe: {e}"))?;
        let mut body = vec![0u8; body_len(prefix).map_err(|e| e.to_string())?];
        pipe.read_exact(&mut body)
            .await
            .map_err(|e| format!("agent pipe: {e}"))?;
        decode(&body).map_err(|e| e.to_string())
    }
}
