//! The replay clock: scenario seconds as a function of wall time, shared by
//! the fake server and the broker.

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::Notify;

/// When and how a clock was started ([`ReplayClock::start`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Started {
    /// The wall time it started at, in milliseconds since the Unix epoch.
    pub wall_unix_ms: i64,
    /// The scenario time it started from.
    pub start_at: f64,
    /// Scenario seconds per wall second.
    pub speed: f64,
}

#[derive(Debug)]
struct ClockState {
    started: Option<(Instant, Started)>,
    /// The time of a manual clock.
    manual_t: f64,
}

/// Scenario time, `t = start_at + (wall - wall_at_start) * speed`, clamped to
/// `0..=duration`. It stands at `start_at` until started.
///
/// A manual clock ([`ReplayClock::manual`]) moves only when told to
/// ([`ReplayClock::set`], [`ReplayClock::advance`]), so tests never wait for
/// real time. Every change wakes whoever waits in [`ReplayClock::changed`]
/// (the push scheduler).
#[derive(Debug)]
pub struct ReplayClock {
    manual: bool,
    start_at: f64,
    speed: f64,
    duration: f64,
    state: Mutex<ClockState>,
    notify: Notify,
}

/// The current wall time in milliseconds since the Unix epoch.
pub fn wall_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64)
}

impl ReplayClock {
    fn new(manual: bool, start_at: f64, speed: f64, duration: f64) -> Self {
        // A duration that is not a finite, non-negative number counts as 0.
        let duration = if duration.is_finite() && duration > 0.0 {
            duration
        } else {
            0.0
        };
        let start_at = if start_at.is_finite() {
            start_at.clamp(0.0, duration)
        } else {
            0.0
        };
        Self {
            manual,
            start_at,
            speed,
            duration,
            state: Mutex::new(ClockState {
                started: None,
                manual_t: start_at,
            }),
            notify: Notify::new(),
        }
    }

    /// A clock that follows wall time at `speed` scenario seconds per second
    /// once started, from `start_at`, for a scenario of `duration` seconds.
    /// A `speed` that is not a positive number counts as 1.
    pub fn wall(start_at: f64, speed: f64, duration: f64) -> Self {
        let speed = if speed.is_finite() && speed > 0.0 {
            speed
        } else {
            1.0
        };
        Self::new(false, start_at, speed, duration)
    }

    /// A clock that stands at `start_at` until it is set or advanced.
    /// `speed` is only reported.
    pub fn manual(start_at: f64, speed: f64, duration: f64) -> Self {
        Self::new(true, start_at, speed, duration)
    }

    fn state(&self) -> MutexGuard<'_, ClockState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether this is a manual clock.
    pub fn is_manual(&self) -> bool {
        self.manual
    }

    /// The scenario time the clock starts from.
    pub fn start_at(&self) -> f64 {
        self.start_at
    }

    /// Scenario seconds per wall second.
    pub fn speed(&self) -> f64 {
        self.speed
    }

    /// The scenario's length; the clock never goes past it.
    pub fn duration(&self) -> f64 {
        self.duration
    }

    fn mark_started(&self, state: &mut ClockState) -> Started {
        if let Some((_, started)) = state.started {
            return started;
        }
        let started = Started {
            wall_unix_ms: wall_unix_ms(),
            start_at: self.start_at,
            speed: self.speed,
        };
        state.started = Some((Instant::now(), started));
        started
    }

    /// Starts the clock (a second call changes nothing) and says when.
    pub fn start(&self) -> Started {
        let started = self.mark_started(&mut self.state());
        self.notify.notify_waiters();
        started
    }

    /// When the clock was started, if it was.
    pub fn started(&self) -> Option<Started> {
        self.state().started.map(|(_, started)| started)
    }

    /// The scenario time now, in seconds.
    pub fn now(&self) -> f64 {
        let state = self.state();
        if self.manual {
            return state.manual_t;
        }
        match state.started {
            None => self.start_at,
            Some((instant, _)) => {
                let t = self.start_at + instant.elapsed().as_secs_f64() * self.speed;
                t.min(self.duration)
            }
        }
    }

    /// The whole scenario second the clock is in (what a scenario line's
    /// `t` is compared with).
    pub fn second(&self) -> u32 {
        self.now().floor() as u32
    }

    /// Whether the clock has reached the end of the scenario.
    pub fn finished(&self) -> bool {
        self.now() >= self.duration
    }

    /// Sets a manual clock to `t` (clamped to the scenario) and starts it.
    /// Fails on a wall clock or for a `t` that is not a number.
    pub fn set(&self, t: f64) -> Result<(), String> {
        if !self.manual {
            return Err("only a manual clock can be set".to_owned());
        }
        if !t.is_finite() {
            return Err(format!("{t} is not a time"));
        }
        {
            let mut state = self.state();
            self.mark_started(&mut state);
            state.manual_t = t.clamp(0.0, self.duration);
        }
        self.notify.notify_waiters();
        Ok(())
    }

    /// Moves a manual clock on by `seconds` (clamped to the scenario).
    pub fn advance(&self, seconds: f64) -> Result<(), String> {
        let now = self.now();
        self.set(now + seconds)
    }

    /// How long in wall time until a started wall clock reaches `t`; `None`
    /// for a manual or stopped clock, zero when it is there already.
    pub fn wall_until(&self, t: f64) -> Option<Duration> {
        if self.manual || self.started().is_none() {
            return None;
        }
        let ahead = (t - self.now()) / self.speed;
        Some(Duration::try_from_secs_f64(ahead.max(0.0)).unwrap_or(Duration::MAX))
    }

    /// Resolves at the next change (start, set or advance). Create the
    /// future before reading the clock so that no change is missed.
    pub fn changed(&self) -> tokio::sync::futures::Notified<'_> {
        self.notify.notified()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wall_clock_stands_until_started_then_runs_at_its_speed() {
        let clock = ReplayClock::wall(100.0, 1000.0, 10_000.0);
        assert!(!clock.is_manual());
        assert_eq!(clock.now(), 100.0);
        assert_eq!(clock.second(), 100);
        assert!(clock.started().is_none());
        assert_eq!(clock.wall_until(200.0), None);
        let started = clock.start();
        assert_eq!((started.start_at, started.speed), (100.0, 1000.0));
        assert_eq!(clock.start(), started, "a second start changes nothing");
        assert!(started.wall_unix_ms > 1_600_000_000_000);
        std::thread::sleep(Duration::from_millis(20));
        let now = clock.now();
        assert!(now > 110.0 && now <= 10_000.0, "{now}");
        assert!(clock.wall_until(now + 1000.0).unwrap() <= Duration::from_secs(1));
        assert_eq!(clock.wall_until(0.0), Some(Duration::ZERO));
        assert!(clock.set(5.0).is_err() && clock.advance(5.0).is_err());
    }

    #[test]
    fn a_wall_clock_clamps_at_the_end() {
        let clock = ReplayClock::wall(0.0, 1_000_000.0, 5.0);
        clock.start();
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(clock.now(), 5.0);
        assert!(clock.finished());
    }

    #[test]
    fn bad_arguments_are_tamed() {
        let clock = ReplayClock::wall(f64::NAN, -1.0, -5.0);
        assert_eq!(
            (clock.start_at(), clock.speed(), clock.duration()),
            (0.0, 1.0, 0.0)
        );
        for duration in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.5] {
            let clock = ReplayClock::manual(5.0, 1.0, duration);
            assert_eq!((clock.duration(), clock.now()), (0.0, 0.0), "{duration}");
            clock.start();
            assert!(clock.finished());
        }
        let clock = ReplayClock::wall(50.0, f64::INFINITY, 10.0);
        assert_eq!((clock.start_at(), clock.speed()), (10.0, 1.0));
    }

    #[test]
    fn a_manual_clock_moves_only_when_told() {
        let clock = ReplayClock::manual(10.0, 60.0, 100.0);
        assert!(clock.is_manual());
        assert_eq!((clock.now(), clock.speed()), (10.0, 60.0));
        assert!(clock.started().is_none());
        clock.set(20.5).unwrap();
        assert!(clock.started().is_some(), "setting starts it");
        assert_eq!((clock.now(), clock.second()), (20.5, 20));
        clock.advance(1.0).unwrap();
        assert_eq!(clock.now(), 21.5);
        clock.advance(1000.0).unwrap();
        assert_eq!(clock.now(), 100.0);
        assert!(clock.finished());
        clock.set(-3.0).unwrap();
        assert_eq!(clock.now(), 0.0);
        assert!(clock.set(f64::NAN).is_err());
        assert_eq!(clock.wall_until(50.0), None);
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(clock.now(), 0.0, "wall time does not move it");
    }

    #[tokio::test]
    async fn changes_wake_the_waiters() {
        let clock = std::sync::Arc::new(ReplayClock::manual(0.0, 1.0, 100.0));
        let waiter = {
            let clock = clock.clone();
            tokio::spawn(async move {
                let changed = clock.changed();
                tokio::pin!(changed);
                changed.as_mut().enable();
                changed.await;
                clock.now()
            })
        };
        // Wake it until it has seen a change after it registered.
        for t in 1..=50 {
            clock.set(f64::from(t)).unwrap();
            tokio::time::sleep(Duration::from_millis(1)).await;
            if waiter.is_finished() {
                break;
            }
        }
        assert!(waiter.await.unwrap() >= 1.0);
        let start = clock.changed();
        tokio::pin!(start);
        start.as_mut().enable();
        clock.start();
        start.await;
    }
}
