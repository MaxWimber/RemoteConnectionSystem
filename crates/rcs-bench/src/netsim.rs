//! `rcs-bench netsim` (P9-4, D-043): a UDP go-between of the browser and the agent that spoils the way to the
//! browser the same way for WebRTC and WebTransport: a narrow link with its queue and random loss (`LinkTest` of
//! `rcs-webrtc`), and short outages. Datagrams from the browser pass unchanged.
//!
//! The browser sends to `--listen`; the go-between forwards to `--to` from a socket of its own and sends the answers
//! back to the last browser address it heard from. One browser at a time. Counters go to `netsim.json`.

use std::fs;
use std::io::ErrorKind;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use rcs_capture::TimerResolution;
use rcs_webrtc::{LinkTest, Shaper};
use serde_json::json;

use crate::options::Options;

const REPORT_FILE: &str = "netsim.json";
const BUFFER: usize = 65_536;
const POLL: Duration = Duration::from_micros(250);

#[derive(Debug, Default)]
struct Counters {
    to_server: u64,
    to_browser: u64,
    bytes_to_browser: u64,
    dropped_outage: u64,
    dropped_link: u64,
}

/// `EVERY_MS:BURST_MS`: nothing reaches the browser for BURST_MS every EVERY_MS.
fn parse_outage(text: &str) -> Result<(Duration, Duration), String> {
    let (every, burst) = text
        .split_once(':')
        .and_then(|(a, b)| Some((a.parse::<u64>().ok()?, b.parse::<u64>().ok()?)))
        .filter(|(a, b)| *b > 0 && b < a)
        .ok_or_else(|| format!("--outage {text}: EVERY_MS:BURST_MS, burst shorter than every"))?;
    Ok((Duration::from_millis(every), Duration::from_millis(burst)))
}

fn in_outage(outage: Option<(Duration, Duration)>, since: Duration) -> bool {
    outage
        .is_some_and(|(every, burst)| since.as_nanos() % every.as_nanos().max(1) < burst.as_nanos())
}

pub fn run(options: &Options) -> Result<(), String> {
    let listen: SocketAddr = options
        .listen
        .as_deref()
        .ok_or("netsim needs --listen")?
        .parse()
        .map_err(|e| format!("--listen: {e}"))?;
    let to: SocketAddr = options
        .to
        .as_deref()
        .ok_or("netsim needs --to")?
        .parse()
        .map_err(|e| format!("--to: {e}"))?;
    let link = options
        .link
        .as_deref()
        .map(|steps| {
            LinkTest::parse(steps, options.link_queue_ms)
                .and_then(|t| t.with_loss(options.link_loss))
        })
        .transpose()
        .map_err(|e| format!("--link: {e}"))?;
    let outage = options.outage.as_deref().map(parse_outage).transpose()?;
    fs::create_dir_all(&options.out).map_err(|e| format!("{}: {e}", options.out.display()))?;

    let browser_side = UdpSocket::bind(listen).map_err(|e| format!("bind {listen}: {e}"))?;
    let server_side = UdpSocket::bind((to.ip(), 0)).map_err(|e| format!("bind for {to}: {e}"))?;
    server_side
        .connect(to)
        .map_err(|e| format!("connect {to}: {e}"))?;
    browser_side
        .set_nonblocking(true)
        .map_err(|e| e.to_string())?;
    server_side
        .set_nonblocking(true)
        .map_err(|e| e.to_string())?;
    let _timer = TimerResolution::new(1);

    println!(
        "netsim {listen} → {to}: канал {}, очередь {} мс, потери {}%, пропадания {}",
        options.link.as_deref().unwrap_or("без ограничения"),
        options.link_queue_ms,
        options.link_loss,
        options.outage.as_deref().unwrap_or("нет"),
    );
    let started = Instant::now();
    let duration = Duration::from_secs(u64::from(options.seconds));
    let mut shaper = link.map(|l| Shaper::new(l, started));
    let mut browser: Option<SocketAddr> = None;
    let mut buffer = vec![0u8; BUFFER];
    let mut c = Counters::default();
    while started.elapsed() < duration {
        let mut busy = false;
        loop {
            match browser_side.recv_from(&mut buffer) {
                Ok((n, from)) => {
                    busy = true;
                    browser = Some(from);
                    c.to_server += 1;
                    let _ = server_side.send(&buffer[..n]);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                // Windows reports an ICMP "port unreachable" of an earlier send on the next receive.
                Err(e) if e.kind() == ErrorKind::ConnectionReset => {}
                Err(e) => return Err(format!("receive from the browser: {e}")),
            }
        }
        loop {
            match server_side.recv(&mut buffer) {
                Ok(n) => {
                    busy = true;
                    let Some(dest) = browser else { continue };
                    let now = Instant::now();
                    if in_outage(outage, now.duration_since(started)) {
                        c.dropped_outage += 1;
                    } else if let Some(s) = shaper.as_mut() {
                        if !s.offer(now, &buffer[..n], dest) {
                            c.dropped_link += 1;
                        }
                    } else {
                        c.to_browser += 1;
                        c.bytes_to_browser += n as u64;
                        let _ = browser_side.send_to(&buffer[..n], dest);
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::ConnectionReset => {}
                Err(e) => return Err(format!("receive from the agent: {e}")),
            }
        }
        if let Some(s) = shaper.as_mut() {
            while let Some((data, dest)) = s.due(Instant::now()) {
                busy = true;
                c.to_browser += 1;
                c.bytes_to_browser += data.len() as u64;
                let _ = browser_side.send_to(&data, dest);
            }
        }
        if !busy {
            std::thread::sleep(POLL);
        }
    }
    let report = json!({
        "listen": listen.to_string(),
        "to": to.to_string(),
        "link": options.link,
        "link_queue_ms": options.link_queue_ms,
        "link_loss_percent": options.link_loss,
        "outage": options.outage,
        "seconds": options.seconds,
        "to_server": c.to_server,
        "to_browser": c.to_browser,
        "mbps_to_browser": c.bytes_to_browser as f64 * 8.0 / f64::from(options.seconds) / 1e6,
        "dropped_outage": c.dropped_outage,
        "dropped_link": c.dropped_link,
    });
    let path = options.out.join(REPORT_FILE);
    fs::write(
        &path,
        serde_json::to_string_pretty(&report).unwrap_or_default(),
    )
    .map_err(|e| format!("{}: {e}", path.display()))?;
    println!("{report}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outages_repeat_at_their_period() {
        let o = Some(parse_outage("10000:150").unwrap());
        assert!(in_outage(o, Duration::from_millis(50)));
        assert!(!in_outage(o, Duration::from_millis(200)));
        assert!(in_outage(o, Duration::from_millis(10_100)));
        assert!(parse_outage("100:200").is_err());
    }
}
