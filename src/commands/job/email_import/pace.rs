//! The run's throughput governor (CLI-0146 §4). `--messages-per-minute`
//! turns into a fixed inter-message interval, and every fetch reserves its
//! slots up front, so the rate a run actually hits is a declared property
//! of the run rather than an emergent side effect of worker count -- which
//! is why this job has no `--concurrency` flag at all.

use std::time::Duration;

use tokio::sync::Mutex;
use tokio::time::Instant;

/// Reserves `count` fetch slots spaced `interval` apart, starting no
/// earlier than `next_allowed`. Returns `(sleep until, new next_allowed)`.
///
/// Pure, so the whole schedule is unit-testable without a clock, a lock, or
/// any I/O -- CLI-0143's `circuit_breaker_should_trip` precedent. Two
/// properties matter and both are deliberate:
///
/// - Concurrent callers *stagger* rather than burst, because each one
///   advances `next_allowed` past its own window before the next caller
///   reads it.
/// - Idle time is never paid back. A gap longer than the outstanding
///   reservation yields `now`, not a burst of accumulated credit, so a run
///   that stalls on a retry does not then hammer the server to catch up.
pub(crate) fn reserve(
    next_allowed: Instant,
    now: Instant,
    count: u32,
    interval: Duration,
) -> (Instant, Instant) {
    let start = if next_allowed > now {
        next_allowed
    } else {
        now
    };
    (start, start + interval * count)
}

/// Converts `--messages-per-minute` into the per-message interval. Clamped
/// to at least 1/min so a `0` can never produce a division by zero or an
/// unbounded rate -- the flag has no "unlimited" spelling by design
/// (CLI-0146 Consequences).
pub(crate) fn interval_for(messages_per_minute: usize) -> Duration {
    Duration::from_secs(60) / messages_per_minute.max(1) as u32
}

/// A token bucket shared by every worker, handing out fetch windows at a
/// fixed spacing.
pub(crate) struct Pacer {
    next_allowed: Mutex<Instant>,
    interval: Duration,
}

impl Pacer {
    pub(crate) fn new(messages_per_minute: usize) -> Self {
        Self {
            next_allowed: Mutex::new(Instant::now()),
            interval: interval_for(messages_per_minute),
        }
    }

    /// Blocks until this caller's window for `count` messages opens.
    ///
    /// The lock is taken only for the arithmetic and **released before the
    /// sleep**. Holding it across the sleep would queue every worker behind
    /// one waiter and silently turn the pacer into a global mutex, which
    /// would serialize the fetch phase rather than pace it.
    pub(crate) async fn acquire(&self, count: usize) {
        let wait_until = {
            let mut next_allowed = self.next_allowed.lock().await;
            let (wait_until, updated) = reserve(
                *next_allowed,
                Instant::now(),
                count.max(1) as u32,
                self.interval,
            );
            *next_allowed = updated;
            wait_until
        };
        tokio::time::sleep_until(wait_until).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INTERVAL: Duration = Duration::from_millis(500);

    #[test]
    fn interval_for_divides_a_minute_by_the_rate() {
        assert_eq!(interval_for(120), Duration::from_millis(500));
        assert_eq!(interval_for(60), Duration::from_secs(1));
    }

    /// A zero rate must not panic on the division, and must not resolve to
    /// "no cap" either.
    #[test]
    fn interval_for_clamps_a_zero_rate_to_one_per_minute() {
        assert_eq!(interval_for(0), Duration::from_secs(60));
    }

    #[test]
    fn reserve_spaces_consecutive_single_reservations_by_one_interval() {
        let start = Instant::now();

        let (first_wait, after_first) = reserve(start, start, 1, INTERVAL);
        let (second_wait, after_second) = reserve(after_first, start, 1, INTERVAL);

        assert_eq!(first_wait, start);
        assert_eq!(second_wait, start + INTERVAL);
        assert_eq!(after_second, start + INTERVAL * 2);
    }

    #[test]
    fn reserve_charges_one_interval_per_message_in_a_batch() {
        let start = Instant::now();

        let (wait, next) = reserve(start, start, 20, INTERVAL);

        assert_eq!(wait, start, "the batch itself starts immediately");
        assert_eq!(next, start + INTERVAL * 20);
    }

    /// The anti-burst property: a long idle gap must not bank credit, or a
    /// run resuming after a retry would fire a backlog of requests at the
    /// server all at once -- exactly what the pacer exists to prevent.
    #[test]
    fn reserve_does_not_pay_back_idle_time() {
        let reserved_at = Instant::now();
        let (_, next_allowed) = reserve(reserved_at, reserved_at, 1, INTERVAL);
        let much_later = reserved_at + Duration::from_secs(3600);

        let (wait, next) = reserve(next_allowed, much_later, 1, INTERVAL);

        assert_eq!(wait, much_later, "no credit is banked for the idle hour");
        assert_eq!(next, much_later + INTERVAL);
    }

    /// Two concurrent reservations get distinct, non-overlapping windows
    /// rather than both firing at once.
    #[test]
    fn reserve_staggers_two_concurrent_batches() {
        let start = Instant::now();

        let (first_wait, after_first) = reserve(start, start, 5, INTERVAL);
        let (second_wait, _) = reserve(after_first, start, 5, INTERVAL);

        assert_eq!(first_wait, start);
        assert_eq!(second_wait, start + INTERVAL * 5);
    }

    /// End-to-end on a virtual clock (the `test-util` dev-dependency
    /// CLI-0144 added), so the real backoff is asserted without sleeping it.
    #[tokio::test(start_paused = true)]
    async fn pacer_acquire_advances_the_clock_by_the_reserved_time() {
        let pacer = Pacer::new(120);
        let start = Instant::now();

        // The first acquire's window opens immediately; the second must
        // wait out the four intervals the first reserved.
        pacer.acquire(4).await;
        assert_eq!(Instant::now(), start);

        pacer.acquire(1).await;
        assert_eq!(Instant::now(), start + INTERVAL * 4);
    }

    /// A zero count still consumes one slot rather than resolving to a free
    /// unpaced request.
    #[tokio::test(start_paused = true)]
    async fn pacer_acquire_charges_at_least_one_slot() {
        let pacer = Pacer::new(120);
        let start = Instant::now();

        pacer.acquire(0).await;
        pacer.acquire(0).await;

        assert_eq!(Instant::now(), start + INTERVAL);
    }
}
