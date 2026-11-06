//! The supervision loop (P6-2, P6-3), without Windows: it starts the agent in the active console session, restarts
//! it after a crash with a growing pause, moves it to a new session when the console session changes, and stops it.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// A running agent process.
pub trait Agent {
    /// The exit code once the process has ended.
    fn exited(&mut self) -> Option<u32>;
    /// Asks the agent to stop and waits up to `grace`, then ends it by force.
    fn stop(&mut self, grace: Duration);
}

/// Starts agents and reports the console session.
pub trait Platform {
    type Agent: Agent;
    /// The session of the physical console, None while there is none (a session switch in progress).
    fn console_session(&mut self) -> Option<u32>;
    fn launch(&mut self, session: u32) -> Result<Self::Agent, String>;
    /// Something worth the Windows event log: starts, exits, restarts, errors.
    fn report(&mut self, event: &Event);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Started {
        session: u32,
    },
    Exited {
        session: u32,
        code: u32,
        ran: Duration,
        next_in: Duration,
    },
    LaunchFailed {
        session: u32,
        error: String,
        next_in: Duration,
    },
    SessionChanged {
        from: u32,
        to: u32,
    },
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// The service got `SERVICE_CONTROL_SESSIONCHANGE`; the loop checks the console session at once.
    SessionChange,
    Stop,
}

pub const FIRST_PAUSE: Duration = Duration::from_secs(1);
pub const LONGEST_PAUSE: Duration = Duration::from_secs(30);
/// A run this long counts as healthy: the next crash starts from the first pause again.
pub const HEALTHY_RUN: Duration = Duration::from_secs(60);
pub const STOP_GRACE: Duration = Duration::from_secs(5);
/// How often the loop looks at the agent and the console session without a command.
pub const POLL: Duration = Duration::from_millis(500);

/// The pause before the next start: 1, 2, 4, … 30 s, back to 1 s after a healthy run.
#[derive(Debug, Default, Clone)]
pub struct Backoff {
    failures: u32,
}

impl Backoff {
    pub fn pause(&self) -> Duration {
        FIRST_PAUSE
            .saturating_mul(1u32 << self.failures.min(5))
            .min(LONGEST_PAUSE)
    }

    /// Records how long the last run lasted and returns the pause before the next start.
    pub fn after(&mut self, ran: Duration) -> Duration {
        if ran >= HEALTHY_RUN {
            self.failures = 0;
        }
        let pause = self.pause();
        self.failures = self.failures.saturating_add(1);
        pause
    }
}

struct Running<A> {
    agent: A,
    session: u32,
    since: Instant,
}

/// Runs until `Command::Stop`. `now` is a clock, so tests can drive time.
pub fn supervise<P: Platform>(
    platform: &mut P,
    commands: &Receiver<Command>,
    mut now: impl FnMut() -> Instant,
) {
    let mut running: Option<Running<P::Agent>> = None;
    let mut backoff = Backoff::default();
    let mut next_start = now();
    loop {
        match commands.recv_timeout(POLL) {
            Ok(Command::Stop) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(Command::SessionChange) | Err(RecvTimeoutError::Timeout) => {}
        }
        let t = now();
        let console = platform.console_session();
        if let Some(r) = running.as_mut() {
            if let Some(code) = r.agent.exited() {
                let ran = t.duration_since(r.since);
                let next_in = backoff.after(ran);
                platform.report(&Event::Exited {
                    session: r.session,
                    code,
                    ran,
                    next_in,
                });
                next_start = t + next_in;
                running = None;
            } else if let Some(to) = console.filter(|s| *s != r.session) {
                // Another user or the sign-in screen took the console: follow it at once.
                platform.report(&Event::SessionChanged {
                    from: r.session,
                    to,
                });
                r.agent.stop(STOP_GRACE);
                running = None;
                next_start = t;
            }
        }
        if running.is_none()
            && t >= next_start
            && let Some(session) = console
        {
            match platform.launch(session) {
                Ok(agent) => {
                    platform.report(&Event::Started { session });
                    running = Some(Running {
                        agent,
                        session,
                        since: t,
                    });
                }
                Err(error) => {
                    let next_in = backoff.after(Duration::ZERO);
                    platform.report(&Event::LaunchFailed {
                        session,
                        error,
                        next_in,
                    });
                    next_start = t + next_in;
                }
            }
        }
    }
    if let Some(mut r) = running {
        r.agent.stop(STOP_GRACE);
    }
    platform.report(&Event::Stopped);
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;
    use std::sync::mpsc::channel;

    use super::*;

    #[test]
    fn pauses_grow_and_reset_after_a_healthy_run() {
        let mut b = Backoff::default();
        let quick = Duration::from_secs(2);
        let pauses: Vec<u64> = (0..7).map(|_| b.after(quick).as_secs()).collect();
        assert_eq!(pauses, [1, 2, 4, 8, 16, 30, 30]);
        assert_eq!(b.after(HEALTHY_RUN).as_secs(), 1);
        assert_eq!(b.after(quick).as_secs(), 2);
    }

    /// What the fake world looks like and what happened in it.
    #[derive(Default)]
    struct World {
        console: VecDeque<Option<u32>>,
        last_console: Option<u32>,
        /// Ticks after which each launched agent exits, in launch order; None runs until stopped.
        lifetimes: VecDeque<Option<u32>>,
        fail_launches: u32,
        events: Vec<Event>,
        launched: Vec<u32>,
        stopped: u32,
    }

    struct FakeAgent {
        world: Rc<RefCell<World>>,
        ticks_left: Option<u32>,
    }

    impl Agent for FakeAgent {
        fn exited(&mut self) -> Option<u32> {
            match self.ticks_left.as_mut() {
                Some(0) => Some(3),
                Some(n) => {
                    *n -= 1;
                    None
                }
                None => None,
            }
        }

        fn stop(&mut self, _grace: Duration) {
            self.world.borrow_mut().stopped += 1;
        }
    }

    struct Fake(Rc<RefCell<World>>);

    impl Platform for Fake {
        type Agent = FakeAgent;

        fn console_session(&mut self) -> Option<u32> {
            let mut w = self.0.borrow_mut();
            if let Some(next) = w.console.pop_front() {
                w.last_console = next;
            }
            w.last_console
        }

        fn launch(&mut self, session: u32) -> Result<FakeAgent, String> {
            let mut w = self.0.borrow_mut();
            if w.fail_launches > 0 {
                w.fail_launches -= 1;
                return Err("no token".into());
            }
            w.launched.push(session);
            let ticks_left = w.lifetimes.pop_front().flatten();
            Ok(FakeAgent {
                world: self.0.clone(),
                ticks_left,
            })
        }

        fn report(&mut self, event: &Event) {
            self.0.borrow_mut().events.push(event.clone());
        }
    }

    /// Runs the loop for `ticks` polls, each moving the fake clock by 1 s, then stops it.
    fn run(world: World, ticks: usize, extra: &[(usize, Command)]) -> World {
        let world = Rc::new(RefCell::new(world));
        let (tx, rx) = channel();
        for _ in 0..ticks {
            tx.send(Command::SessionChange).unwrap();
        }
        for (_, c) in extra {
            tx.send(*c).unwrap();
        }
        tx.send(Command::Stop).unwrap();
        let start = Instant::now();
        let mut tick = 0u64;
        supervise(&mut Fake(world.clone()), &rx, || {
            tick += 1;
            start + Duration::from_secs(tick)
        });
        Rc::try_unwrap(world).ok().unwrap().into_inner()
    }

    #[test]
    fn a_crashing_agent_restarts_after_growing_pauses() {
        let w = run(
            World {
                console: [Some(1)].into(),
                lifetimes: [Some(0), Some(0), Some(0), None].into(),
                ..World::default()
            },
            12,
            &[],
        );
        assert_eq!(w.launched, [1, 1, 1, 1]);
        let pauses: Vec<u64> = w
            .events
            .iter()
            .filter_map(|e| match e {
                Event::Exited { next_in, .. } => Some(next_in.as_secs()),
                _ => None,
            })
            .collect();
        assert_eq!(pauses, [1, 2, 4]);
        assert_eq!(w.events.last(), Some(&Event::Stopped));
        assert_eq!(w.stopped, 1, "the last agent is stopped with the service");
    }

    #[test]
    fn the_agent_follows_the_console_session() {
        let w = run(
            World {
                console: [Some(1), Some(1), Some(2)].into(),
                ..World::default()
            },
            5,
            &[],
        );
        assert_eq!(w.launched, [1, 2]);
        assert!(w.events.contains(&Event::SessionChanged { from: 1, to: 2 }));
        assert_eq!(w.stopped, 2);
    }

    #[test]
    fn no_console_session_means_no_agent_and_failed_launches_back_off() {
        let w = run(
            World {
                console: [None, None, Some(4)].into(),
                fail_launches: 2,
                ..World::default()
            },
            8,
            &[],
        );
        let failures = w
            .events
            .iter()
            .filter(|e| matches!(e, Event::LaunchFailed { .. }))
            .count();
        assert_eq!(failures, 2);
        assert_eq!(w.launched, [4]);
    }
}
