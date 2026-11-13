//! `rcs-bench netsim` (P9-4, D-043): a UDP go-between of the browser and the agent that spoils the way to the
//! browser the same way for WebRTC and WebTransport: a narrow link with its queue and random loss (`LinkTest` of
//! `rcs-webrtc`), and short outages. Datagrams from the browser pass unchanged.
//!
//! After the link a delay stage can hold the datagrams as a mobile network does (2026-11-11, D-047): a fixed one-way
//! delay, a variation that changes every `JITTER_SLOT` and stalls that hold everything for a while and then let it
//! through at once. Nothing is lost there and the order stays.
//!
//! The way to the agent, where input goes, can lose datagrams at random and delay them with a variation, in its own
//! delay stage without stalls (P11-1).
//!
//! The browser sends to `--listen`; the go-between forwards to `--to` from a socket of its own and sends the answers
//! back to the last browser address it heard from. One browser at a time. Counters go to `netsim.json`.

use std::collections::VecDeque;
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
/// The extra delay of the jitter changes this often.
const JITTER_SLOT: Duration = Duration::from_millis(20);

#[derive(Debug, Default)]
struct Counters {
    to_server: u64,
    to_browser: u64,
    bytes_to_browser: u64,
    dropped_outage: u64,
    dropped_link: u64,
    stalled: u64,
    dropped_up: u64,
}

/// The delay stage: base delay, jitter, stalls; keeps the order.
struct Delay {
    base: Duration,
    jitter: Duration,
    stall: Option<(Duration, Duration)>,
    started: Instant,
    slot: (Instant, Duration),
    last_due: Instant,
    queue: VecDeque<(Instant, Vec<u8>, SocketAddr)>,
    random: u64,
}

impl Delay {
    fn new(
        base: Duration,
        jitter: Duration,
        stall: Option<(Duration, Duration)>,
        now: Instant,
    ) -> Self {
        Self {
            base,
            jitter,
            stall,
            started: now,
            slot: (now, Duration::ZERO),
            last_due: now,
            queue: VecDeque::new(),
            random: 0x2545_F491_4F6C_DD1D,
        }
    }

    fn uniform(&mut self) -> f64 {
        let mut x = self.random;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.random = x;
        (x >> 11) as f64 / (1u64 << 53) as f64
    }

    /// When a datagram that reaches the stage at `now` leaves it; true in the second place when a stall holds it.
    fn due(&mut self, now: Instant) -> (Instant, bool) {
        if now.duration_since(self.slot.0) >= JITTER_SLOT {
            let extra = self.jitter.mul_f64(self.uniform());
            self.slot = (now, extra);
        }
        let mut due = now + self.base + self.slot.1;
        let mut stalled = false;
        if let Some((every, hold)) = self.stall {
            let since = now.duration_since(self.started);
            let into = Duration::from_nanos((since.as_nanos() % every.as_nanos().max(1)) as u64);
            if into < hold {
                due = due.max(now + (hold - into) + self.base);
                stalled = true;
            }
        }
        let due = due.max(self.last_due);
        self.last_due = due;
        (due, stalled)
    }

    fn push(&mut self, now: Instant, data: Vec<u8>, to: SocketAddr) -> bool {
        let (due, stalled) = self.due(now);
        self.queue.push_back((due, data, to));
        stalled
    }

    fn pop(&mut self, now: Instant) -> Option<(Vec<u8>, SocketAddr)> {
        if self.queue.front().is_some_and(|f| f.0 <= now) {
            self.queue.pop_front().map(|(_, d, to)| (d, to))
        } else {
            None
        }
    }
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
    let stall = options
        .stall
        .as_deref()
        .map(|s| parse_outage(s).map_err(|e| e.replace("--outage", "--stall")))
        .transpose()?;
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
        "netsim {listen} → {to}: канал {}, очередь {} мс, потери {}%, пропадания {}, задержка {} мс, разброс {} мс, \
         задержки {}",
        options.link.as_deref().unwrap_or("без ограничения"),
        options.link_queue_ms,
        options.link_loss,
        options.outage.as_deref().unwrap_or("нет"),
        options.delay_ms,
        options.jitter_ms,
        options.stall.as_deref().unwrap_or("нет"),
    );
    let started = Instant::now();
    let duration = Duration::from_secs(u64::from(options.seconds));
    let mut shaper = link.map(|l| Shaper::new(l, started));
    let mut delay = (options.delay_ms > 0 || options.jitter_ms > 0 || stall.is_some()).then(|| {
        Delay::new(
            Duration::from_millis(options.delay_ms),
            Duration::from_millis(options.jitter_ms),
            stall,
            started,
        )
    });
    let mut up = (options.up_loss > 0.0 || options.up_delay_ms > 0 || options.up_jitter_ms > 0)
        .then(|| {
            Delay::new(
                Duration::from_millis(options.up_delay_ms),
                Duration::from_millis(options.up_jitter_ms),
                None,
                started,
            )
        });
    let up_loss = options.up_loss / 100.0;
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
                    if let Some(u) = up.as_mut() {
                        if u.uniform() < up_loss {
                            c.dropped_up += 1;
                        } else {
                            u.push(Instant::now(), buffer[..n].to_vec(), to);
                        }
                        continue;
                    }
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
                    } else if let Some(d) = delay.as_mut() {
                        if d.push(now, buffer[..n].to_vec(), dest) {
                            c.stalled += 1;
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
                if let Some(d) = delay.as_mut() {
                    if d.push(Instant::now(), data, dest) {
                        c.stalled += 1;
                    }
                    continue;
                }
                c.to_browser += 1;
                c.bytes_to_browser += data.len() as u64;
                let _ = browser_side.send_to(&data, dest);
            }
        }
        if let Some(u) = up.as_mut() {
            while let Some((data, _)) = u.pop(Instant::now()) {
                busy = true;
                c.to_server += 1;
                let _ = server_side.send(&data);
            }
        }
        if let Some(d) = delay.as_mut() {
            while let Some((data, dest)) = d.pop(Instant::now()) {
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
        "delay_ms": options.delay_ms,
        "jitter_ms": options.jitter_ms,
        "stall": options.stall,
        "stalled": c.stalled,
        "up_loss_percent": options.up_loss,
        "up_delay_ms": options.up_delay_ms,
        "up_jitter_ms": options.up_jitter_ms,
        "dropped_up": c.dropped_up,
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

    #[test]
    fn the_delay_stage_keeps_the_order_and_holds_through_a_stall() {
        let t = Instant::now();
        let to: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let mut d = Delay::new(
            Duration::from_millis(20),
            Duration::from_millis(60),
            Some((Duration::from_millis(1000), Duration::from_millis(200))),
            t,
        );
        // Inside the stall: held until it ends, plus the base delay.
        assert!(d.push(t + Duration::from_millis(50), vec![1], to));
        assert!(d.queue[0].0 >= t + Duration::from_millis(220));
        let mut last = t;
        for ms in (300..900).step_by(5) {
            assert!(!d.push(t + Duration::from_millis(ms), vec![2], to));
            let due = d.queue.back().unwrap().0;
            assert!(due >= last, "order kept");
            let after = due.duration_since(t + Duration::from_millis(ms));
            assert!(
                after >= Duration::from_millis(20) && after <= Duration::from_millis(80),
                "{after:?}"
            );
            last = due;
        }
        assert!(d.pop(t + Duration::from_millis(219)).is_none());
        assert_eq!(d.pop(t + Duration::from_millis(230)).unwrap().0, vec![1]);
    }
}
