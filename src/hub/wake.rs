//! Notices the machine waking from sleep: the monotonic clock behind tokio's timers stands still
//! while it sleeps on macOS and Linux, but the wall clock moves on.

use std::time::{Duration, SystemTime};

use tokio::time::{Instant, Interval, MissedTickBehavior, interval_at};

const POLL: Duration = Duration::from_secs(1);
/// How far the wall clock may run ahead of the monotonic one between polls without counting as
/// sleep, so a busy runtime or a small clock correction does not.
const SLACK: Duration = Duration::from_secs(5);

pub struct Wake {
    ticks: Interval,
    wall: SystemTime,
    monotonic: Instant,
}

impl Wake {
    pub fn new() -> Self {
        let mut ticks = interval_at(Instant::now() + POLL, POLL);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);

        Self {
            ticks,
            wall: SystemTime::now(),
            monotonic: Instant::now(),
        }
    }

    /// Resolves once the machine wakes from sleep. Safe to cancel and call again.
    pub async fn woke(&mut self) {
        loop {
            self.ticks.tick().await;
            let (wall, monotonic) = (SystemTime::now(), Instant::now());
            // A wall clock set back reads as no time passed, which is not sleep.
            let wall_elapsed = wall.duration_since(self.wall).unwrap_or_default();
            let slept = slept(wall_elapsed, monotonic - self.monotonic);
            self.wall = wall;
            self.monotonic = monotonic;

            if slept {
                return;
            }
        }
    }
}

fn slept(wall: Duration, monotonic: Duration) -> bool {
    wall > monotonic + SLACK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_wall_clock_far_ahead_counts_as_sleep() {
        let second = Duration::from_secs(1);

        assert!(!slept(second, second));
        assert!(!slept(second + SLACK, second));
        assert!(slept(Duration::from_secs(600), second));
        // Paused test time runs only the monotonic clock.
        assert!(!slept(Duration::ZERO, Duration::from_secs(600)));
    }
}
