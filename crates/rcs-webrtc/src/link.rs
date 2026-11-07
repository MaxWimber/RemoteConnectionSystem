//! Testing only (P8-1, D-027): a narrow link on the way out. Datagrams of a connected session leave at the rate of
//! the current step, wait in a queue of limited delay and are dropped when it is full, like a bottleneck router.
//! Off unless `--simulate-link` is given. It copies each datagram; that is fine for a test and never runs otherwise.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

/// Rates over time, repeated: `30:50,30:10` is 30 s at 50 Mbit/s, then 30 s at 10 Mbit/s, then again.
#[derive(Debug, Clone, PartialEq)]
pub struct LinkTest {
    pub steps: Vec<(Duration, f64)>,
    /// The longest a datagram may wait; beyond it the queue drops.
    pub queue: Duration,
}

impl LinkTest {
    pub fn parse(steps: &str, queue_ms: u64) -> Result<Self, String> {
        let steps = steps
            .split(',')
            .map(|step| {
                let (secs, mbps) = step
                    .split_once(':')
                    .ok_or_else(|| format!("«{step}»: expected seconds:Mbit/s"))?;
                let secs: f64 = secs
                    .trim()
                    .parse()
                    .map_err(|_| format!("«{step}»: seconds"))?;
                let mbps: f64 = mbps
                    .trim()
                    .parse()
                    .map_err(|_| format!("«{step}»: Mbit/s"))?;
                if !(secs > 0.0 && mbps > 0.0 && secs.is_finite() && mbps.is_finite()) {
                    return Err(format!("«{step}»: both must be above zero"));
                }
                Ok((Duration::from_secs_f64(secs), mbps * 1e6))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if steps.is_empty() {
            return Err("no steps".into());
        }
        Ok(Self {
            steps,
            queue: Duration::from_millis(queue_ms.max(1)),
        })
    }

    /// Bits per second `since` the session connected.
    pub fn rate_at(&self, since: Duration) -> f64 {
        let cycle: Duration = self.steps.iter().map(|s| s.0).sum();
        let mut t = Duration::from_nanos((since.as_nanos() % cycle.as_nanos().max(1)) as u64);
        for &(length, rate) in &self.steps {
            if t < length {
                return rate;
            }
            t -= length;
        }
        self.steps[self.steps.len() - 1].1
    }
}

pub(crate) struct Shaper {
    test: LinkTest,
    started: Instant,
    queue: VecDeque<(Instant, Vec<u8>, SocketAddr)>,
    free_at: Instant,
}

impl Shaper {
    pub(crate) fn new(test: LinkTest, now: Instant) -> Self {
        Self {
            test,
            started: now,
            queue: VecDeque::new(),
            free_at: now,
        }
    }

    /// Takes a datagram into the queue, or returns false when the queue is full and it is dropped.
    pub(crate) fn offer(&mut self, now: Instant, data: &[u8], to: SocketAddr) -> bool {
        let rate = self.test.rate_at(now.duration_since(self.started));
        let wire = Duration::from_secs_f64(data.len() as f64 * 8.0 / rate);
        let done = self.free_at.max(now) + wire;
        if done.duration_since(now) > self.test.queue {
            return false;
        }
        self.free_at = done;
        self.queue.push_back((done, data.to_vec(), to));
        true
    }

    /// The next datagram whose time has come.
    pub(crate) fn due(&mut self, now: Instant) -> Option<(Vec<u8>, SocketAddr)> {
        if self.queue.front().is_some_and(|f| f.0 <= now) {
            self.queue.pop_front().map(|(_, d, to)| (d, to))
        } else {
            None
        }
    }

    pub(crate) fn next(&self) -> Option<Instant> {
        self.queue.front().map(|f| f.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_parse_and_repeat() {
        let t = LinkTest::parse("30:50, 10:2.5", 200).unwrap();
        assert_eq!(t.rate_at(Duration::from_secs(0)), 50e6);
        assert_eq!(t.rate_at(Duration::from_secs(35)), 2.5e6);
        assert_eq!(
            t.rate_at(Duration::from_secs(41)),
            50e6,
            "the cycle repeats"
        );
        for bad in ["", "30", "x:5", "0:5", "5:0", "5:-1"] {
            assert!(LinkTest::parse(bad, 200).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_queue_paces_and_drops_when_full() {
        let now = Instant::now();
        // 8 Mbit/s: 1000 bytes take 1 ms on the wire; the queue holds 5 ms.
        let mut s = Shaper::new(LinkTest::parse("60:8", 5).unwrap(), now);
        let to: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let accepted = (0..10).filter(|_| s.offer(now, &[0; 1000], to)).count();
        assert_eq!(accepted, 5);
        assert!(s.due(now).is_none(), "nothing leaves before its time");
        let first = s.next().unwrap();
        assert_eq!(first.duration_since(now), Duration::from_millis(1));
        assert!(s.due(first).is_some());
        assert!(s.due(first).is_none());
        // Later the queue has drained and takes datagrams again.
        let later = now + Duration::from_millis(20);
        while s.due(later).is_some() {}
        assert!(s.offer(later, &[0; 1000], to));
    }
}
