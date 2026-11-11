//! The experimental WebTransport video path of the agent (P9-4, D-043), behind `--webtransport`.

use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};

use rcs_webrtc::{AltVideo, FrameSink, RateLimits};
use rcs_wt::{Hooks, WtFrame, WtServer};
use serde_json::{Value, json};

pub struct WebTransport {
    server: WtServer,
    /// The port written into the address, when the browser reaches the server through `rcs-bench netsim`.
    url_port: Option<u16>,
}

impl AltVideo for WebTransport {
    fn offer(&self) -> Option<Value> {
        let (url, hash, token) = self.server.offer(self.url_port)?;
        Some(json!({ "url": format!("{url}?token={token}"), "cert_sha256": hash }))
    }

    fn push(&self, data: Vec<u8>, captured_unix_us: u64, keyframe: bool) -> bool {
        self.server.push(WtFrame {
            data,
            captured_unix_us,
            keyframe,
        })
    }

    fn connected(&self) -> bool {
        self.server.connected()
    }
}

/// Starts the server; the sink is known only after the media server started, so the hooks look it up then.
pub fn start(
    bind: SocketAddr,
    url_port: Option<u16>,
    limits: RateLimits,
    sink: Arc<OnceLock<FrameSink>>,
) -> Result<Arc<WebTransport>, String> {
    let keyframe_sink = sink.clone();
    let hooks = Hooks {
        keyframe: Arc::new(move || {
            if let Some(s) = keyframe_sink.get() {
                s.request_keyframe();
            }
        }),
        target: Arc::new(move |bps| {
            if let Some(s) = sink.get() {
                s.set_target_override(bps);
            }
        }),
    };
    let server = WtServer::start(bind, limits, hooks)?;
    Ok(Arc::new(WebTransport { server, url_port }))
}
