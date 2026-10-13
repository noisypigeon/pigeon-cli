//! Serialised recovery from an outage at either end of the pipeline
//! (ADR-0146 §4) -- ADR-0144's `EndpointGate` lesson applied before a
//! failed run rather than after one.
//!
//! Whichever task first observes a classified failure wins an election and
//! becomes *the prober*: it closes the gate, sleeps a backoff round, then
//! reopens it. Every other task failing the same way during that window
//! parks on the gate instead of issuing its own doomed request, so one
//! outage costs the run one wait rather than one wait per in-flight item.
//!
//! Like ADR-0144's gate and unlike ADR-0121/0143's transcode circuit
//! breaker, this **pauses and resumes** rather than aborting: a transient
//! server fault warrants patience, not abandonment. And as there, `rounds`
//! is never reset -- once the budget is spent the gate latches
//! open-and-giving-up so the remaining work fails immediately instead of
//! every successive batch re-paying the whole budget, which is the real
//! amplifier a long outage would otherwise cause.
//!
//! Written generic over its classifier and budget because this job uses it
//! twice (IMAP leg and push leg). A third consumer should hoist it into
//! `core::rclone`, which already owns one of the two classifiers --
//! deferred per ADR-0031 rather than hoisted on the second.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::Instant;

/// Substrings marking a server that is up but refusing work right now --
/// rate limited, connection capped, or over quota -- as opposed to one that
/// is unreachable (`core::rclone::is_endpoint_unreachable_error` covers
/// that, and the push leg reuses it directly).
///
/// Matched case-insensitively against the error text, and promoted to a
/// named const per ADR-0143. Unlike that ADR's guards these match *server*
/// text rather than pigeon's own, so they are inherently
/// provider-dependent: the list is a best-effort widening of the one real
/// signature ADR-0071 observed ("Too many simultaneous connections"), and a
/// provider phrasing not listed here simply falls through to the ordinary
/// batch retry rather than the gate.
const SERVER_OVERLOADED_SIGNATURES: &[&str] = &[
    "too many simultaneous connections",
    "too many connections",
    "too many requests",
    "try again later",
    "temporarily unavailable",
    "server unavailable",
    "over quota",
    "rate limit",
    "throttl",
];

/// Whether `err` describes a server refusing work rather than a genuine
/// protocol or credential failure. Kept deliberately separate from
/// `core::rclone::is_endpoint_unreachable_error`: the two answer the same
/// question about different endpoints, and conflating them would park an
/// IMAP failure on the push gate.
pub(crate) fn is_server_overloaded_error(err: &str) -> bool {
    let lowered = err.to_ascii_lowercase();
    SERVER_OVERLOADED_SIGNATURES
        .iter()
        .any(|signature| lowered.contains(signature))
}

/// What a task should do after `RecoveryGate::wait_for_recovery` returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GateDecision {
    /// The gate has reopened -- try again.
    Retry,
    /// The endpoint stayed unreachable across every round; stop waiting and
    /// let this item fail.
    GiveUp,
}

pub(crate) struct RecoveryGate {
    /// `false` while a recovery round is in progress. Non-prober tasks park
    /// on this rather than retrying.
    open: watch::Sender<bool>,
    /// Election: whoever wins `try_lock` is the prober for this round and
    /// holds it for the round's whole duration.
    prober: tokio::sync::Mutex<()>,
    /// Rounds run so far. Read without the election lock by parked tasks,
    /// so it has to be atomic.
    rounds: AtomicUsize,
    /// How many rounds to spend before latching into `GiveUp`.
    max_rounds: usize,
    /// Scaled linearly by round number, matching
    /// `core::retry::retry_with_backoff`'s schedule.
    backoff: Duration,
    /// Names this gate's endpoint in its log lines, e.g. `"imap"`.
    leg: &'static str,
}

impl RecoveryGate {
    pub(crate) fn new(leg: &'static str, max_rounds: usize, backoff: Duration) -> Self {
        Self {
            open: watch::Sender::new(true),
            prober: tokio::sync::Mutex::new(()),
            rounds: AtomicUsize::new(0),
            max_rounds,
            backoff,
            leg,
        }
    }

    /// Called by a task whose request just failed with an endpoint-level
    /// error. Returns once it is worth retrying, or `GiveUp` once the
    /// endpoint has stayed down across every round.
    pub(crate) async fn wait_for_recovery(&self, subject: &str, err: &str) -> GateDecision {
        match self.prober.try_lock() {
            Ok(_election) => {
                // Close the gate before anything else. A task failing in
                // the window between the `try_lock` above and this send
                // sees an open gate and retries once against an endpoint
                // that is still down -- harmless, since it just re-enters
                // here and parks, but worth knowing it is possible
                // (ADR-0144 recorded the same race).
                let _ = self.open.send(false);

                let round = self.rounds.fetch_add(1, Ordering::SeqCst) + 1;
                if round > self.max_rounds {
                    let _ = self.open.send(true);
                    return GateDecision::GiveUp;
                }

                tracing::warn!(
                    pigeon_job = "email-import",
                    leg = self.leg,
                    subject,
                    round,
                    rounds = self.max_rounds,
                    error = %err,
                    "email-import: endpoint refusing work; pausing"
                );

                let paused = Instant::now();
                tokio::time::sleep(self.backoff * round as u32).await;
                let _ = self.open.send(true);

                tracing::info!(
                    pigeon_job = "email-import",
                    leg = self.leg,
                    round,
                    paused_ms = paused.elapsed().as_millis(),
                    "email-import: resuming"
                );
                GateDecision::Retry
            }
            Err(_) => {
                let mut open = self.open.subscribe();
                while !*open.borrow_and_update() {
                    if open.changed().await.is_err() {
                        break;
                    }
                }
                if self.rounds.load(Ordering::SeqCst) > self.max_rounds {
                    GateDecision::GiveUp
                } else {
                    GateDecision::Retry
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    const BACKOFF: Duration = Duration::from_secs(60);

    #[test]
    fn is_server_overloaded_error_matches_the_signature_adr_0071_actually_observed() {
        assert!(is_server_overloaded_error(
            "failed to connect: Too many simultaneous connections"
        ));
    }

    #[test]
    fn is_server_overloaded_error_matches_every_named_signature_case_insensitively() {
        for signature in SERVER_OVERLOADED_SIGNATURES {
            let shouted = signature.to_ascii_uppercase();
            assert!(
                is_server_overloaded_error(&format!("server said: {shouted}")),
                "{signature} must match regardless of case"
            );
        }
    }

    #[test]
    fn is_server_overloaded_error_rejects_an_unrelated_failure() {
        // A credential failure must reach the ordinary failure path, not
        // park every worker on the gate for 21 minutes first.
        assert!(!is_server_overloaded_error(
            "LOGIN failed: invalid credentials"
        ));
        assert!(!is_server_overloaded_error("failed to parse BODYSTRUCTURE"));
    }

    /// An unreachable-endpoint error belongs to the *other* classifier;
    /// conflating the two would park an IMAP failure on the push gate.
    #[test]
    fn is_server_overloaded_error_does_not_claim_an_unreachable_endpoint_error() {
        let unreachable = "dial tcp: lookup s3.fr-par.scw.cloud: no such host";
        assert!(!is_server_overloaded_error(unreachable));
        assert!(crate::core::rclone::is_endpoint_unreachable_error(
            unreachable
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn first_round_sleeps_the_backoff_and_then_retries() {
        let gate = RecoveryGate::new("imap", 6, BACKOFF);
        let start = Instant::now();

        let decision = gate.wait_for_recovery("INBOX", "rate limit exceeded").await;

        assert_eq!(decision, GateDecision::Retry);
        assert_eq!(Instant::now(), start + BACKOFF, "round 1 sleeps 1x backoff");
    }

    /// The whole point of the election: a second failing task must pay the
    /// prober's wait rather than start its own round.
    #[tokio::test(start_paused = true)]
    async fn a_task_losing_the_election_parks_until_the_prober_reopens() {
        let gate = Arc::new(RecoveryGate::new("imap", 6, BACKOFF));
        let start = Instant::now();

        let prober = {
            let gate = Arc::clone(&gate);
            tokio::spawn(async move { gate.wait_for_recovery("INBOX", "rate limit").await })
        };
        // Let the prober win the election and close the gate.
        tokio::task::yield_now().await;

        let parked = gate.wait_for_recovery("Archive", "rate limit").await;

        assert_eq!(parked, GateDecision::Retry);
        assert_eq!(prober.await.unwrap(), GateDecision::Retry);
        assert_eq!(
            Instant::now(),
            start + BACKOFF,
            "one shared wait, not two rounds"
        );
        assert_eq!(
            gate.rounds.load(Ordering::SeqCst),
            1,
            "the parked task must not have started its own round"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn backoff_scales_linearly_with_the_round_number() {
        let gate = RecoveryGate::new("imap", 6, BACKOFF);
        let start = Instant::now();

        gate.wait_for_recovery("INBOX", "rate limit").await;
        gate.wait_for_recovery("INBOX", "rate limit").await;
        gate.wait_for_recovery("INBOX", "rate limit").await;

        // 1x + 2x + 3x
        assert_eq!(Instant::now(), start + BACKOFF * 6);
    }

    /// Past the budget the gate must latch, so remaining items fail fast
    /// instead of each re-paying the full recovery cost (ADR-0144).
    #[tokio::test(start_paused = true)]
    async fn gives_up_past_the_budget_and_stays_latched() {
        let gate = RecoveryGate::new("imap", 2, BACKOFF);

        assert_eq!(
            gate.wait_for_recovery("INBOX", "rate limit").await,
            GateDecision::Retry
        );
        assert_eq!(
            gate.wait_for_recovery("INBOX", "rate limit").await,
            GateDecision::Retry
        );

        let latched_at = Instant::now();
        for _ in 0..3 {
            assert_eq!(
                gate.wait_for_recovery("INBOX", "rate limit").await,
                GateDecision::GiveUp
            );
        }
        assert_eq!(
            Instant::now(),
            latched_at,
            "a latched gate must not sleep again"
        );
    }

    /// A latched gate must also be `GiveUp` for the *parked* branch, not
    /// just the prober's -- these are two separate return paths.
    #[tokio::test(start_paused = true)]
    async fn a_parked_task_also_gives_up_once_the_budget_is_spent() {
        let gate = Arc::new(RecoveryGate::new("imap", 1, BACKOFF));

        // Spend the budget, then push `rounds` past it.
        gate.wait_for_recovery("INBOX", "rate limit").await;
        assert_eq!(
            gate.wait_for_recovery("INBOX", "rate limit").await,
            GateDecision::GiveUp
        );

        let prober = {
            let gate = Arc::clone(&gate);
            tokio::spawn(async move { gate.wait_for_recovery("INBOX", "rate limit").await })
        };
        tokio::task::yield_now().await;

        assert_eq!(
            gate.wait_for_recovery("Archive", "rate limit").await,
            GateDecision::GiveUp
        );
        assert_eq!(prober.await.unwrap(), GateDecision::GiveUp);
    }
}
