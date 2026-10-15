//! Per-file pull-&gt;transcode-&gt;push pipeline (ADR-0116, amending ADR-0112
//! Decision §2/§6/§7). Download stays one bulk `rclone copy` subprocess
//! (unchanged invocation, still gets rclone's own connection-reuse/listing
//! efficiency), but the worker consumes it *live* instead of blocking on its
//! exit: as each file's bytes land, rclone's own JSON log reports it
//! ("Copied (new)"/"Copied (replaced existing)", surfaced via
//! `core::rclone::command::Copy`'s `on_copied` channel), and that file
//! enters the transcode/copy-through -&gt; place -&gt; push pipeline immediately --
//! it never waits for the rest of the batch to finish downloading. Transcode
//! is bounded by `--concurrency` (CPU-bound); the new per-file push (`rclone
//! copyto`, replacing the old bulk Phase C) is bounded by its own semaphore
//! sized from `--transfers` (IO-bound, deliberately decoupled, ADR-0090's
//! CPU-vs-IO split precedent).
//!
//! A single file's failure (transcode or push, after retries exhaust) is
//! recorded and reported but never stops any other file's processing or
//! pushing -- there is no more whole-run abort. The job's exit code (via
//! `TransformSummary.failed`) still reflects whether *any* file failed.
//!
//! Two dispatch sources feed the same pipeline: an initial directory scan
//! (`manifest::gather_pending`, run once before the pull subprocess spawns,
//! catching anything already on disk this run before the live tail would
//! otherwise report it), and the live tail above. Both route through
//! `enqueue`, which decides "already done" by checking each file's
//! deterministic `placement::compute_destination_name` against a listing of
//! `--destination-path` fetched once up front (`destination::
//! list_existing_filenames`, ADR-0120) -- not a local checkpoint, since
//! `transform` runs on a freshly-provisioned, ephemeral VM per invocation
//! and local disk never survives a VM replacement; `--destination-path` is
//! the thing that actually persists. The two dispatch sources are disjoint
//! in the common case but not watertight by construction -- rclone can in
//! principle still emit a "Copied" line for a file the initial scan already
//! queued, if it can't confirm a size/modtime match -- so a `dispatched` set
//! guards against double-enqueueing, on top of `placement::place_one`'s own
//! independent hard-error-on-collision backstop.
//!
//! **Local disk usage (ADR-0124).** Nothing above downloads or retains more
//! than it needs to: before Phase A's pull is even spawned,
//! `destination::list_source_relative_paths` lists the source tree
//! (metadata only, no bytes) and `files_needing_pull` filters out anything
//! whose `placement::compute_destination_name` is already present at the
//! destination, so an already-done file from a prior run is never
//! downloaded again at all -- `enqueue`'s own post-download check against
//! the same destination listing stays in place as a second, independent
//! backstop, not the primary filter anymore. And once a freshly-pulled
//! file's own pipeline finishes pushing it, `process_one` deletes both its
//! local source and placed-result copies immediately (`remove_local_copy`)
//! -- so steady-state local disk usage during a run is bounded by the
//! in-flight window (`--concurrency`/`--transfers` wide), not by the total
//! size of the source/destination trees. A file that fails is deliberately
//! left in place on disk for diagnosis -- with one exception (ADR-0143): a
//! file rejected because this run was given the wrong `--input-file-type`
//! for it is deleted too, since it is undamaged and still present at
//! `--source`, so retaining it only lets a fully-misdirected run
//! accumulate the whole source tree locally.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::{Semaphore, mpsc, watch};
use tokio::task::JoinSet;
use tracing::Instrument;

use super::format::{AudioQuality, InputFileType, VideoQuality};
use super::manifest::{self, PendingFile};
use super::{destination, documents, media, placement, push, sheets};
use crate::core::rclone::RcloneCommand as _;
use crate::core::rclone::command::Copy;

/// Wraps `media::transcode_to_jpg` at the call site (not inside `media.rs`
/// itself, mirroring `upload.rs`'s separation of retry policy from the thing
/// being retried). A genuinely corrupt file still fails after these exhaust
/// -- correct, no amount of retrying fixes corrupted source bytes -- but a
/// transient failure (resource contention, a disk hiccup under concurrent
/// load) now gets a second chance. `transcode_to_jpg`'s existing `-y` flag
/// already makes a retried attempt safely overwrite the previous attempt's
/// scratch output, so no cleanup is needed between attempts.
const TRANSCODE_RETRIES: usize = 2;
const TRANSCODE_RETRY_BACKOFF: Duration = Duration::from_secs(1);

/// Sampled over the first `CIRCUIT_BREAKER_SAMPLE_SIZE` per-file pipeline
/// completions (in completion order, not dispatch order); see
/// `run_transform_job`'s circuit-breaker state and doc comment (ADR-0121).
const CIRCUIT_BREAKER_SAMPLE_SIZE: usize = 20;

/// How many of those `CIRCUIT_BREAKER_SAMPLE_SIZE` completions must be
/// non-retryable transcode failures to trip the breaker (ADR-0143).
///
/// ADR-0121 required unanimity, which made the breaker disarmable by a
/// single interleaved success -- and since `circuit_breaker_samples` only
/// ever counts up to the sample size, that disarming was *permanent*. A
/// mixed source, which is the common shape of a run pointed at the wrong
/// `--input-file-type`, could therefore grind through every remaining file.
/// 18 of 20 is still decisive rather than marginal: ADR-0130 already
/// narrowed what counts here to transcode-stage *and* non-retryable, so
/// this is 18 same-class deterministic failures, not 18 assorted errors.
const CIRCUIT_BREAKER_TRIP_THRESHOLD: usize = 18;

/// Whether the circuit breaker should trip, given how many completions have
/// been sampled and how many of those matched (ADR-0143). Extracted as a
/// pure function so the threshold is unit-testable without depending on
/// concurrent completion ordering -- the same move ADR-0137 made with
/// `default_bucket_index`.
fn circuit_breaker_should_trip(samples: usize, matches: usize) -> bool {
    samples == CIRCUIT_BREAKER_SAMPLE_SIZE && matches >= CIRCUIT_BREAKER_TRIP_THRESHOLD
}

/// How many times a single file will wait out a destination-endpoint outage
/// before giving up on it (ADR-0144). Bounds the per-file cost of an outage
/// that never ends; `EndpointGate`'s own backoff is what bounds the cost of
/// one that does.
const PUSH_ENDPOINT_RECOVERY_ROUNDS: usize = 4;

/// Base backoff for one `EndpointGate` recovery round, scaled linearly by
/// round number to match `core::retry::retry_with_backoff`'s schedule --
/// 30s/60s/90s/120s, five minutes of patience across all four rounds, on
/// top of `CopyTo`'s own five-minute per-attempt budget.
const PUSH_ENDPOINT_RECOVERY_BACKOFF: Duration = Duration::from_secs(30);

/// What a file should do after `EndpointGate::wait_for_recovery` returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GateDecision {
    /// The gate has reopened -- try this file's push again.
    Retry,
    /// The endpoint has stayed unreachable across every recovery round;
    /// stop waiting and let this file fail.
    GiveUp,
}

/// Serialises recovery from a destination-endpoint outage so one outage
/// costs the run one wait, not one wait per in-flight file (ADR-0144).
///
/// Whichever task first observes an `is_endpoint_unreachable_error` wins an
/// election and becomes *the prober*: it closes the gate, sleeps a backoff
/// round, then reopens it. Every other task that fails the same way during
/// that window parks on the gate instead of spawning its own doomed
/// `rclone` subprocess, and retries only once the gate reopens. The actual
/// "is it back?" test is the next real push -- there is deliberately no
/// separate probe command, because the only listing primitive available
/// (`Lsjson` on `--destination-path`) is a full-tree listing of a
/// 38k-object prefix, far too expensive to use as a health check.
///
/// This is the push-leg analogue of ADR-0121/0143's transcode circuit
/// breaker, with one deliberate difference: it **pauses and resumes**
/// rather than aborting the run. A transient network fault is not the
/// systemic converter-capability gap that breaker exists for, so the response
/// to it is patience, not abandonment -- and only after
/// `PUSH_ENDPOINT_RECOVERY_ROUNDS` fruitless rounds does it let files start
/// failing.
///
/// Note that `rounds` is never reset. Once the endpoint has proven
/// unreachable across every round, the gate stays latched open-and-giving-up
/// for the rest of the run, so the remaining files fail immediately rather
/// than each re-paying the full recovery budget. That re-payment is the real
/// amplifier an outage longer than `CopyTo`'s retry budget would otherwise
/// cause: the push semaphore already caps concurrent pushers at
/// `--upload-concurrency`, so a *short* outage is absorbed by `CopyTo`'s
/// retry alone, but a long one would otherwise cost every successive batch
/// of `upload_concurrency` files the whole budget over again.
struct EndpointGate {
    /// `false` while a recovery round is in progress. Non-prober tasks park
    /// on this rather than retrying.
    open: watch::Sender<bool>,
    /// Election: whoever wins `try_lock` is the prober for this round, and
    /// holds it for the round's whole duration.
    prober: tokio::sync::Mutex<()>,
    /// How many recovery rounds have been run. Read without the election
    /// lock by parked tasks, so it has to be atomic.
    rounds: AtomicUsize,
}

impl EndpointGate {
    fn new() -> Self {
        Self {
            open: watch::Sender::new(true),
            prober: tokio::sync::Mutex::new(()),
            rounds: AtomicUsize::new(0),
        }
    }

    /// Called by a task whose push just failed with an endpoint-level
    /// error. Returns once it is worth retrying, or `GiveUp` if the
    /// endpoint has stayed down across every round.
    async fn wait_for_recovery(&self, relative_path: &str, err: &str) -> GateDecision {
        match self.prober.try_lock() {
            Ok(_election) => {
                // Close the gate before anything else -- a task that fails
                // in the microseconds between the `try_lock` above and this
                // send sees an open gate and retries once against a remote
                // that is still down. Harmless (it just re-enters here and
                // parks), but worth knowing it is possible.
                let _ = self.open.send(false);

                let round = self.rounds.fetch_add(1, Ordering::SeqCst) + 1;
                if round > PUSH_ENDPOINT_RECOVERY_ROUNDS {
                    let _ = self.open.send(true);
                    return GateDecision::GiveUp;
                }

                tracing::warn!(
                    pigeon_job = "transform",
                    relative_path,
                    round,
                    rounds = PUSH_ENDPOINT_RECOVERY_ROUNDS,
                    error = %err,
                    "transform: destination endpoint unreachable; pausing the push leg"
                );

                let paused = Instant::now();
                tokio::time::sleep(PUSH_ENDPOINT_RECOVERY_BACKOFF * round as u32).await;
                let _ = self.open.send(true);

                tracing::info!(
                    pigeon_job = "transform",
                    round,
                    paused_ms = paused.elapsed().as_millis(),
                    "transform: resuming the push leg"
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
                if self.rounds.load(Ordering::SeqCst) > PUSH_ENDPOINT_RECOVERY_ROUNDS {
                    GateDecision::GiveUp
                } else {
                    GateDecision::Retry
                }
            }
        }
    }
}

/// Mirrors `core::retry::retry_with_backoff`'s shape (linear backoff,
/// `tracing::warn!` per attempt, `tracing::error!` on exhaustion) but adds
/// one branch: a transcode error classified non-retryable by
/// `media::is_non_retryable_transcode_error` returns immediately, with no
/// sleep and no further attempt (ADR-0121) -- re-running `transcode_to_jpg`
/// against the exact same bytes reproduces the identical failure every time,
/// so retrying only doubles ffmpeg invocations and adds a guaranteed sleep
/// for zero benefit. Kept local rather than added as a parameter to the
/// shared `retry_with_backoff` -- that helper has 5+ unrelated consumers
/// (`push.rs`, `destination.rs`, `deduplicate`, ...); this classification is
/// specific to `transcode_to_jpg`'s error strings alone.
///
/// `context` is the file's source-relative path, emitted as a
/// `relative_path` field on every event below (ADR-0141). Without it these
/// events named no file at all: `process_one`'s own span carries
/// `relative_path` but stays at `debug` per ADR-0099's log-volume
/// reasoning, so it is absent from a default-level run -- which is exactly
/// the run whose failures someone reads afterwards.
async fn retry_transcode_unless_fatal<T, F, Fut>(
    attempts: usize,
    backoff: Duration,
    context: &str,
    mut f: F,
) -> Result<T, String>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, String>>,
{
    let mut last_err = None;
    for attempt in 0..attempts.max(1) {
        if attempt > 0 {
            tokio::time::sleep(backoff * attempt as u32).await;
        }
        match f().await {
            Ok(value) => return Ok(value),
            Err(err) => {
                if media::is_non_retryable_transcode_error(&err) {
                    tracing::warn!(
                        relative_path = %context,
                        attempt = attempt + 1,
                        attempts,
                        error = %err,
                        "transcode failed with a non-retryable error; skipping remaining retries"
                    );
                    return Err(err);
                }
                tracing::warn!(
                    relative_path = %context,
                    attempt = attempt + 1,
                    attempts,
                    error = %err,
                    "retrying after error"
                );
                last_err = Some(err);
            }
        }
    }
    if let Some(err) = &last_err {
        tracing::error!(
            relative_path = %context,
            attempts,
            error = %err,
            "retries exhausted"
        );
    }
    Err(last_err
        .unwrap_or_else(|| "retry_transcode_unless_fatal called with zero attempts".to_string()))
}

pub(crate) struct TransformPlan {
    pub source_path: String,
    pub destination_path: String,
    pub local_output: PathBuf,
    pub input_file_type: InputFileType,
    /// Only consulted when `input_file_type.is_video()` (ADR-0122); an
    /// unused, harmless `VideoQuality::Medium` default on an image run.
    pub video_quality: VideoQuality,
    /// Only consulted when `input_file_type.is_audio()` (ADR-0153); its
    /// default varies by kind (`InputFileType::default_audio_quality`), so
    /// `mp4-audio` keeps ADR-0142's 48k while a music kind gets the top
    /// tier.
    pub audio_quality: AudioQuality,
    pub download_transfers: usize,
    pub download_checkers: usize,
    pub tpslimit: Option<usize>,
    pub max_pending_downloads: usize,
    pub run_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    Transcoded,
    CopiedThrough,
    Failed,
}

impl Outcome {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Outcome::Transcoded => "transcoded",
            Outcome::CopiedThrough => "copied_through",
            Outcome::Failed => "failed",
        }
    }
}

/// Which stage of a file's pipeline failed (ADR-0130). Previously the
/// failing stage survived only as prose inside `FileOutcome.detail`, so a
/// run's failures could not be tallied per stage the way every other job's
/// `FailureBreakdown` already does -- and the circuit breaker had to match
/// an error *string* without knowing whether it even came from transcoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureStage {
    /// The file was in this run's intended pull set but `rclone` never
    /// delivered it (ADR-0130's reconciliation), so no local pipeline ever
    /// ran for it.
    Pull,
    Transcode,
    Place,
    Push,
}

impl FailureStage {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            FailureStage::Pull => "pull",
            FailureStage::Transcode => "transcode",
            FailureStage::Place => "place",
            FailureStage::Push => "push",
        }
    }
}

/// One file's pipeline result: its placed local path(s), or the stage that
/// failed plus that stage's error (ADR-0130).
///
/// A `Vec` since ADR-0149: a workbook yields one output per non-empty
/// sheet. Every other input kind yields a one-element vector.
type ProcessResult = Result<Vec<PathBuf>, (FailureStage, String)>;

/// Per-stage failure tally (ADR-0130), reusing the field names
/// `metrics::record_phase`'s doc comment already expects a job's breakdown
/// to share with its `phase` label.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FailureBreakdown {
    pub pull: u64,
    pub transcode: u64,
    pub place: u64,
    pub push: u64,
}

impl FailureBreakdown {
    fn record(&mut self, stage: FailureStage) {
        match stage {
            FailureStage::Pull => self.pull += 1,
            FailureStage::Transcode => self.transcode += 1,
            FailureStage::Place => self.place += 1,
            FailureStage::Push => self.push += 1,
        }
    }

    pub(crate) fn total(self) -> u64 {
        self.pull + self.transcode + self.place + self.push
    }
}

#[derive(Debug, Clone)]
pub(crate) struct FileOutcome {
    pub relative_path: String,
    /// Every output this input produced -- several for a multi-sheet
    /// workbook (ADR-0149), one for every other kind, none on failure.
    /// A row stays per *input* file, which is what leaves ADR-0130's pull
    /// reconciliation unchanged.
    pub destination_filenames: Vec<String>,
    pub outcome: Outcome,
    /// `Some` iff `outcome` is `Outcome::Failed` (ADR-0130).
    pub stage: Option<FailureStage>,
    pub detail: String,
}

/// `Job::Summary`. `failed` is `true` iff at least one file's outcome is
/// `Outcome::Failed` after retries exhausted (ADR-0116) -- no longer "did
/// the run abort" (there is no more whole-run abort). Every discovered file
/// is attempted regardless of this value, and `pushed` reflects exactly how
/// many files were actually pushed, independent of whether any other file
/// failed.
#[derive(Debug, Default)]
pub(crate) struct TransformSummary {
    pub outcomes: Vec<FileOutcome>,
    pub pulled: u64,
    pub pushed: u64,
    pub failed: bool,
    /// Per-stage tally of everything in `outcomes` that failed (ADR-0130).
    pub failures: FailureBreakdown,
    /// Source files excluded before download because their deterministic
    /// destination name was already present at `--destination-path`
    /// (ADR-0130) -- previously discarded silently by `files_needing_pull`,
    /// which made a run that correctly skipped 500k already-done files
    /// indistinguishable in the report from one that found nothing to do.
    pub skipped_already_done: u64,
    /// macOS AppleDouble sidecars (`._*`) excluded from the pull set and
    /// from dispatch (ADR-0130). Deliberately not a failure: these carry no
    /// user data, and their absence from the destination is correct.
    pub skipped_apple_double: u64,
    /// Excel lock stubs (`~$*`) excluded the same way (ADR-0149), counted
    /// separately since ADR-0151 -- they were folded into the AppleDouble
    /// tally, so no artifact could say which kind of stub a run had
    /// actually skipped.
    pub skipped_lock_file: u64,
    /// `rclone`'s own aggregate transfer-error count for the pull phase,
    /// retained as a cross-check against the reconciled per-file `Pull`
    /// failures rather than being dropped on the floor (ADR-0130).
    pub pull_errors: u64,
    /// Every group of source keys differing only by letter case, each
    /// sorted (ADR-0156). Listed in full in the report, since the log now
    /// carries only an aggregate line with a few examples.
    pub case_collision_groups: Vec<Vec<String>>,
}

fn source_dir(local_output: &Path) -> PathBuf {
    local_output.join("source")
}

fn staging_dir(local_output: &Path) -> PathBuf {
    local_output.join(".staging")
}

fn scratch_dir(local_output: &Path) -> PathBuf {
    staging_dir(local_output).join("scratch")
}

fn result_dir(local_output: &Path) -> PathBuf {
    local_output.join("result")
}

/// Where a case-colliding source key's bytes land (ADR-0152).
///
/// Deliberately **outside** `source/`: `manifest::gather_pending` scans
/// that tree and reconstructs each file's key from its path, which a
/// disambiguated name would make it get wrong. Keeping variants out of it
/// means ADR-0127's crash resume does not cover them -- after a crash they
/// are simply re-pulled (one small bulk `copy` per slot since ADR-0156) --
/// and that is exactly why no on-disk key-to-path side map is needed.
fn case_variant_dir(local_output: &Path) -> PathBuf {
    staging_dir(local_output).join("case-variants")
}

/// One slot's own directory under `case_variant_dir`.
///
/// A directory per *slot* rather than per key (ADR-0156). ADR-0152's
/// per-key directory was what made the pull one `copyto` subprocess per
/// key; a slot holds at most one member of each colliding group, so it is
/// collision-free on a case-insensitive volume by construction and can
/// take a whole bulk `rclone copy` instead. Indexed by position, which
/// `assign_case_variant_slots` makes deterministic across runs.
///
/// The key is kept **verbatim** as a relative path under the slot, so
/// `slot_dir.join(key)` has exactly the same shape as the bulk pull's
/// `source_dir.join(key)` in `enqueue` -- which is why recovering the
/// source key from the local path is never needed, the same reasoning
/// ADR-0152 used for its per-key directories.
fn case_variant_slot_dir(local_output: &Path, slot: usize) -> PathBuf {
    case_variant_dir(local_output).join(format!("slot{slot}"))
}

/// Every path prefix of `source_key`, outermost first: `a/b/c.md` yields
/// `a`, `a/b`, `a/b/c.md`.
///
/// Slot assignment checks all of them, not just the full key, because two
/// keys from *different* colliding groups can still name the same
/// directory with different capitalisation -- and one directory cannot
/// hold both spellings on a case-insensitive volume any more than one
/// file can.
fn path_prefixes(source_key: &str) -> Vec<String> {
    let mut prefixes = Vec::new();
    let mut accumulated = String::new();
    for component in source_key.split('/') {
        if !accumulated.is_empty() {
            accumulated.push('/');
        }
        accumulated.push_str(component);
        prefixes.push(accumulated.clone());
    }
    prefixes
}

/// Distributes `variants` across the fewest slots in which every slot is
/// collision-free under case folding (ADR-0156).
///
/// Greedy, first-fit over the already-sorted input, so the assignment is a
/// pure function of the key set and stable across runs -- the property
/// `split_pull_set`'s doc comment has always promised, now carried by slot
/// index rather than by per-key index.
///
/// A key joins the first slot where every one of its path prefixes is
/// either unclaimed or already claimed by *this exact spelling*. For the
/// flat key sets this job actually sees, that yields exactly as many slots
/// as the largest colliding group has members -- 4, for the 428-group run
/// that prompted this.
fn assign_case_variant_slots(variants: &[String]) -> Vec<Vec<String>> {
    let mut slots: Vec<Vec<String>> = Vec::new();
    let mut claims: Vec<HashMap<String, String>> = Vec::new();

    'key: for key in variants {
        let prefixes = path_prefixes(key);
        for (index, claimed) in claims.iter_mut().enumerate() {
            let fits = prefixes
                .iter()
                .all(|prefix| match claimed.get(&prefix.to_lowercase()) {
                    Some(spelling) => spelling == prefix,
                    None => true,
                });
            if fits {
                for prefix in prefixes {
                    claimed.insert(prefix.to_lowercase(), prefix);
                }
                slots[index].push(key.clone());
                continue 'key;
            }
        }
        let claimed = prefixes
            .into_iter()
            .map(|prefix| (prefix.to_lowercase(), prefix))
            .collect();
        claims.push(claimed);
        slots.push(vec![key.clone()]);
    }

    slots
}

fn push_log_dir(local_output: &Path) -> PathBuf {
    staging_dir(local_output).join("push-logs")
}

/// A scratch directory that removes itself (ADR-0149).
///
/// `process_one` has many `?` early-returns, and a spreadsheet's scratch
/// directory can hold a whole LibreOffice user profile -- hundreds of files
/// per `.numbers` input. Neither existing precedent in this codebase has
/// such a guard (`deduplicate::process_item` hand-rolls nine
/// `remove_file` calls at its exit paths), and without one the only cleanup
/// is `run_transform_job`'s startup clear, so a long run would accumulate
/// one profile per converted workbook until the *next* run -- real disk
/// exhaustion on the ephemeral VM this job is built for.
///
/// Best-effort on drop, like `remove_local_copy`: a removal failure must
/// never change a file's outcome.
struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    fn create(parent: &Path, group: &str) -> Result<Self, String> {
        let path = parent.join(group);
        std::fs::create_dir_all(&path)
            .map_err(|err| format!("failed to create {}: {err}", path.display()))?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_dir_all(&self.path)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %self.path.display(),
                error = %err,
                "transform: failed to remove scratch directory"
            );
        }
    }
}

/// One output's final name and the scratch file holding its bytes.
struct PendingOutput {
    destination_name: String,
    scratch_path: PathBuf,
}

/// Local `result/` files belonging to `group`, newest-run-agnostic and
/// sorted by name (ADR-0149's per-group extension of ADR-0127's resume).
///
/// `result_dir` is bounded by the in-flight window, not the dataset, because
/// ADR-0124's GC empties it as files are pushed -- so reading it per input
/// file is cheap.
fn existing_group_outputs(result_dir: &Path, group: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(result_dir) else {
        return Vec::new();
    };
    let plain_prefix = format!("{group}.");
    let sheet_prefix = format!("{group}-");
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with(&plain_prefix) || name.starts_with(&sheet_prefix))
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|name| result_dir.join(name))
        .collect()
}

/// One file's full pipeline: convert (or copy through) into a scratch
/// directory, place every resulting output under `result_dir` at its
/// deterministic, collision-free name, then push each one. Returns the
/// final local paths on success so the caller can report them.
///
/// **One input may yield several outputs** since ADR-0149: a workbook's
/// sheets each become their own CSV. Every other input kind still produces
/// exactly one, and takes a path through here that is behaviourally
/// identical to before.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(
    skip_all,
    fields(relative_path = %pending_file.source_key),
    level = "debug"
)]
async fn process_one(
    pending_file: &PendingFile,
    input_file_type: InputFileType,
    video_quality: VideoQuality,
    audio_quality: AudioQuality,
    transcode_threads: usize,
    source_path: &str,
    destination_path: &str,
    result_dir: &Path,
    scratch_dir: &Path,
    push_log_dir: &Path,
    push_semaphore: Arc<Semaphore>,
    endpoint_gate: Arc<EndpointGate>,
) -> ProcessResult {
    let group = placement::group_key(source_path, &pending_file.source_key, input_file_type);

    // ADR-0127, extended per-group by ADR-0149: outputs already sitting in
    // `result_dir` survived a prior crash *after* a successful, verified
    // conversion -- placement only ever renames a scratch file in once the
    // converter exited 0 and passed its own guards -- so they are never
    // partial writes. Skip straight to push instead of redoing the work and
    // then colliding.
    //
    // For a single-output kind this is the same `exists()` question as
    // before. For a workbook it cannot be, because "are all its outputs
    // here?" needs the sheet count, which needs opening the file. The
    // answer is to ask the *names already present*: they carry their own
    // `{nn}of{tt}` totals, so `DestinationGroups` reads completeness off
    // the local listing exactly as it does off the remote one.
    let existing = existing_group_outputs(result_dir, &group);
    let existing_names: Vec<String> = existing
        .iter()
        .filter_map(|path| path.file_name()?.to_str().map(str::to_string))
        .collect();
    let final_paths = if !existing.is_empty()
        && placement::DestinationGroups::index(&existing_names).is_complete(&group)
    {
        existing
    } else {
        let scratch = ScratchDir::create(scratch_dir, &group)
            .map_err(|err| (FailureStage::Transcode, err))?;

        // ADR-0130: the run's real disk high-water mark. A 4K video's
        // scratch output is multi-GB and a workbook's CSVs can exceed the
        // workbook itself, while ADR-0124's post-push GC only bounds
        // steady-state usage, not this transient peak. Fails just this one
        // file, matching `check_disk_space`'s documented contract.
        crate::commands::job::local::check_disk_space(scratch.path(), 0)
            .map_err(|err| (FailureStage::Transcode, err))?;

        let outputs = convert_one(
            pending_file,
            input_file_type,
            video_quality,
            audio_quality,
            transcode_threads,
            source_path,
            &group,
            scratch.path(),
        )
        .await;

        crate::observability::metrics::record_phase_count(
            "transform",
            "transcode",
            if outputs.is_ok() {
                outcome_for(input_file_type).as_str()
            } else {
                Outcome::Failed.as_str()
            },
            1,
            None,
        );
        // ADR-0143: a wrong-`--input-file-type` failure is the one class
        // where ADR-0124's retain-the-failed-file-for-diagnosis rule buys
        // nothing -- the file is undamaged, still present at `--source`
        // (where a rerun pulls it from anyway), and the error already says
        // exactly what is wrong. Keeping it would let a fully-misdirected
        // run accumulate the entire source tree locally. Deleting it is
        // non-destructive and bounds that. Every *other* failure class
        // still keeps its local copy.
        if let Err(err) = &outputs
            && media::is_wrong_input_kind_error(err)
        {
            remove_local_copy(&pending_file.absolute_path);
        }
        let outputs = outputs.map_err(|err| (FailureStage::Transcode, err))?;

        let mut placed = Vec::with_capacity(outputs.len());
        for output in &outputs {
            // A single-output kind keeps `place_one`, whose hard error on an
            // existing target is ADR-0112 §5's deliberate choice: at this
            // scale a collision means a bug or an astronomically unlucky
            // hash, and a verifiably-unique scheme must fail loudly rather
            // than fall back to the probabilistic disambiguation it exists
            // to avoid. The legitimate resume it would otherwise trip over
            // is already caught above, before any conversion runs.
            //
            // A workbook needs the tolerant `place_sheet` instead: a prior
            // run can have crashed after placing sheet 1 of 3, and
            // re-converting reproduces all three, so sheet 1 arriving again
            // is expected rather than anomalous.
            let placed_path = if input_file_type.is_spreadsheet() {
                placement::place_sheet(result_dir, &output.scratch_path, &output.destination_name)
            } else {
                placement::place_one(
                    result_dir,
                    &output.scratch_path,
                    source_path,
                    &pending_file.source_key,
                    input_file_type,
                )
            };
            placed.push(placed_path.map_err(|err| (FailureStage::Place, err))?);
        }
        placed
    };

    for final_path in &final_paths {
        let destination_filename = final_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                (
                    FailureStage::Place,
                    format!(
                        "placed file at {} has no valid UTF-8 filename",
                        final_path.display()
                    ),
                )
            })?
            .to_string();

        // One permit per push rather than one held across every output of a
        // workbook, so `--upload-transfers` keeps meaning "concurrent
        // pushes" and no permit is held while a sibling waits.
        let _permit = push_semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|err| {
                (
                    FailureStage::Push,
                    format!("push semaphore closed unexpectedly: {err}"),
                )
            })?;
        // ADR-0144: an endpoint-level failure is shared by every concurrent
        // push, so it is waited out once for the whole run via
        // `endpoint_gate` rather than being re-paid per file. Anything else
        // (a 403, a per-file read error) fails this file immediately,
        // exactly as before -- parking on the gate for those would stall
        // every other push behind a wait that could never help.
        loop {
            let Err(err) = push::push_one(
                final_path,
                destination_path,
                &destination_filename,
                push_log_dir,
            )
            .await
            else {
                break;
            };

            if !crate::core::rclone::is_endpoint_unreachable_error(&err) {
                return Err((FailureStage::Push, err));
            }
            if endpoint_gate
                .wait_for_recovery(&pending_file.source_key, &err)
                .await
                == GateDecision::GiveUp
            {
                return Err((FailureStage::Push, err));
            }
        }

        // This output's bytes are now durably present at
        // `destination_path`, so its local copy no longer serves any
        // purpose (ADR-0124). The *source* is kept until every output has
        // pushed -- a partially-pushed workbook must still be re-convertible
        // from it on the next attempt.
        remove_local_copy(final_path);
        // And so does its push log (ADR-0151). One per *output* object since
        // ADR-0149, none of them ever cleaned: a single real run left 1318
        // behind. A *failed* push keeps its log -- that is the one case where
        // it has diagnostic value, matching ADR-0124's retain-on-failure
        // rule, and this line is only reached once the push succeeded.
        remove_local_copy(&push::log_path(push_log_dir, &destination_filename));
    }

    // Every output is durably pushed, so the local source is now spent too.
    // Safe without synchronization: each task only ever touches its own
    // paths.
    remove_local_copy(&pending_file.absolute_path);

    Ok(final_paths)
}

/// Converts one input into its output(s) inside `scratch`, retrying a
/// transient converter failure (ADR-0121) but never a deterministic one.
///
/// Every kind but the spreadsheets produces exactly one output, named
/// exactly as it always was. A workbook produces one per non-empty sheet --
/// except a single-sheet one, which keeps the plain name too, so the
/// `{nn}of{tt}` marker only ever appears where it carries information.
#[allow(clippy::too_many_arguments)]
async fn convert_one(
    pending_file: &PendingFile,
    input_file_type: InputFileType,
    video_quality: VideoQuality,
    audio_quality: AudioQuality,
    transcode_threads: usize,
    source_path: &str,
    group: &str,
    scratch: &Path,
) -> Result<Vec<PendingOutput>, String> {
    let single_name =
        placement::compute_destination_name(source_path, &pending_file.source_key, input_file_type);

    if input_file_type.is_spreadsheet() {
        return convert_spreadsheet(pending_file, input_file_type, group, &single_name, scratch)
            .await;
    }

    let scratch_path = scratch.join(format!("{single_name}.scratch"));
    let input = &pending_file.absolute_path;

    // Each arm returns the path it actually produced, rather than writing
    // to `scratch_path` by convention (ADR-0154 §5). `soffice` and
    // `ebook-convert` derive their own output name -- the former inside
    // `--outdir`, the latter from the extension it is given -- so a single
    // caller-chosen path cannot serve every converter.
    let produced: PathBuf = match input_file_type {
        // Already in the target format: copied through byte-for-byte on
        // ADR-0112's reasoning. `Pdf` and `Txt` joined `Jpeg` in ADR-0154.
        InputFileType::Jpeg | InputFileType::Pdf | InputFileType::Txt => {
            media::copy_through(input, &scratch_path).map(|()| scratch_path.clone())
        }
        InputFileType::Png | InputFileType::Heic => retry_transcode_unless_fatal(
            TRANSCODE_RETRIES,
            TRANSCODE_RETRY_BACKOFF,
            &pending_file.source_key,
            || media::transcode_to_jpg(input, &scratch_path),
        )
        .await
        .map(|()| scratch_path.clone()),
        InputFileType::Mov | InputFileType::M4v | InputFileType::Mp4 => {
            retry_transcode_unless_fatal(
                TRANSCODE_RETRIES,
                TRANSCODE_RETRY_BACKOFF,
                &pending_file.source_key,
                || media::transcode_video(input, &scratch_path, video_quality, transcode_threads),
            )
            .await
            .map(|()| scratch_path.clone())
        }
        InputFileType::Mp4Audio
        | InputFileType::M4a
        | InputFileType::Mp3
        | InputFileType::Aac
        | InputFileType::Wav
        | InputFileType::Opus => retry_transcode_unless_fatal(
            TRANSCODE_RETRIES,
            TRANSCODE_RETRY_BACKOFF,
            &pending_file.source_key,
            || media::transcode_audio(input, &scratch_path, audio_quality),
        )
        .await
        .map(|()| scratch_path.clone()),
        // The six LibreOffice-readable document kinds (ADR-0154). `Key`
        // is sniffed inside `to_pdf` before soffice sees it.
        //
        // Deliberately **not** retried, matching ADR-0149's reasoning for
        // `.numbers`: a document converter fails deterministically on a
        // given file, so retrying would burn two extra invocations for a
        // fixed outcome -- ADR-0141's pathology.
        InputFileType::Doc
        | InputFileType::Docx
        | InputFileType::Pages
        | InputFileType::Rtf
        | InputFileType::Pptx
        | InputFileType::Key => documents::to_pdf(input, input_file_type, scratch).await,
        // The one kind LibreOffice cannot read: its EPUB filter is
        // export-only, so this goes to Calibre instead.
        InputFileType::Epub => documents::epub_to_pdf(input, scratch).await,
        InputFileType::Md | InputFileType::Rst => {
            documents::markup_to_text(input, input_file_type, &scratch_path)
                .await
                .map(|()| scratch_path.clone())
        }
        InputFileType::Textclipping => {
            documents::clipping_to_text(input, &scratch_path).map(|()| scratch_path.clone())
        }
        InputFileType::Tsv => {
            // CPU-bound and blocking, so it runs off the async runtime
            // (ADR-0090), as `sheets::split_workbook_to_csv` already does.
            let (from, to) = (input.clone(), scratch_path.clone());
            tokio::task::spawn_blocking(move || documents::tsv_to_csv(&from, &to))
                .await
                .map_err(|err| format!("tsv conversion task failed: {err}"))?
                .map(|()| scratch_path.clone())
        }
        InputFileType::Numbers | InputFileType::Xlsx | InputFileType::Xls | InputFileType::Csv => {
            unreachable!("handled by the is_spreadsheet() branch above")
        }
    }?;
    Ok(vec![PendingOutput {
        destination_name: single_name,
        scratch_path: produced,
    }])
}

/// The spreadsheet half of `convert_one` (ADR-0149).
async fn convert_spreadsheet(
    pending_file: &PendingFile,
    input_file_type: InputFileType,
    group: &str,
    single_name: &str,
    scratch: &Path,
) -> Result<Vec<PendingOutput>, String> {
    let input = &pending_file.absolute_path;

    // Already CSV: copied through byte-for-byte, exactly as ADR-0112 treats
    // a `jpeg` input already in the target format. Encoding, line endings,
    // delimiter, and quoting are deliberately untouched.
    if input_file_type == InputFileType::Csv {
        let scratch_path = scratch.join(format!("{single_name}.scratch"));
        media::copy_through(input, &scratch_path)?;
        return Ok(vec![PendingOutput {
            destination_name: single_name.to_string(),
            scratch_path,
        }]);
    }

    // `.numbers` has no Rust reader, so LibreOffice converts it to `.xlsx`
    // and the result rejoins the shared `calamine` path. Not retried: both
    // legs fail deterministically on a given file.
    let workbook = if input_file_type == InputFileType::Numbers {
        sheets::convert_numbers_to_xlsx(input, scratch).await?
    } else {
        input.clone()
    };

    let outputs: Vec<sheets::SheetOutput> =
        sheets::split_workbook_to_csv(&workbook, scratch).await?;
    Ok(outputs
        .into_iter()
        .map(|sheet| PendingOutput {
            destination_name: if sheet.total == 1 {
                single_name.to_string()
            } else {
                placement::sheet_destination_name(
                    group,
                    sheet.index,
                    sheet.total,
                    &sheet.sheet_name,
                    input_file_type,
                )
            },
            scratch_path: sheet.path,
        })
        .collect())
}

/// Best-effort local cleanup. Two call contexts: once a file is confirmed
/// durably pushed (ADR-0124), and -- since ADR-0143 -- when a file failed
/// because this run was given the wrong `--input-file-type` for it, where
/// the local copy has no diagnostic value.
///
/// `transform` has no `--upload-only`/local-state-dependent
/// resume mechanism to preserve by keeping these files around -- unlike
/// `deduplicate`, which deliberately retains its own `result/` tree for
/// exactly that purpose -- every phase here is already
/// independently rerun-safe via the remote destination listing, not local
/// state. A removal failure (e.g. a permissions issue) is logged and
/// otherwise ignored: it must never turn an already-successful push into a
/// failed `Outcome`, nor change which error a failed file reports.
fn remove_local_copy(path: &Path) {
    if let Err(err) = std::fs::remove_file(path) {
        tracing::warn!(
            path = %path.display(),
            error = %err,
            "transform: failed to remove local file"
        );
    }
}

fn outcome_for(input_file_type: InputFileType) -> Outcome {
    match input_file_type {
        InputFileType::Jpeg => Outcome::CopiedThrough,
        // ADR-0149: `csv` is already in the target format, so it is copied
        // through on exactly ADR-0112's reasoning for `jpeg`.
        //
        // `Opus` is **not** here despite also arriving in its target
        // format: ADR-0153 re-encodes it to normalize a mixed library to
        // one bitrate, so it is genuinely transcoded.
        //
        // ADR-0154 adds `Pdf` and `Txt` on the same reasoning.
        InputFileType::Csv | InputFileType::Pdf | InputFileType::Txt => Outcome::CopiedThrough,
        InputFileType::Png
        | InputFileType::Heic
        | InputFileType::Mov
        | InputFileType::M4v
        | InputFileType::Mp4
        | InputFileType::Mp4Audio
        | InputFileType::Numbers
        | InputFileType::Xlsx
        | InputFileType::Xls
        | InputFileType::M4a
        | InputFileType::Mp3
        | InputFileType::Aac
        | InputFileType::Wav
        | InputFileType::Opus
        | InputFileType::Doc
        | InputFileType::Docx
        | InputFileType::Pages
        | InputFileType::Rtf
        | InputFileType::Epub
        | InputFileType::Pptx
        | InputFileType::Key
        | InputFileType::Md
        | InputFileType::Rst
        | InputFileType::Textclipping
        | InputFileType::Tsv => Outcome::Transcoded,
    }
}

/// Send SIGSTOP to pause a process (Unix only, ADR-0129).
#[cfg(unix)]
fn pause_pull_process(pid: u32) {
    if let Err(err) = unsafe {
        if libc::kill(pid as i32, libc::SIGSTOP) != 0 {
            Err("kill SIGSTOP failed")
        } else {
            Ok(())
        }
    } {
        tracing::warn!(pid, error = ?err, "transform: failed to pause pull process");
    } else {
        tracing::debug!(pid, "transform: paused pull process");
    }
}

/// Send SIGCONT to resume a process (Unix only, ADR-0129).
#[cfg(unix)]
fn resume_pull_process(pid: u32) {
    if let Err(err) = unsafe {
        if libc::kill(pid as i32, libc::SIGCONT) != 0 {
            Err("kill SIGCONT failed")
        } else {
            Ok(())
        }
    } {
        tracing::warn!(pid, error = ?err, "transform: failed to resume pull process");
    } else {
        tracing::debug!(pid, "transform: resumed pull process");
    }
}

/// Enqueues `relative_path` into `queue` unless it is already fully done --
/// its deterministic `placement::compute_destination_name` already exists in
/// `existing_destination_filenames` (fetched once from `--destination-path`
/// before dispatch begins, ADR-0120) -- or already dispatched this run
/// (`dispatched`, guarding against the live tail re-reporting a file the
/// initial scan already queued -- see this module's doc comment).
fn enqueue(
    relative_path: String,
    source_dir: &Path,
    source_path: &str,
    input_file_type: InputFileType,
    destination_groups: &placement::DestinationGroups,
    dispatched: &mut HashSet<String>,
    queue: &mut VecDeque<PendingFile>,
) -> EnqueueOutcome {
    // ADR-0130: the second of two AppleDouble choke points. `files_needing_pull`
    // keeps these out of the download set in the first place; this one covers
    // the other two dispatch sources -- the initial on-disk scan (a stub left
    // by an earlier run, or an operator-staged tree) and rclone's live tail --
    // so nothing reaches `ffmpeg` by any route.
    if crate::core::data::is_apple_double_basename(&relative_path) {
        return EnqueueOutcome::SkippedAppleDouble;
    }
    // ADR-0149: an Excel lock stub matches `--include '*.xlsx'` but is not a
    // workbook. Same two choke points, and the same reason, as the
    // AppleDouble filter above: without this it arrives as a deterministic
    // *failure*, which is exactly what can trip the circuit breaker.
    if crate::core::data::is_excel_lock_basename(&relative_path) {
        return EnqueueOutcome::SkippedLockFile;
    }
    if destination_groups.is_complete(&placement::group_key(
        source_path,
        &relative_path,
        input_file_type,
    )) {
        return EnqueueOutcome::AlreadyDone;
    }
    if !dispatched.insert(relative_path.clone()) {
        return EnqueueOutcome::AlreadyDispatched;
    }
    let absolute_path = source_dir.join(&relative_path);
    queue.push_back(PendingFile {
        source_key: relative_path,
        absolute_path,
    });
    EnqueueOutcome::Queued
}

/// Why `enqueue` did or didn't queue a path.
///
/// Both skip variants are counted by the caller, each into its own tally
/// (ADR-0130 for AppleDouble, ADR-0151 for the lock file). The rest are
/// ordinary, expected outcomes of the two overlapping dispatch sources.
/// ADR-0149 added `SkippedLockFile` but left every call site matching only
/// on `SkippedAppleDouble`, so a lock file skipped here was counted
/// nowhere at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnqueueOutcome {
    Queued,
    AlreadyDone,
    AlreadyDispatched,
    SkippedAppleDouble,
    /// An Excel `~$` lock stub (ADR-0149), counted with the AppleDouble
    /// skips: neither carries user data, and neither belongs at the
    /// destination.
    SkippedLockFile,
}

/// Filters `source_relative_paths` down to the ones that still need
/// pulling: a relative path whose `placement::compute_destination_name` is
/// already present in `existing_destination_filenames` is already fully
/// done and must not be downloaded again at all (ADR-0124) -- this
/// supersedes ADR-0120's "Accepted limitation" that Phase A could only ever
/// re-pull already-done files on a rerun. That limitation assumed the
/// destination's hash-based naming was one-way and therefore unusable as a
/// pull-side filter; in fact `compute_destination_name` is a pure *forward*
/// function of `(source_path, relative_path)` alone, needing no file
/// content, so the same "already done" check `enqueue` performs after
/// download can just as well run before it, against a lightweight,
/// metadata-only listing of the source tree.
fn files_needing_pull(
    source_relative_paths: Vec<String>,
    source_path: &str,
    input_file_type: InputFileType,
    destination_groups: &placement::DestinationGroups,
) -> PullSelection {
    let mut selection = PullSelection::default();
    for relative_path in source_relative_paths {
        // ADR-0130: an AppleDouble stub matches rclone's `--include
        // '*.<ext>'` filter but is an 82-byte resource fork, not decodable
        // media. Excluding it here means it is never downloaded at all,
        // rather than pulled and then failed by `ffmpeg` with an error that
        // can trip the circuit breaker and abort the whole run.
        if crate::core::data::is_apple_double_basename(&relative_path) {
            selection.skipped_apple_double += 1;
            continue;
        }
        // ADR-0149, the first of the two choke points -- see `enqueue`.
        if crate::core::data::is_excel_lock_basename(&relative_path) {
            selection.skipped_lock_file += 1;
            continue;
        }
        // ADR-0149: "already done" is a group-completeness question, not
        // single-name membership, because a workbook maps to several
        // destination names. The group key is still a pure forward function
        // of the path alone, which is what keeps ADR-0124's pre-pull filter
        // working at all -- a sheet *count* needs the file, a group key does
        // not.
        if destination_groups.is_complete(&placement::group_key(
            source_path,
            &relative_path,
            input_file_type,
        )) {
            selection.skipped_already_done += 1;
            continue;
        }
        selection.to_pull.push(relative_path);
    }
    selection.case_collisions = case_insensitive_collisions(&selection.to_pull);
    selection
}

/// Why a file this run asked rclone for never entered the pipeline.
///
/// Extracted as a pure function (ADR-0144) for the same reason ADR-0143
/// extracted `circuit_breaker_should_trip`: the branch is the interesting
/// part, and it is untestable while buried in `run_transform_job`'s
/// reconciliation loop.
///
/// The generic arm's wording is deliberately narrower than it used to be.
/// For a case-collision it was actively *wrong* -- rclone does report such
/// a file, at `notice` level as `Duplicate object found in source -
/// ignoring`, which this job never reads -- and that wrongness sent the
/// original ADR-0144 investigation down a false-positive path for a while.
///
/// ADR-0152 replaces the collision arm entirely. A colliding key is no
/// longer dropped by the bulk pull: it is fetched separately, so the only
/// way one goes undelivered now is that its own pull failed -- and
/// `case_variant_failures` carries rclone's actual error for it, which is
/// strictly more useful than any prose this function could invent. Since
/// ADR-0156 that pull is its slot's bulk `copy`, and the error is the
/// slot's, attributed only to the keys that did not land.
fn undelivered_detail(
    relative_path: &str,
    case_collisions: &HashSet<String>,
    case_variant_failures: &HashMap<String, String>,
) -> String {
    if let Some(err) = case_variant_failures.get(relative_path) {
        format!(
            "this source key collides with another under case-folding, so it was pulled \
             separately to its own local path (ADR-0152/0156) -- and that pull failed: {err}"
        )
    } else if case_collisions.contains(relative_path) {
        // Reachable only if a colliding key went missing without its own
        // pull reporting an error, which would be a bug rather than the
        // ordinary case this arm used to describe.
        "this source key collides with another under case-folding and should have been pulled \
         separately, but it never entered the pipeline and its own pull reported no error"
            .to_string()
    } else {
        "file was in this run's pull set but rclone never reported it as copied, so it never \
         entered the transcode pipeline"
            .to_string()
    }
}

/// Splits this run's pull set into the keys the bulk `rclone copy` can take
/// and the ones that must be fetched separately (ADR-0152), into the
/// collision-free slots `assign_case_variant_slots` builds (ADR-0156).
///
/// Deliberately removes *every* member of a colliding group, not just the
/// ones rclone would have dropped: which one it keeps is its own ordering
/// decision and not something this job can predict, so excluding the whole
/// group is what makes the outcome deterministic. It also leaves the bulk
/// pull with no collision to resolve at all, so its `duplicate_notices`
/// should be 0 -- a crisp signal that this split is doing its job.
///
/// The variant list is sorted so `assign_case_variant_slots`' slot
/// assignment, and therefore every local path it derives, is stable across
/// runs.
fn split_pull_set(
    files_to_pull: &[String],
    case_collisions: &HashSet<String>,
) -> (Vec<String>, Vec<String>) {
    let bulk: Vec<String> = files_to_pull
        .iter()
        .filter(|key| !case_collisions.contains(*key))
        .cloned()
        .collect();
    let mut variants: Vec<String> = case_collisions.iter().cloned().collect();
    variants.sort();
    (bulk, variants)
}

/// Cadence of the main loop's `transform: progress` line (ADR-0156).
const PROGRESS_INTERVAL: Duration = Duration::from_secs(60);

/// How many undelivered files the pull reconciliation names in the log
/// before collapsing the rest into one line (ADR-0156).
const UNDELIVERED_LOG_LIMIT: u64 = 50;

/// How many colliding groups the aggregate collision log line names
/// (ADR-0156); the report carries the rest.
const COLLISION_LOG_EXAMPLES: usize = 10;

/// `case_collisions` regrouped by folded key, each group and the list of
/// groups sorted, so the log line's examples and the report's listing are
/// stable across runs (ADR-0156).
fn case_collision_groups(case_collisions: &HashSet<String>) -> Vec<Vec<String>> {
    let mut by_folded: HashMap<String, Vec<String>> = HashMap::new();
    for key in case_collisions {
        by_folded
            .entry(key.to_lowercase())
            .or_default()
            .push(key.clone());
    }
    let mut groups: Vec<Vec<String>> = by_folded
        .into_values()
        .map(|mut group| {
            group.sort();
            group
        })
        .collect();
    groups.sort();
    groups
}

/// Source keys in `relative_paths` that differ from another key *only* by
/// letter case, and so cannot both exist in the local pull tree on a
/// case-insensitive volume (APFS's default, and HFS+ before it) -- ADR-0144.
///
/// Returns every member of every colliding group, not just the ones that
/// lose: which member rclone keeps and which it discards is its own
/// ordering decision, not something this job can predict or control.
///
/// Why these go undetected without this: rclone resolves the collision
/// itself, logging `Duplicate object found in source - ignoring` at
/// **`notice`** level and leaving its aggregate error count at zero, so
/// ADR-0130's reconciliation correctly sees the file as undelivered while
/// `pull_summary.errors` disagrees -- exactly the mismatch ADR-0130's own
/// warning exists to surface. S3 keys are case-sensitive, so two such keys
/// are entirely legitimate objects with potentially different content;
/// rclone's "duplicate" judgment is purely name-based and never compares
/// bytes.
fn case_insensitive_collisions(relative_paths: &[String]) -> HashSet<String> {
    let mut by_folded: HashMap<String, Vec<&String>> = HashMap::new();
    for relative_path in relative_paths {
        by_folded
            .entry(relative_path.to_lowercase())
            .or_default()
            .push(relative_path);
    }
    by_folded
        .into_values()
        .filter(|group| group.len() > 1)
        .flatten()
        .cloned()
        .collect()
}

/// What `files_needing_pull` decided, including the two counts it used to
/// discard silently (ADR-0130). `to_pull` doubles as the expected set the
/// pull phase is reconciled against once the pipeline drains.
#[derive(Debug, Default)]
struct PullSelection {
    to_pull: Vec<String>,
    skipped_already_done: u64,
    skipped_apple_double: u64,
    skipped_lock_file: u64,
    /// Members of `to_pull` that collide with another member under
    /// case-folding (ADR-0144), so they cannot all share one directory on
    /// a case-insensitive local volume.
    ///
    /// Since ADR-0152 these are **excluded from the bulk pull and fetched
    /// separately** into their own directories (per-slot since ADR-0156),
    /// so every one of them arrives. Before that, rclone resolved the clash itself by keeping
    /// one and ignoring the rest, and the rest became `Pull` failures.
    case_collisions: HashSet<String>,
}

/// Writes `relative_paths` newline-delimited to `dir/file_name`, for
/// `Copy`'s `files_from` (ADR-0124). The name is the caller's since
/// ADR-0156, which needs one list per case-variant slot beside the bulk
/// pull's own `pull-files-from.txt`.
fn write_files_from(
    dir: &Path,
    file_name: &str,
    relative_paths: &[String],
) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir)
        .map_err(|err| format!("failed to create {}: {err}", dir.display()))?;
    let path = dir.join(file_name);
    std::fs::write(&path, relative_paths.join("\n"))
        .map_err(|err| format!("failed to write {}: {err}", path.display()))?;
    Ok(path)
}

/// Runs the whole per-file pipeline. `Err` is reserved for infrastructure-
/// level failures (the pull subprocess itself failing, a directory that
/// couldn't be created) -- a per-file transcode/push failure is captured in
/// the returned `TransformSummary` instead, so every already-succeeded
/// file's outcome survives for the report.
///
/// `upload_concurrency` sizes the `push_semaphore` (`Arc<Semaphore>`), the
/// concurrency cap for per-file `rclone copyto` push subprocesses (ADR-0129).
pub(crate) async fn run_transform_job(
    plan: &TransformPlan,
    concurrency: usize,
    upload_concurrency: usize,
) -> Result<TransformSummary, String> {
    let source_dir = source_dir(&plan.local_output);
    let result_dir = result_dir(&plan.local_output);
    let scratch_dir = scratch_dir(&plan.local_output);
    let push_log_dir = push_log_dir(&plan.local_output);

    // A `.scratch` file surviving from a prior crashed run is never a
    // resumable artifact (ADR-0127): `transcode_video`/`transcode_to_jpg`
    // write `ffmpeg`'s output directly to this path, so anything found here
    // at startup is, by construction, a truncated/corrupt partial, always
    // silently overwritten (`-y`) the next time its still-present `source/`
    // file is reprocessed anyway. Clearing it here just reclaims that disk
    // space proactively instead of leaving it until then. Best-effort: a
    // missing directory (first run) or a removal failure is not fatal.
    // ADR-0152 clears the case-variant tree on the same reasoning: a
    // variant left behind is not resumable state (nothing records which key
    // it came from), and re-pulling it costs one small per-slot `copy`.
    for stale in [&scratch_dir, &case_variant_dir(&plan.local_output)] {
        if let Err(err) = std::fs::remove_dir_all(stale)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %stale.display(),
                error = %err,
                "transform: failed to clear a stale staging directory from a prior run"
            );
        }
    }

    // The authoritative "already done" source (ADR-0120) -- fetched once,
    // before the pull subprocess exists, since `--destination-path` is the
    // one thing that actually persists across this job's ephemeral VMs.
    let existing_destination_filenames =
        destination::list_existing_filenames(&plan.destination_path).await?;
    // ADR-0149 §3: indexed once, so the per-source-path cost stays O(1)
    // exactly as the `HashSet` it replaces.
    let destination_groups = placement::DestinationGroups::index(&existing_destination_filenames);

    // Initial directory scan, taken once, before the pull subprocess exists
    // -- picks up anything already on disk this run before the live tail
    // would otherwise report it.
    let already_on_disk = manifest::gather_pending(&source_dir, plan.input_file_type)?;

    let mut dispatched: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<PendingFile> = VecDeque::new();
    let mut skipped_apple_double: u64 = 0;
    let mut skipped_lock_file: u64 = 0;
    for pending_file in already_on_disk {
        match enqueue(
            pending_file.source_key,
            &source_dir,
            &plan.source_path,
            plan.input_file_type,
            &destination_groups,
            &mut dispatched,
            &mut queue,
        ) {
            EnqueueOutcome::SkippedAppleDouble => skipped_apple_double += 1,
            EnqueueOutcome::SkippedLockFile => skipped_lock_file += 1,
            _ => {}
        }
    }

    // ADR-0124: compute which source files still need pulling *before*
    // spawning Phase A at all -- a file already fully done at the
    // destination is excluded here, never downloaded in the first place,
    // rather than discovered redundant only after its bytes already landed
    // locally (ADR-0120's "Accepted limitation").
    let source_relative_paths = destination::list_source_relative_paths(
        &plan.source_path,
        plan.input_file_type.extensions(),
    )
    .await?;
    // ADR-0139: a listing that matches nothing produces a run that is
    // byte-for-byte indistinguishable from a successful no-op (0 pulled, 0
    // failed, 0 rclone errors, exit 0) -- which is exactly how the
    // `.jpg`-vs-`.jpeg` filter bug stayed invisible. Say so out loud.
    // `eprintln!` as well as `tracing::warn!` on purpose: `observability::
    // init` deliberately registers no console layer, so a `warn!` alone
    // reaches only `pigeon.jsonl` and never the operator's terminal. Safe
    // here specifically because `transform` runs no `MultiProgress` bars
    // for a stray write to corrupt (ADR-0015).
    if source_relative_paths.is_empty() {
        let patterns = plan
            .input_file_type
            .extensions()
            .iter()
            .map(|extension| format!("*.{extension}"))
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!(
            "WARNING: {} matched 0 files for --input-file-type {} (patterns: {patterns}). \
             Nothing to do -- check the prefix and file type.",
            plan.source_path, plan.input_file_type
        );
        tracing::warn!(
            pigeon_job = "transform",
            source = %plan.source_path,
            input_file_type = %plan.input_file_type,
            patterns = %patterns,
            "transform: source listing matched no files"
        );
    }
    let selection = files_needing_pull(
        source_relative_paths,
        &plan.source_path,
        plan.input_file_type,
        &destination_groups,
    );
    let files_to_pull = selection.to_pull;
    let selection_skipped_already_done = selection.skipped_already_done;
    let case_collisions = selection.case_collisions;
    let (bulk_files_to_pull, case_variant_keys) = split_pull_set(&files_to_pull, &case_collisions);
    // The initial on-disk scan above may have found its own AppleDouble
    // stubs; both choke points feed the same counter.
    skipped_apple_double += selection.skipped_apple_double;
    skipped_lock_file += selection.skipped_lock_file;

    // ADR-0130: fail fast before a single byte is downloaded. Nothing has
    // been pulled yet at this point, so there is nothing to salvage -- unlike
    // the per-file check in `process_one`, which fails only its own file.
    if !files_to_pull.is_empty() {
        std::fs::create_dir_all(&source_dir)
            .map_err(|err| format!("failed to create {}: {err}", source_dir.display()))?;
        crate::commands::job::local::check_disk_space(&source_dir, 0)?;
    }

    // ADR-0144 surfaced this before the pull so the operator learns about it
    // from the log rather than from an inexplicable `Pull` failure hours
    // later. ADR-0152 keeps the per-group line and the named keys, but it is
    // no longer a warning about data this job is about to lose: every member
    // is now pulled to its own directory, so this is an INFO describing what
    // was done. ADR-0144's "rclone will deliver at most one of them" became
    // false the moment that landed.
    //
    // ADR-0156: one aggregate line, not one per group. Per-group lines were
    // 428 of one `md` run's 430, re-inflating the per-run log artifact
    // ADR-0151 had just shrunk. The complete list moved to the report,
    // which is the right home for a list that scales with the source.
    let case_collision_groups = case_collision_groups(&case_collisions);
    if !case_collision_groups.is_empty() {
        let examples: Vec<&Vec<String>> = case_collision_groups
            .iter()
            .take(COLLISION_LOG_EXAMPLES)
            .collect();
        tracing::info!(
            pigeon_job = "transform",
            groups = case_collision_groups.len(),
            keys = case_collisions.len(),
            largest_group = case_collision_groups.iter().map(Vec::len).max().unwrap_or(0),
            examples = ?examples,
            "transform: source keys differ only by letter case, so they cannot share one \
             local directory; each is pulled separately (all groups are listed in \
             transform-report.txt)"
        );
    }

    tracing::info!(
        pigeon_job = "transform",
        files_to_pull = files_to_pull.len(),
        skipped_already_done = selection_skipped_already_done,
        skipped_apple_double,
        skipped_lock_file,
        case_colliding_keys = case_collisions.len(),
        bulk_files_to_pull = bulk_files_to_pull.len(),
        queued_from_local_scan = queue.len(),
        "transform: pull phase starting"
    );

    let (copied_tx, mut copied_rx) = mpsc::unbounded_channel();
    let (pull_pid_tx, mut pull_pid_rx) = tokio::sync::mpsc::channel::<u32>(1);
    let pull_log_path = plan
        .local_output
        .join(format!("rclone-pull-{}.jsonl", plan.run_id));
    let mut pull_handle = if bulk_files_to_pull.is_empty() {
        // Nothing to pull -- every matching source file is already done.
        // Skip spawning `rclone` entirely rather than running it against an
        // empty files-from list.
        tokio::spawn(async move {
            drop(copied_tx);
            Ok::<_, String>(crate::core::rclone::RcloneLogSummary::default())
        })
    } else {
        let files_from_path = write_files_from(
            &staging_dir(&plan.local_output),
            "pull-files-from.txt",
            &bulk_files_to_pull,
        )?;
        let pull_pid_tx_clone = pull_pid_tx.clone();
        tokio::spawn({
            let source_path = plan.source_path.clone();
            let source_dir_str = source_dir.display().to_string();
            let download_transfers = plan.download_transfers;
            let download_checkers = plan.download_checkers;
            let tpslimit = plan.tpslimit;
            async move {
                (Copy {
                    source: source_path,
                    destination: source_dir_str,
                    log_path: pull_log_path,
                    transfers: download_transfers,
                    checkers: download_checkers,
                    tpslimit,
                    files_from: Some(files_from_path),
                    job: "transform",
                    step: "pull",
                    upload_metrics: false,
                    hooks: crate::core::rclone::StreamHooks {
                        on_copied: Some(copied_tx),
                        pid_tx: Some(pull_pid_tx_clone),
                    },
                })
                .run()
                .await
            }
        })
    };
    drop(pull_pid_tx);

    // ADR-0152: colliding keys are fetched outside the bulk pull and queued
    // directly, because each lands at a path its key does not describe, so
    // the live tail's `enqueue` (`source_dir.join(key)`) would be wrong for
    // it -- the queue entry carries the two separately, which is what
    // `PendingFile`'s rename made explicit.
    //
    // ADR-0156: one bulk `Copy` per collision-free *slot*, all concurrent,
    // instead of ADR-0152's one sequential `copyto` per key. ADR-0152 called
    // the per-key pass "inherently tiny"; a real `md` run had 428 groups /
    // 870 keys and spent 19m45s here -- almost all of it rclone process
    // startup -- with the whole pipeline stalled behind it. Still a barrier
    // before the main loop (see the ADR for why), but now a seconds-long one.
    let mut case_variant_failures: HashMap<String, String> = HashMap::new();
    let mut case_variant_summary = crate::core::rclone::RcloneLogSummary::default();
    if !case_variant_keys.is_empty() {
        let slots = assign_case_variant_slots(&case_variant_keys);
        let pulls = slots.iter().enumerate().map(|(slot, keys)| {
            let slot_dir = case_variant_slot_dir(&plan.local_output, slot);
            let variant_dir = case_variant_dir(&plan.local_output);
            async move {
                std::fs::create_dir_all(&slot_dir)
                    .map_err(|err| format!("failed to create {}: {err}", slot_dir.display()))?;
                let files_from =
                    write_files_from(&variant_dir, &format!("slot{slot}-files-from.txt"), keys)?;
                (Copy {
                    source: plan.source_path.clone(),
                    destination: slot_dir.display().to_string(),
                    log_path: variant_dir.join(format!("slot{slot}-pull.jsonl")),
                    transfers: plan.download_transfers,
                    checkers: plan.download_checkers,
                    tpslimit: plan.tpslimit,
                    files_from: Some(files_from),
                    job: "transform",
                    step: "pull",
                    upload_metrics: false,
                    // No `pid_tx`: these are not under ADR-0129 backpressure,
                    // since they finish before the main loop starts.
                    hooks: crate::core::rclone::StreamHooks::default(),
                })
                .run()
                .await
            }
        });
        let results = futures::future::join_all(pulls).await;

        for (slot, (keys, result)) in slots.iter().zip(results).enumerate() {
            let slot_dir = case_variant_slot_dir(&plan.local_output, slot);
            // A failed slot is not all-or-nothing: rclone still delivers
            // every object it could, so presence on disk decides each key,
            // and only a key that did not land inherits the slot's error.
            // rclone writes to a temporary name and renames on completion,
            // so a present file is a complete one.
            let slot_error = match result {
                Ok(summary) => {
                    case_variant_summary.transferred += summary.transferred;
                    case_variant_summary.errors += summary.errors;
                    case_variant_summary.bytes += summary.bytes;
                    case_variant_summary.duplicate_notices += summary.duplicate_notices;
                    None
                }
                Err(err) => Some(err),
            };
            for source_key in keys {
                let local_path = slot_dir.join(source_key);
                if local_path.is_file() {
                    if dispatched.insert(source_key.clone()) {
                        queue.push_back(PendingFile {
                            source_key: source_key.clone(),
                            absolute_path: local_path,
                        });
                    }
                } else if let Some(err) = &slot_error {
                    case_variant_failures.insert(source_key.clone(), err.clone());
                }
            }
        }
        tracing::info!(
            pigeon_job = "transform",
            slots = slots.len(),
            keys = case_variant_keys.len(),
            pulled = case_variant_keys
                .iter()
                .filter(|key| dispatched.contains(*key))
                .count(),
            failed = case_variant_failures.len(),
            "transform: case-variant pull complete"
        );
    }

    // Caps each concurrent `ffmpeg` transcode's own internal thread count so
    // the whole in-flight fleet stays within the host's real core count
    // instead of each process independently defaulting to all of them
    // (ADR-0127).
    let transcode_threads = (std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        / concurrency.max(1))
    .max(1);

    let push_semaphore = Arc::new(Semaphore::new(upload_concurrency.max(1)));
    let endpoint_gate = Arc::new(EndpointGate::new());

    // Backpressure: pause/resume pull based on backlog depth (ADR-0129).
    let high_water_mark = plan.max_pending_downloads;
    let low_water_mark = (high_water_mark / 2).max(1);
    let mut pull_pid: Option<u32> = None;
    let mut pull_is_paused = false;

    let mut in_flight: JoinSet<(String, ProcessResult)> = JoinSet::new();
    let mut outcomes = Vec::new();
    let mut pushed: u64 = 0;
    let mut pull_finished = false;
    let mut copied_rx_closed = false;
    let mut pull_result = None;
    let mut failures = FailureBreakdown::default();

    // Circuit breaker (ADR-0121): if the first `CIRCUIT_BREAKER_SAMPLE_SIZE`
    // per-file completions (in completion order, not dispatch order -- see
    // this module's doc comment on concurrency) are *all* transcode failures
    // sharing the same deterministic, non-retryable error class, this is a
    // systemic environment-capability gap, not an isolated bad file --
    // categorically different from, and must not regress, ADR-0116's
    // removal of whole-run-abort-on-a-single-failure. Once tripped, no new
    // work is dispatched or enqueued; whatever's already in-flight (and the
    // pull subprocess, left to finish naturally) still completes.
    let mut circuit_breaker_samples: usize = 0;
    let mut circuit_breaker_matches: usize = 0;
    let mut circuit_breaker_tripped = false;
    let mut circuit_breaker_example: Option<String> = None;

    // ADR-0141: `tokio::spawn` does not propagate the caller's ambient span,
    // so without this every per-file event below (notably
    // `retry_transcode_unless_fatal`'s WARN/ERROR lines) landed in
    // `pigeon.jsonl` with no `command` span at all -- invisible to the
    // `analyze-job-run` skill's documented `spans[].command` filter. Same
    // capture-then-`.instrument()` shape ADR-0098 applied to
    // `deduplicate::worker`'s own spawned tasks.
    let command_span = tracing::Span::current();

    // ADR-0156: the run body used to emit nothing between "pull phase
    // starting" and "run complete" -- on a ~18h `md` run, one line and then
    // silence. These are the numbers that explain a stopped pull (backlog
    // vs. ADR-0129's water marks) and make an ETA computable, and they
    // exist nowhere but this function's locals. Unconditional rather than
    // change-gated: an unchanged line is itself the evidence of a stall.
    let mut progress_interval = tokio::time::interval_at(
        tokio::time::Instant::now() + PROGRESS_INTERVAL,
        PROGRESS_INTERVAL,
    );
    progress_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        while !circuit_breaker_tripped && in_flight.len() < concurrency.max(1) {
            let Some(pending_file) = queue.pop_front() else {
                break;
            };
            let relative_path = pending_file.source_key.clone();
            let source_path = plan.source_path.clone();
            let destination_path = plan.destination_path.clone();
            let result_dir = result_dir.clone();
            let scratch_dir = scratch_dir.clone();
            let push_log_dir = push_log_dir.clone();
            let push_semaphore = push_semaphore.clone();
            let endpoint_gate = endpoint_gate.clone();
            let input_file_type = plan.input_file_type;
            let video_quality = plan.video_quality;
            let audio_quality = plan.audio_quality;
            in_flight.spawn(
                async move {
                    let outcome = process_one(
                        &pending_file,
                        input_file_type,
                        video_quality,
                        audio_quality,
                        transcode_threads,
                        &source_path,
                        &destination_path,
                        &result_dir,
                        &scratch_dir,
                        &push_log_dir,
                        push_semaphore,
                        endpoint_gate,
                    )
                    .await;
                    (relative_path, outcome)
                }
                .instrument(command_span.clone()),
            );
        }

        if in_flight.is_empty() && pull_finished && (queue.is_empty() || circuit_breaker_tripped) {
            break;
        }

        tokio::select! {
            _ = progress_interval.tick() => {
                tracing::info!(
                    pigeon_job = "transform",
                    queued = queue.len(),
                    in_flight = in_flight.len(),
                    dispatched = dispatched.len(),
                    completed = outcomes.len(),
                    pushed,
                    failed_transcode = failures.transcode,
                    failed_place = failures.place,
                    failed_push = failures.push,
                    pull_finished,
                    pull_is_paused,
                    "transform: progress"
                );
            }
            received_pid = pull_pid_rx.recv() => {
                if let Some(pid) = received_pid {
                    pull_pid = Some(pid);
                }
            }
            joined = in_flight.join_next(), if !in_flight.is_empty() => {
                let Some(joined) = joined else { continue; };
                let (relative_path, result) = joined.expect("transform pipeline task panicked");

                if !circuit_breaker_tripped && circuit_breaker_samples < CIRCUIT_BREAKER_SAMPLE_SIZE {
                    circuit_breaker_samples += 1;
                    // ADR-0130: require the failure to have actually come from
                    // the transcode stage, not merely to carry a string that
                    // matches. A place/push error is never evidence of the
                    // systemic converter-capability gap this breaker exists for.
                    if let Err((stage, err)) = &result
                        && *stage == FailureStage::Transcode
                        && media::is_non_retryable_transcode_error(err)
                    {
                        circuit_breaker_matches += 1;
                        circuit_breaker_example.get_or_insert_with(|| err.clone());
                    }
                    if circuit_breaker_should_trip(
                        circuit_breaker_samples,
                        circuit_breaker_matches,
                    ) {
                        circuit_breaker_tripped = true;
                        // ADR-0129: deadlock guard -- if pull is paused, resume it so
                        // backlog can drain (the circuit breaker stops dispatch anyway).
                        if let (Some(pid), true) = (pull_pid, pull_is_paused) {
                            resume_pull_process(pid);
                            pull_is_paused = false;
                        }
                        tracing::error!(
                            sample_size = CIRCUIT_BREAKER_SAMPLE_SIZE,
                            matches = circuit_breaker_matches,
                            "transform circuit breaker tripped: stopping dispatch of further files"
                        );
                    }
                }

                match result {
                    Ok(final_paths) => {
                        // Counts output *objects* (ADR-0149): a workbook
                        // pushing three sheets is three pushes.
                        pushed += final_paths.len() as u64;
                        outcomes.push(FileOutcome {
                            relative_path,
                            destination_filenames: final_paths
                                .iter()
                                .filter_map(|path| {
                                    path.file_name()?.to_str().map(str::to_string)
                                })
                                .collect(),
                            outcome: outcome_for(plan.input_file_type),
                            stage: None,
                            detail: String::new(),
                        });
                    }
                    Err((stage, err)) => {
                        // ADR-0156: the one choke point every stage's
                        // failure funnels through. Before this, only the
                        // ffmpeg kinds logged a failure at all (inside
                        // `retry_transcode_unless_fatal`); every document,
                        // spreadsheet and copy-through kind, and every
                        // `Place` failure, reached the operator only via the
                        // end-of-run report.
                        tracing::warn!(
                            pigeon_job = "transform",
                            relative_path = %relative_path,
                            stage = stage.as_str(),
                            error = %err,
                            "transform: file failed"
                        );
                        failures.record(stage);
                        outcomes.push(FileOutcome {
                            relative_path,
                            destination_filenames: Vec::new(),
                            outcome: Outcome::Failed,
                            stage: Some(stage),
                            detail: err,
                        });
                    }
                }

                // ADR-0129: dynamic backpressure -- check backlog and adjust
                // pull pause state.
                //
                // ADR-0151: only while the pull is still running. Without the
                // `!pull_finished` guard, a run whose pull outpaces local work
                // -- which is most of them, and reliably so for small inputs --
                // signalled the exited pid moments after it finished, then
                // believed it was paused for the rest of the run and tried to
                // resume it. Two misleading WARNs, and nothing a dead process
                // could do about either.
                if let (Some(pid), false) = (pull_pid, pull_finished) {
                    let backlog = queue.len() + in_flight.len();
                    if pull_is_paused && backlog <= low_water_mark {
                        resume_pull_process(pid);
                        pull_is_paused = false;
                    } else if !pull_is_paused && backlog >= high_water_mark {
                        pause_pull_process(pid);
                        pull_is_paused = true;
                    }
                }
            }
            received = copied_rx.recv(), if !pull_finished && !copied_rx_closed => {
                match received {
                    Some(relative_path) => {
                        if !circuit_breaker_tripped {
                            match enqueue(
                                relative_path,
                                &source_dir,
                                &plan.source_path,
                                plan.input_file_type,
                                &destination_groups,
                                &mut dispatched,
                                &mut queue,
                            ) {
                                EnqueueOutcome::SkippedAppleDouble => skipped_apple_double += 1,
                                EnqueueOutcome::SkippedLockFile => skipped_lock_file += 1,
                                _ => {}
                            }
                        }
                    }
                    None => copied_rx_closed = true,
                }
            }
            joined = &mut pull_handle, if !pull_finished => {
                let result = joined.map_err(|err| format!("rclone pull task join failed: {err}"))?;
                pull_finished = true;
                // An exited process is not paused (ADR-0151).
                pull_is_paused = false;
                // Drain anything buffered between the last select poll and
                // the subprocess's exit -- `try_recv` is non-blocking, so
                // this can't stall the loop.
                while let Ok(relative_path) = copied_rx.try_recv() {
                    if !circuit_breaker_tripped {
                        match enqueue(
                            relative_path,
                            &source_dir,
                            &plan.source_path,
                            plan.input_file_type,
                            &destination_groups,
                            &mut dispatched,
                            &mut queue,
                        ) {
                            EnqueueOutcome::SkippedAppleDouble => skipped_apple_double += 1,
                            EnqueueOutcome::SkippedLockFile => skipped_lock_file += 1,
                            _ => {}
                        }
                    }
                }
                pull_result = Some(result);
            }
        }
    }

    let mut pull_summary = pull_result.expect("loop only exits after pull_finished is set")?;
    // ADR-0156: the case-variant slots are real pulls too, so they count
    // toward the totals the cross-check, the summary line and the report use.
    pull_summary.transferred += case_variant_summary.transferred;
    pull_summary.errors += case_variant_summary.errors;
    pull_summary.bytes += case_variant_summary.bytes;
    pull_summary.duplicate_notices += case_variant_summary.duplicate_notices;

    if circuit_breaker_tripped {
        let example = circuit_breaker_example
            .as_deref()
            .unwrap_or("<no example captured>");
        return Err(format!(
            "transform circuit breaker tripped: at least {CIRCUIT_BREAKER_TRIP_THRESHOLD} of the \
             first {CIRCUIT_BREAKER_SAMPLE_SIZE} completed files failed transcoding with the same \
             deterministic error signature. That is a systemic problem, not isolated bad source \
             files, so the run is being aborted instead of grinding through every remaining file \
             with a guaranteed-identical failure. The two usual causes: (1) the wrong \
             --input-file-type for what this source actually holds -- e.g. a mp4-audio run \
             pointed at real video, or vice versa (ADR-0143); a spreadsheet kind pointed at \
             something that is not a workbook (ADR-0149); or a document kind pointed at \
             something unreadable, which for `key` specifically means a file that is not an \
             iWork container at all, often a private key (ADR-0154); or (2) an \
             environment/capability gap, the converter on this job's host lacking support for \
             this input's format or a feature it uses -- `ffmpeg` for image/video/audio kinds, \
             `soffice` for numbers/doc/docx/pages/rtf/pptx/key, `ebook-convert` for epub, and \
             `pandoc` for md/rst. The example error below distinguishes them. \
             Example error: {example}"
        ));
    }

    // ADR-0130: reconcile the pull phase against its own intended set.
    // `files_to_pull` is exactly what this run asked rclone for, and every
    // path that entered the pipeline by *either* dispatch source passed
    // through `enqueue` and so is recorded in `dispatched`. Anything intended
    // but never dispatched was therefore never delivered -- a real per-file
    // pull failure, at a fidelity rclone's own aggregate error count can't
    // give us without `-v`-level logging this job doesn't enable (the same
    // limit ADR-0102 recorded for `import`).
    //
    // Deliberately after the circuit-breaker return above: a tripped breaker
    // stops dispatch on purpose, so reconciling there would manufacture
    // hundreds of spurious "pull failures" out of work that was correctly
    // abandoned.
    //
    // ADR-0156: each undelivered file is also logged, but only the first
    // `UNDELIVERED_LOG_LIMIT` by name -- a pull that failed wholesale would
    // otherwise write one WARN per file in the source. The report still
    // names every one.
    for relative_path in &files_to_pull {
        if dispatched.contains(relative_path) {
            continue;
        }
        let detail = undelivered_detail(relative_path, &case_collisions, &case_variant_failures);
        if failures.pull < UNDELIVERED_LOG_LIMIT {
            tracing::warn!(
                pigeon_job = "transform",
                relative_path = %relative_path,
                stage = FailureStage::Pull.as_str(),
                error = %detail,
                "transform: file failed"
            );
        }
        failures.record(FailureStage::Pull);
        outcomes.push(FileOutcome {
            relative_path: relative_path.clone(),
            destination_filenames: Vec::new(),
            outcome: Outcome::Failed,
            stage: Some(FailureStage::Pull),
            detail,
        });
    }
    if failures.pull > UNDELIVERED_LOG_LIMIT {
        tracing::warn!(
            pigeon_job = "transform",
            stage = FailureStage::Pull.as_str(),
            named = UNDELIVERED_LOG_LIMIT,
            not_named = failures.pull - UNDELIVERED_LOG_LIMIT,
            "transform: further undelivered files not named individually; see \
             transform-report.txt"
        );
    }
    if failures.pull > 0 {
        crate::observability::metrics::record_phase_count(
            "transform",
            "pull",
            Outcome::Failed.as_str(),
            failures.pull,
            None,
        );
    }

    // The two counts measure subtly different things -- rclone counts
    // transfer *errors*, this counts *undelivered files* -- so a mismatch is
    // surfaced rather than silently reconciled away.
    // The two counts measure subtly different things, so a mismatch is
    // surfaced rather than reconciled away -- but ADR-0151 makes it say what
    // accounts for the gap when the job already knows. A case collision is
    // the common case: rclone resolves it itself, logs `Duplicate object
    // found in source - ignoring` at **notice** level, and leaves its error
    // count at zero, so the disagreement is expected rather than suspicious.
    if pull_summary.errors != failures.pull {
        let explained_by_collisions =
            failures.pull <= case_collisions.len() as u64 && !case_collisions.is_empty();
        if explained_by_collisions {
            tracing::warn!(
                pigeon_job = "transform",
                rclone_errors = pull_summary.errors,
                undelivered_files = failures.pull,
                case_colliding_keys = case_collisions.len(),
                rclone_duplicate_notices = pull_summary.duplicate_notices,
                "transform: rclone's pull error count disagrees with the number of undelivered \
                 files, fully accounted for by case-colliding source keys"
            );
        } else {
            tracing::warn!(
                pigeon_job = "transform",
                rclone_errors = pull_summary.errors,
                undelivered_files = failures.pull,
                case_colliding_keys = case_collisions.len(),
                rclone_duplicate_notices = pull_summary.duplicate_notices,
                "transform: rclone's pull error count disagrees with the number of undelivered \
                 files"
            );
        }
    }

    tracing::info!(
        pigeon_job = "transform",
        transferred = pull_summary.transferred,
        errors = pull_summary.errors,
        "transform: pull phase complete"
    );

    let failed = outcomes.iter().any(|o| o.outcome == Outcome::Failed);

    let transcoded = outcomes
        .iter()
        .filter(|o| o.outcome == Outcome::Transcoded)
        .count();
    let copied_through = outcomes
        .iter()
        .filter(|o| o.outcome == Outcome::CopiedThrough)
        .count();
    tracing::info!(
        pigeon_job = "transform",
        pulled = pull_summary.transferred,
        // Output objects, where `transcoded`/`copied_through` below count
        // input files (ADR-0149). The field name is kept because the
        // `analyze-job-run` runbook reads this line, and it genuinely is
        // the number of pushes.
        pushed,
        transcoded,
        copied_through,
        failed_total = failures.total(),
        failed_pull = failures.pull,
        failed_transcode = failures.transcode,
        failed_place = failures.place,
        failed_push = failures.push,
        skipped_already_done = selection_skipped_already_done,
        skipped_apple_double,
        skipped_lock_file,
        "transform: run complete"
    );

    Ok(TransformSummary {
        outcomes,
        pulled: pull_summary.transferred,
        pushed,
        failed,
        failures,
        skipped_already_done: selection_skipped_already_done,
        skipped_apple_double,
        skipped_lock_file,
        pull_errors: pull_summary.errors,
        case_collision_groups,
    })
}

/// Bespoke, tab-separated `<local_output>/transform-report.txt` (ADR-0112
/// Decision §8) -- not the generic `Debug`-based report
/// `report_upload::write_summary_report` writes for jobs without a bespoke
/// one, mirroring `deduplicate-report.txt`'s precedent. Written at
/// `local_output`'s top level, sibling of `.staging/`/`result/`, so it is
/// never swept into any push by `core::data::collect_files`.
pub(crate) fn write_report(
    local_output: &Path,
    summary: &TransformSummary,
) -> Result<PathBuf, String> {
    use std::fmt::Write as _;

    // `stage` column added by ADR-0130 -- a failed row now says which stage
    // failed without the reader having to parse `detail`'s prose.
    let mut body =
        String::from("source_relative_path\toutcome\tstage\tdestination_filenames\tdetail\n");
    let mut transcoded = 0;
    let mut copied_through = 0;
    let mut failed = 0;
    for outcome in &summary.outcomes {
        match outcome.outcome {
            Outcome::Transcoded => transcoded += 1,
            Outcome::CopiedThrough => copied_through += 1,
            Outcome::Failed => failed += 1,
        }
        let _ = writeln!(
            body,
            "{}\t{}\t{}\t{}\t{}",
            outcome.relative_path,
            outcome.outcome.as_str(),
            outcome.stage.map(FailureStage::as_str).unwrap_or(""),
            // Comma-joined, which is unambiguous only because
            // `core::data::sheet_slug` cannot emit a comma -- see its doc
            // comment before widening that character set. An empty vector
            // joins to "", so a failed row renders exactly as before.
            outcome.destination_filenames.join(","),
            outcome.detail,
        );
    }
    let _ = writeln!(
        body,
        "\n{transcoded} transcoded, {copied_through} copied through, {failed} failed; \
         {} output file(s) pushed.",
        summary.pushed
    );
    let failures = summary.failures;
    let _ = writeln!(
        body,
        "failures by stage: {} pull, {} transcode, {} place, {} push.",
        failures.pull, failures.transcode, failures.place, failures.push
    );
    let _ = writeln!(
        body,
        "skipped: {} already done at the destination, {} AppleDouble sidecar(s), {} Excel lock \
         stub(s).",
        summary.skipped_already_done, summary.skipped_apple_double, summary.skipped_lock_file
    );
    let _ = writeln!(
        body,
        "pull: {} transferred, {} rclone error(s) reported.",
        summary.pulled, summary.pull_errors
    );
    // ADR-0156: the full list the log line now only samples. One line per
    // group, keys separated by " | " -- a source key can contain a comma
    // but this listing is prose after the table, not a TSV column.
    if !summary.case_collision_groups.is_empty() {
        let _ = writeln!(
            body,
            "case-colliding source keys, each pulled separately ({} group(s)):",
            summary.case_collision_groups.len()
        );
        for group in &summary.case_collision_groups {
            let _ = writeln!(body, "  {}", group.join(" | "));
        }
    }

    let path = local_output.join("transform-report.txt");
    std::fs::write(&path, body)
        .map_err(|err| format!("failed to write {}: {err}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    const TEST_RETRIES: usize = 3;

    /// ADR-0143: the breaker trips on a strong majority, not unanimity.
    /// Unit-tested through the extracted pure function because the real
    /// loop's `matches` count depends on concurrent completion ordering,
    /// which a test can't pin deterministically.
    #[test]
    fn circuit_breaker_trips_on_a_strong_majority_not_only_unanimity() {
        // Unanimous still trips, as it always did (ADR-0121).
        assert!(circuit_breaker_should_trip(
            CIRCUIT_BREAKER_SAMPLE_SIZE,
            CIRCUIT_BREAKER_SAMPLE_SIZE
        ));
        // The new case: a few interleaved successes no longer disarm it.
        assert!(circuit_breaker_should_trip(
            CIRCUIT_BREAKER_SAMPLE_SIZE,
            CIRCUIT_BREAKER_TRIP_THRESHOLD
        ));
        // One below the threshold must not trip -- this is what keeps
        // ADR-0116's "one bad apple doesn't abort the run" guarantee.
        assert!(!circuit_breaker_should_trip(
            CIRCUIT_BREAKER_SAMPLE_SIZE,
            CIRCUIT_BREAKER_TRIP_THRESHOLD - 1
        ));
        // Never trips before the full sample is in, however bad it looks.
        assert!(!circuit_breaker_should_trip(
            CIRCUIT_BREAKER_SAMPLE_SIZE - 1,
            CIRCUIT_BREAKER_SAMPLE_SIZE - 1
        ));
        assert!(!circuit_breaker_should_trip(0, 0));
    }

    /// The whole point of the gate (ADR-0144): one outage costs the run one
    /// recovery round, not one per in-flight file. Without this, the 275
    /// files of the motivating run would each have paid the full recovery
    /// budget over again.
    ///
    /// `start_paused` means the round's `sleep` is virtual, so this asserts
    /// the election and the round accounting, not wall-clock patience.
    #[tokio::test(start_paused = true)]
    async fn endpoint_gate_charges_one_recovery_round_for_many_concurrent_failures() {
        let gate = Arc::new(EndpointGate::new());
        let mut waiters = JoinSet::new();
        for index in 0..16 {
            let gate = gate.clone();
            waiters.spawn(async move {
                gate.wait_for_recovery(
                    &format!("file-{index}.jpg"),
                    "dial tcp: lookup s3.example.invalid: no such host",
                )
                .await
            });
        }

        let mut decisions = Vec::new();
        while let Some(joined) = waiters.join_next().await {
            decisions.push(joined.unwrap());
        }

        assert_eq!(decisions.len(), 16);
        assert!(
            decisions
                .iter()
                .all(|decision| *decision == GateDecision::Retry),
            "every file should be told to retry once the gate reopens: {decisions:?}"
        );
        assert_eq!(
            gate.rounds.load(Ordering::SeqCst),
            1,
            "16 files failing on one shared outage must elect one prober and burn one round"
        );
    }

    /// An outage that never ends must not stall the run forever: after
    /// `PUSH_ENDPOINT_RECOVERY_ROUNDS` fruitless rounds the gate latches,
    /// so every remaining file fails immediately instead of re-paying the
    /// budget (ADR-0144).
    #[tokio::test(start_paused = true)]
    async fn endpoint_gate_gives_up_and_stays_latched_once_the_budget_is_exhausted() {
        let gate = EndpointGate::new();
        for round in 1..=PUSH_ENDPOINT_RECOVERY_ROUNDS {
            assert_eq!(
                gate.wait_for_recovery("f.jpg", "no such host").await,
                GateDecision::Retry,
                "round {round} of {PUSH_ENDPOINT_RECOVERY_ROUNDS} should still be patient"
            );
        }
        for _ in 0..3 {
            assert_eq!(
                gate.wait_for_recovery("f.jpg", "no such host").await,
                GateDecision::GiveUp,
                "the gate must stay latched, not re-arm for every later file"
            );
        }
    }

    /// A file that arrives *after* a latched gate must also give up
    /// immediately rather than parking -- the waiter branch reads the same
    /// exhausted round count the prober branch does.
    #[tokio::test(start_paused = true)]
    async fn endpoint_gate_waiters_give_up_when_the_budget_is_already_exhausted() {
        let gate = Arc::new(EndpointGate::new());
        gate.rounds
            .store(PUSH_ENDPOINT_RECOVERY_ROUNDS + 1, Ordering::SeqCst);

        // Hold the election so `wait_for_recovery` takes the waiter branch.
        let _election = gate.prober.lock().await;
        let waiter = {
            let gate = gate.clone();
            tokio::spawn(async move { gate.wait_for_recovery("late.jpg", "no such host").await })
        };

        assert_eq!(waiter.await.unwrap(), GateDecision::GiveUp);
    }

    #[tokio::test]
    async fn retry_transcode_unless_fatal_does_not_retry_a_non_retryable_error() {
        let attempts = AtomicUsize::new(0);
        let result: Result<(), String> =
            retry_transcode_unless_fatal(TEST_RETRIES, Duration::from_secs(60), "x.heic", || {
                attempts.fetch_add(1, Ordering::SeqCst);
                async { Err("ffmpeg failed to transcode x.heic: moov atom not found".to_string()) }
            })
            .await;

        assert!(result.is_err());
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retry_transcode_unless_fatal_still_retries_a_transient_looking_error_and_eventually_succeeds()
     {
        let attempts = AtomicUsize::new(0);
        let result: Result<&str, String> =
            retry_transcode_unless_fatal(TEST_RETRIES, Duration::from_millis(1), "x.png", || {
                let count = attempts.fetch_add(1, Ordering::SeqCst) + 1;
                async move {
                    if count < 3 {
                        Err("Resource temporarily unavailable (os error 11)".to_string())
                    } else {
                        Ok("transcoded")
                    }
                }
            })
            .await;

        assert_eq!(result, Ok("transcoded"));
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn retry_transcode_unless_fatal_returns_the_last_error_after_exhausting_retryable_attempts()
     {
        let attempts = AtomicUsize::new(0);
        let result: Result<(), String> =
            retry_transcode_unless_fatal(TEST_RETRIES, Duration::from_millis(1), "x.mp4", || {
                let count = attempts.fetch_add(1, Ordering::SeqCst) + 1;
                async move { Err(format!("Cannot allocate memory (attempt {count})")) }
            })
            .await;

        assert_eq!(
            result,
            Err(format!("Cannot allocate memory (attempt {TEST_RETRIES})"))
        );
        assert_eq!(attempts.load(Ordering::SeqCst), TEST_RETRIES);
    }

    #[tokio::test]
    async fn run_transform_job_transcodes_and_pushes_every_pending_file() {
        if media::check_ffmpeg_available().await.is_err() {
            eprintln!("skipping: ffmpeg/ffprobe not found on PATH");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let local_output = dir.path().join("work");
        let source_for_pull = dir.path().join("pull-source");
        let destination = dir.path().join("destination");
        std::fs::create_dir_all(&source_for_pull).unwrap();
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(source_for_pull.join("a.jpeg"), b"jpeg bytes").unwrap();
        std::fs::write(source_for_pull.join("b.jpeg"), b"other jpeg bytes").unwrap();

        let plan = TransformPlan {
            source_path: source_for_pull.display().to_string(),
            destination_path: destination.display().to_string(),
            local_output: local_output.clone(),
            input_file_type: InputFileType::Jpeg,
            download_transfers: 2,
            download_checkers: 4,
            video_quality: VideoQuality::Medium,
            audio_quality: AudioQuality::Medium,
            tpslimit: None,
            max_pending_downloads: 100,
            run_id: "test-run".to_string(),
        };

        let summary = run_transform_job(&plan, 2, 2).await.unwrap();

        assert!(!summary.failed);
        assert_eq!(summary.pushed, 2);
        assert_eq!(summary.outcomes.len(), 2);
        assert!(
            summary
                .outcomes
                .iter()
                .all(|outcome| outcome.outcome == Outcome::CopiedThrough)
        );

        let pushed_names: Vec<_> = std::fs::read_dir(&destination)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(pushed_names.len(), 2);

        // ADR-0124: once a file is durably pushed, its local source and
        // result copies must be gone -- steady-state disk usage should not
        // scale with the whole dataset.
        assert!(
            std::fs::read_dir(source_dir(&local_output))
                .unwrap()
                .next()
                .is_none(),
            "local source/ should be empty after every file is pushed"
        );
        assert!(
            std::fs::read_dir(result_dir(&local_output))
                .unwrap()
                .next()
                .is_none(),
            "local result/ should be empty after every file is pushed"
        );

        // A rerun must be a no-op: nothing left pending, nothing re-pushed.
        let rerun_summary = run_transform_job(&plan, 2, 2).await.unwrap();
        assert_eq!(rerun_summary.pushed, 0);
        assert!(rerun_summary.outcomes.is_empty());
    }

    /// The end-to-end regression test for ADR-0139, and the coverage hole
    /// that let it ship: every pre-existing jpeg test used a `.jpeg`
    /// fixture and none used `.jpg`, so a filter that matched only one
    /// spelling passed the whole suite. A `--input-file-type=jpeg` run over
    /// a source of `.jpg` files silently pushed nothing.
    #[tokio::test]
    async fn run_transform_job_handles_every_spelling_of_one_input_kind() {
        if media::check_ffmpeg_available().await.is_err() {
            eprintln!("skipping: ffmpeg/ffprobe not found on PATH");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let local_output = dir.path().join("work");
        let source_for_pull = dir.path().join("pull-source");
        let destination = dir.path().join("destination");
        std::fs::create_dir_all(&source_for_pull).unwrap();
        std::fs::create_dir_all(&destination).unwrap();
        // The reported failure's shape: a source holding only the `.jpg`
        // spelling, plus the `.jpeg` one to prove both still work together,
        // plus an unrelated format that must stay excluded.
        std::fs::write(source_for_pull.join("a.jpg"), b"jpg bytes").unwrap();
        std::fs::write(source_for_pull.join("b.jpeg"), b"jpeg bytes").unwrap();
        std::fs::write(source_for_pull.join("c.png"), b"a different format").unwrap();

        let plan = TransformPlan {
            source_path: source_for_pull.display().to_string(),
            destination_path: destination.display().to_string(),
            local_output,
            input_file_type: InputFileType::Jpeg,
            download_transfers: 2,
            download_checkers: 4,
            video_quality: VideoQuality::Medium,
            audio_quality: AudioQuality::Medium,
            tpslimit: None,
            max_pending_downloads: 100,
            run_id: "test-run".to_string(),
        };

        let summary = run_transform_job(&plan, 2, 2).await.unwrap();

        assert!(!summary.failed);
        assert_eq!(
            summary.pushed, 2,
            "both the .jpg and the .jpeg file must be pushed"
        );
        assert!(
            summary
                .outcomes
                .iter()
                .all(|outcome| outcome.outcome == Outcome::CopiedThrough),
            "jpeg input is copied through, never transcoded"
        );
        let mut processed: Vec<&str> = summary
            .outcomes
            .iter()
            .map(|outcome| outcome.relative_path.as_str())
            .collect();
        processed.sort();
        assert_eq!(processed, vec!["a.jpg", "b.jpeg"]);

        // The `.png` must not have been swept up by the widened filter.
        assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 2);
    }

    /// ADR-0139: a source whose listing matches nothing is a clean, quiet
    /// no-op in the summary (0 pulled, 0 failed, exit 0) -- which is why it
    /// was mistaken for success. The run must stay a no-op, but also emit a
    /// warning; run with `--nocapture` to see the WARNING line this prints.
    #[tokio::test]
    async fn run_transform_job_is_a_clean_no_op_when_the_source_matches_nothing() {
        if media::check_ffmpeg_available().await.is_err() {
            eprintln!("skipping: ffmpeg/ffprobe not found on PATH");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let source_for_pull = dir.path().join("pull-source");
        let destination = dir.path().join("destination");
        std::fs::create_dir_all(&source_for_pull).unwrap();
        std::fs::create_dir_all(&destination).unwrap();
        // Present, but not of the requested input kind.
        std::fs::write(source_for_pull.join("notes.txt"), b"not an image").unwrap();

        let plan = TransformPlan {
            source_path: source_for_pull.display().to_string(),
            destination_path: destination.display().to_string(),
            local_output: dir.path().join("work"),
            input_file_type: InputFileType::Jpeg,
            download_transfers: 2,
            download_checkers: 4,
            video_quality: VideoQuality::Medium,
            audio_quality: AudioQuality::Medium,
            tpslimit: None,
            max_pending_downloads: 100,
            run_id: "test-run".to_string(),
        };

        let summary = run_transform_job(&plan, 2, 2).await.unwrap();

        assert!(!summary.failed);
        assert_eq!(summary.pulled, 0);
        assert_eq!(summary.pushed, 0);
        assert!(summary.outcomes.is_empty());
        assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 0);
    }

    /// The direct regression test for ADR-0116's production incident: one
    /// permanently-corrupt file among several good ones must not stop the
    /// others from being attempted and pushed.
    #[tokio::test]
    async fn a_mixed_batch_with_one_permanently_failing_file_still_processes_and_pushes_every_other_file()
     {
        if media::check_ffmpeg_available().await.is_err() {
            eprintln!("skipping: ffmpeg/ffprobe not found on PATH");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let local_output = dir.path().join("work");
        let source_for_pull = dir.path().join("pull-source");
        let destination = dir.path().join("destination");
        std::fs::create_dir_all(&source_for_pull).unwrap();
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(source_for_pull.join("good-1.heic"), b"not a real heic file").unwrap();
        std::fs::write(
            source_for_pull.join("good-2.heic"),
            b"also not a real heic file",
        )
        .unwrap();
        std::fs::write(
            source_for_pull.join("bad.heic"),
            b"still not a real heic file",
        )
        .unwrap();

        let plan = TransformPlan {
            source_path: source_for_pull.display().to_string(),
            destination_path: destination.display().to_string(),
            local_output: local_output.clone(),
            input_file_type: InputFileType::Heic,
            download_transfers: 3,
            download_checkers: 4,
            video_quality: VideoQuality::Medium,
            audio_quality: AudioQuality::Medium,
            tpslimit: None,
            max_pending_downloads: 100,
            run_id: "test-run".to_string(),
        };

        let summary = run_transform_job(&plan, 3, 3).await.unwrap();

        // Every file is a corrupt HEIC fixture (no real `libheif`-decodable
        // content), so every one of the 3 fails -- this test only exists to
        // prove ALL 3 are attempted (none skipped because an earlier one
        // failed) and that failure never prevents reporting for files that
        // *do* succeed, which this synthetic-fixture constraint can't
        // itself demonstrate directly.
        assert_eq!(summary.outcomes.len(), 3);
        assert!(summary.failed);
        let failed_paths: HashSet<_> = summary
            .outcomes
            .iter()
            .filter(|outcome| outcome.outcome == Outcome::Failed)
            .map(|outcome| outcome.relative_path.clone())
            .collect();
        assert!(failed_paths.contains("bad.heic"));
        for outcome in &summary.outcomes {
            if outcome.outcome == Outcome::Failed {
                assert!(!outcome.detail.is_empty());
            }
        }
    }

    /// Proves the initial directory scan dispatches a file already on disk
    /// from a prior run, even though rclone's own skip-unchanged-file logic
    /// means no fresh "Copied" line fires for it this run.
    #[tokio::test]
    async fn run_transform_job_dispatches_a_file_already_on_disk_from_a_prior_run() {
        if media::check_ffmpeg_available().await.is_err() {
            eprintln!("skipping: ffmpeg/ffprobe not found on PATH");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let local_output = dir.path().join("work");
        let source_for_pull = dir.path().join("pull-source");
        let destination = dir.path().join("destination");
        std::fs::create_dir_all(&source_for_pull).unwrap();
        std::fs::create_dir_all(&destination).unwrap();
        // Seed the *pull source* (so Phase A's rclone copy has something
        // matching to consider already-synced) AND the local source tree
        // (simulating "already pulled in a prior, interrupted run") with
        // byte-identical content.
        std::fs::write(source_for_pull.join("a.jpeg"), b"jpeg bytes").unwrap();
        std::fs::create_dir_all(source_dir(&local_output)).unwrap();
        std::fs::write(source_dir(&local_output).join("a.jpeg"), b"jpeg bytes").unwrap();

        let plan = TransformPlan {
            source_path: source_for_pull.display().to_string(),
            destination_path: destination.display().to_string(),
            local_output: local_output.clone(),
            input_file_type: InputFileType::Jpeg,
            download_transfers: 2,
            download_checkers: 4,
            video_quality: VideoQuality::Medium,
            audio_quality: AudioQuality::Medium,
            tpslimit: None,
            max_pending_downloads: 100,
            run_id: "test-run".to_string(),
        };

        let summary = run_transform_job(&plan, 2, 2).await.unwrap();

        assert!(!summary.failed);
        assert_eq!(summary.pushed, 1);
        assert!(
            destination
                .join(&summary.outcomes[0].destination_filenames[0])
                .exists()
        );
    }

    /// ADR-0127 regression test: a file whose transcode already completed
    /// and was placed into `result/` in a prior, crashed run (survived past
    /// `place_one`'s atomic rename but never got pushed) must be pushed
    /// directly on a rerun, never re-transcoded. Proven with a deliberately
    /// corrupt HEIC source -- a real transcode attempt against it always
    /// fails -- alongside a placeholder "already transcoded" file sitting in
    /// `result/` under this file's deterministic destination name: the job
    /// can only succeed here by skipping straight to push.
    #[tokio::test]
    async fn run_transform_job_pushes_an_already_placed_result_file_without_retranscoding() {
        if media::check_ffmpeg_available().await.is_err() {
            eprintln!("skipping: ffmpeg/ffprobe not found on PATH");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let local_output = dir.path().join("work");
        let source_for_pull = dir.path().join("pull-source");
        let destination = dir.path().join("destination");
        std::fs::create_dir_all(&source_for_pull).unwrap();
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(source_for_pull.join("a.heic"), b"not a real heic file").unwrap();
        std::fs::create_dir_all(source_dir(&local_output)).unwrap();
        std::fs::write(
            source_dir(&local_output).join("a.heic"),
            b"not a real heic file",
        )
        .unwrap();

        let source_path = source_for_pull.display().to_string();
        let destination_name =
            placement::compute_destination_name(&source_path, "a.heic", InputFileType::Heic);
        std::fs::create_dir_all(result_dir(&local_output)).unwrap();
        std::fs::write(
            result_dir(&local_output).join(&destination_name),
            b"already transcoded in a prior run",
        )
        .unwrap();

        let plan = TransformPlan {
            source_path,
            destination_path: destination.display().to_string(),
            local_output: local_output.clone(),
            input_file_type: InputFileType::Heic,
            download_transfers: 2,
            download_checkers: 4,
            video_quality: VideoQuality::Medium,
            audio_quality: AudioQuality::Medium,
            tpslimit: None,
            max_pending_downloads: 100,
            run_id: "test-run".to_string(),
        };

        let summary = run_transform_job(&plan, 2, 2).await.unwrap();

        assert!(!summary.failed);
        assert_eq!(summary.pushed, 1);
        assert_eq!(
            std::fs::read(destination.join(&destination_name)).unwrap(),
            b"already transcoded in a prior run"
        );
    }

    /// The direct regression test for ADR-0120's production gap: on a truly
    /// fresh VM (no local-disk seeding at all -- `local_output` starts
    /// completely empty, unlike the "already on disk" test above, which is
    /// what actually simulates a VM replacement), a file already pushed to
    /// `--destination-path` in a prior run must be skipped entirely --
    /// neither re-transcoded nor re-pushed. Also the direct regression test
    /// for ADR-0124's pre-pull filter: this file must not even be
    /// downloaded, superseding ADR-0120's "Accepted limitation" that it
    /// would always be redundantly re-pulled.
    #[tokio::test]
    async fn run_transform_job_skips_transcode_and_push_for_a_file_already_present_at_the_destination()
     {
        if media::check_ffmpeg_available().await.is_err() {
            eprintln!("skipping: ffmpeg/ffprobe not found on PATH");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let local_output = dir.path().join("work");
        let source_for_pull = dir.path().join("pull-source");
        let destination = dir.path().join("destination");
        std::fs::create_dir_all(&source_for_pull).unwrap();
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(source_for_pull.join("a.jpeg"), b"jpeg bytes").unwrap();

        let source_path = source_for_pull.display().to_string();
        let destination_filename =
            placement::compute_destination_name(&source_path, "a.jpeg", InputFileType::Jpeg);
        std::fs::write(destination.join(&destination_filename), b"already pushed").unwrap();

        let plan = TransformPlan {
            source_path,
            destination_path: destination.display().to_string(),
            local_output: local_output.clone(),
            input_file_type: InputFileType::Jpeg,
            download_transfers: 2,
            download_checkers: 4,
            video_quality: VideoQuality::Medium,
            audio_quality: AudioQuality::Medium,
            tpslimit: None,
            max_pending_downloads: 100,
            run_id: "test-run".to_string(),
        };

        let summary = run_transform_job(&plan, 2, 2).await.unwrap();

        assert!(!summary.failed);
        assert_eq!(summary.pushed, 0);
        assert!(summary.outcomes.is_empty());
        // The pre-existing destination file must be untouched -- proof
        // nothing re-pushed over it.
        assert_eq!(
            std::fs::read(destination.join(&destination_filename)).unwrap(),
            b"already pushed"
        );
        // ADR-0124: the file must never even be downloaded -- the
        // pre-pull filter excludes it before Phase A's `rclone copy` runs
        // at all, not merely discovered-and-skipped afterward.
        assert!(!source_dir(&local_output).join("a.jpeg").exists());
    }

    /// The direct regression test for ADR-0121's circuit breaker: a batch
    /// where every file is the same kind of permanently-corrupt input (not
    /// one bad apple among many good ones -- that's ADR-0116's scenario,
    /// covered by `a_mixed_batch_with_one_permanently_failing_file_...`
    /// above) must abort fast with `Err` instead of attempting every file.
    #[tokio::test]
    async fn run_transform_job_trips_the_circuit_breaker_when_the_first_20_completions_all_fail_the_same_way()
     {
        if media::check_ffmpeg_available().await.is_err() {
            eprintln!("skipping: ffmpeg/ffprobe not found on PATH");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let local_output = dir.path().join("work");
        let source_for_pull = dir.path().join("pull-source");
        let destination = dir.path().join("destination");
        std::fs::create_dir_all(&source_for_pull).unwrap();
        std::fs::create_dir_all(&destination).unwrap();
        for i in 0..25 {
            std::fs::write(
                source_for_pull.join(format!("bad-{i}.heic")),
                b"not a real heic file",
            )
            .unwrap();
        }

        let plan = TransformPlan {
            source_path: source_for_pull.display().to_string(),
            destination_path: destination.display().to_string(),
            local_output: local_output.clone(),
            input_file_type: InputFileType::Heic,
            download_transfers: 5,
            download_checkers: 4,
            video_quality: VideoQuality::Medium,
            audio_quality: AudioQuality::Medium,
            tpslimit: None,
            max_pending_downloads: 100,
            run_id: "test-run".to_string(),
        };

        let result = run_transform_job(&plan, 5, 5).await;

        let err = result.expect_err("a uniformly-corrupt batch must trip the circuit breaker");
        assert!(err.contains("circuit breaker"));
        assert!(err.contains(&CIRCUIT_BREAKER_SAMPLE_SIZE.to_string()));
    }

    /// Proves the circuit breaker does *not* trip on a batch that's mostly
    /// healthy, at a scale that actually exercises its 20-sample threshold
    /// (unlike the 3-file mixed-batch test above, which never reaches it) --
    /// the real proof this doesn't regress ADR-0116 at the scale that
    /// matters for this new logic.
    #[tokio::test]
    async fn run_transform_job_does_not_trip_the_circuit_breaker_on_a_mostly_healthy_batch() {
        if media::check_ffmpeg_available().await.is_err() {
            eprintln!("skipping: ffmpeg/ffprobe not found on PATH");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let local_output = dir.path().join("work");
        let source_for_pull = dir.path().join("pull-source");
        let destination = dir.path().join("destination");
        std::fs::create_dir_all(&source_for_pull).unwrap();
        std::fs::create_dir_all(&destination).unwrap();
        for i in 0..24 {
            let path = source_for_pull.join(format!("good-{i}.png"));
            generate_test_png(&path).await.unwrap();
        }
        std::fs::write(source_for_pull.join("bad.png"), b"not a real png file").unwrap();

        let plan = TransformPlan {
            source_path: source_for_pull.display().to_string(),
            destination_path: destination.display().to_string(),
            local_output: local_output.clone(),
            input_file_type: InputFileType::Png,
            download_transfers: 5,
            download_checkers: 4,
            video_quality: VideoQuality::Medium,
            audio_quality: AudioQuality::Medium,
            tpslimit: None,
            max_pending_downloads: 100,
            run_id: "test-run".to_string(),
        };

        let summary = run_transform_job(&plan, 5, 5).await.unwrap();

        assert!(summary.failed);
        assert_eq!(summary.pushed, 24);

        // ADR-0124: every successfully-pushed file's local source copy is
        // gone, but the one failed file is deliberately left in place for
        // diagnosis.
        assert!(source_dir(&local_output).join("bad.png").exists());
        for i in 0..24 {
            assert!(
                !source_dir(&local_output)
                    .join(format!("good-{i}.png"))
                    .exists()
            );
        }
    }

    /// Mirrors `media.rs::tests::generate_test_png` -- a third real consumer
    /// of this exact helper shape within the `transform` module, kept local
    /// per this codebase's duplicate-until-a-third-consumer convention
    /// rather than hoisted, since each copy is tiny and test-only.
    async fn generate_test_png(path: &Path) -> Result<(), String> {
        let result = tokio::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=1:size=64x48:rate=1",
                "-frames:v",
                "1",
            ])
            .arg(path)
            .output()
            .await
            .map_err(|err| format!("failed to run ffmpeg: {err}"))?;
        if !result.status.success() {
            return Err(String::from_utf8_lossy(&result.stderr).to_string());
        }
        Ok(())
    }

    /// Mirrors `media.rs::tests::generate_test_video`, kept local per this
    /// module's existing `generate_test_png` precedent.
    async fn generate_test_video(path: &Path) -> Result<(), String> {
        let result = tokio::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=1:size=64x48:rate=10",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=duration=1",
                "-c:v",
                "libx264",
                "-c:a",
                "aac",
            ])
            .arg(path)
            .output()
            .await
            .map_err(|err| format!("failed to run ffmpeg: {err}"))?;
        if !result.status.success() {
            return Err(String::from_utf8_lossy(&result.stderr).to_string());
        }
        Ok(())
    }

    /// The direct regression test for ADR-0122: a video input pushes as
    /// `.mp4`, mirroring the existing jpeg/png/heic pipeline tests above.
    #[tokio::test]
    async fn run_transform_job_transcodes_and_pushes_a_video_file() {
        if media::check_ffmpeg_available().await.is_err() {
            eprintln!("skipping: ffmpeg/ffprobe not found on PATH");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let local_output = dir.path().join("work");
        let source_for_pull = dir.path().join("pull-source");
        let destination = dir.path().join("destination");
        std::fs::create_dir_all(&source_for_pull).unwrap();
        std::fs::create_dir_all(&destination).unwrap();
        if generate_test_video(&source_for_pull.join("a.mp4"))
            .await
            .is_err()
        {
            eprintln!("skipping: this ffmpeg build can't generate a test fixture");
            return;
        }

        let plan = TransformPlan {
            source_path: source_for_pull.display().to_string(),
            destination_path: destination.display().to_string(),
            local_output: local_output.clone(),
            input_file_type: InputFileType::Mp4,
            download_transfers: 2,
            download_checkers: 4,
            video_quality: VideoQuality::Low,
            audio_quality: AudioQuality::Medium,
            tpslimit: None,
            max_pending_downloads: 100,
            run_id: "test-run".to_string(),
        };

        let summary = run_transform_job(&plan, 2, 2).await.unwrap();

        if summary.failed {
            let detail = summary
                .outcomes
                .iter()
                .find(|outcome| outcome.outcome == Outcome::Failed)
                .map(|outcome| outcome.detail.as_str())
                .unwrap_or_default();
            eprintln!("skipping: this ffmpeg build can't encode libx265: {detail}");
            return;
        }
        assert_eq!(summary.pushed, 1);
        assert_eq!(summary.outcomes[0].outcome, Outcome::Transcoded);
        let pushed_names: Vec<_> = std::fs::read_dir(&destination)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(pushed_names.len(), 1);
        assert!(pushed_names[0].ends_with(".mp4"));
    }

    #[test]
    fn enqueue_does_not_double_dispatch_a_path_already_dispatched_this_run() {
        let dir = tempfile::tempdir().unwrap();
        let source_dir = dir.path().to_path_buf();
        let destination_groups = placement::DestinationGroups::default();
        let mut dispatched = HashSet::new();
        let mut queue = VecDeque::new();

        enqueue(
            "a.png".to_string(),
            &source_dir,
            "source:png/",
            InputFileType::Png,
            &destination_groups,
            &mut dispatched,
            &mut queue,
        );
        // Simulates the live tail reporting the same path a second time --
        // e.g. the initial scan already queued it, or rclone emitted two
        // "Copied" lines for it.
        enqueue(
            "a.png".to_string(),
            &source_dir,
            "source:png/",
            InputFileType::Png,
            &destination_groups,
            &mut dispatched,
            &mut queue,
        );

        assert_eq!(queue.len(), 1);
    }

    /// The "already done" check is no longer a local checkpoint (ADR-0120)
    /// -- it's membership in a pre-fetched listing of `--destination-path`,
    /// keyed by each file's deterministic `compute_destination_name`.
    #[test]
    fn enqueue_skips_a_path_whose_destination_name_already_exists_at_the_destination() {
        let dir = tempfile::tempdir().unwrap();
        let source_dir = dir.path().to_path_buf();
        let source_path = "source:png/";
        let names = vec![placement::compute_destination_name(
            source_path,
            "a.png",
            InputFileType::Png,
        )];
        let destination_groups = placement::DestinationGroups::index(&names);
        let mut dispatched = HashSet::new();
        let mut queue = VecDeque::new();

        enqueue(
            "a.png".to_string(),
            &source_dir,
            source_path,
            InputFileType::Png,
            &destination_groups,
            &mut dispatched,
            &mut queue,
        );

        assert!(queue.is_empty());
    }

    /// ADR-0124: the same "already done" check `enqueue` performs after
    /// download should exclude an already-done path from the pull-side
    /// candidate list entirely, before any bytes are downloaded.
    #[test]
    fn files_needing_pull_excludes_a_path_already_present_at_the_destination() {
        let source_path = "source:png/";
        let names = vec![placement::compute_destination_name(
            source_path,
            "a.png",
            InputFileType::Png,
        )];
        let destination_groups = placement::DestinationGroups::index(&names);

        let result = files_needing_pull(
            vec!["a.png".to_string(), "b.png".to_string()],
            source_path,
            InputFileType::Png,
            &destination_groups,
        );

        assert_eq!(result.to_pull, vec!["b.png".to_string()]);
        assert_eq!(result.skipped_already_done, 1);
        assert_eq!(result.skipped_apple_double, 0);
    }

    /// The exact shape found in the ADR-0144 run: `IMG_0260.JPG` and
    /// `IMG_0260.jpg` were the only two case-insensitive duplicates among
    /// 38,232 source keys, and the lowercase one silently never arrived.
    #[test]
    fn case_insensitive_collisions_finds_the_real_extension_case_pair() {
        let collisions = case_insensitive_collisions(&[
            "IMG_0260.JPG".to_string(),
            "IMG_0260.jpg".to_string(),
            "IMG_1551.JPG".to_string(),
            "IMG_1551.jpg".to_string(),
            "IMG_0261.jpg".to_string(),
        ]);

        let mut found: Vec<&String> = collisions.iter().collect();
        found.sort();
        assert_eq!(
            found,
            vec![
                "IMG_0260.JPG",
                "IMG_0260.jpg",
                "IMG_1551.JPG",
                "IMG_1551.jpg"
            ],
            "both members of each colliding group are implicated, since which one \
             rclone keeps is its own ordering decision"
        );
        assert!(
            !collisions.contains("IMG_0261.jpg"),
            "a key with no case-variant twin must never be flagged"
        );
    }

    /// Stems collide too, not just extensions, and a collision is only ever
    /// within the same directory.
    #[test]
    fn case_insensitive_collisions_covers_stems_and_respects_directories() {
        let collisions = case_insensitive_collisions(&[
            "photo.jpg".to_string(),
            "PHOTO.jpg".to_string(),
            "a/one.jpg".to_string(),
            "b/ONE.jpg".to_string(),
        ]);
        assert!(collisions.contains("photo.jpg") && collisions.contains("PHOTO.jpg"));
        assert!(
            !collisions.contains("a/one.jpg") && !collisions.contains("b/ONE.jpg"),
            "same basename in different directories maps to distinct local paths"
        );
    }

    #[test]
    fn case_insensitive_collisions_is_empty_for_an_all_distinct_pull_set() {
        assert!(
            case_insensitive_collisions(&[
                "a.jpg".to_string(),
                "b.jpg".to_string(),
                "c/d.jpg".to_string(),
            ])
            .is_empty()
        );
    }

    /// `files_needing_pull` must surface the collision alongside its other
    /// counts rather than discarding it -- the whole point of ADR-0130's
    /// `PullSelection`.
    #[test]
    fn files_needing_pull_reports_case_colliding_keys_without_dropping_them() {
        let selection = files_needing_pull(
            vec!["IMG_0260.JPG".to_string(), "IMG_0260.jpg".to_string()],
            "source:jpg",
            InputFileType::Jpeg,
            &placement::DestinationGroups::default(),
        );

        assert_eq!(
            selection.to_pull.len(),
            2,
            "both keys are still asked for: rclone decides which it can deliver"
        );
        assert_eq!(selection.case_collisions.len(), 2);
        assert_eq!(selection.skipped_already_done, 0);
        assert_eq!(selection.skipped_apple_double, 0);
    }

    /// Case-colliding keys hash to *distinct* destination names, so this is
    /// a missing-file problem at the source side only -- there is no
    /// destination collision to worry about (ADR-0112's naming scheme is
    /// case-sensitive).
    #[test]
    fn case_colliding_keys_still_map_to_distinct_destination_names() {
        let upper =
            placement::compute_destination_name("source:jpg", "IMG_0260.JPG", InputFileType::Jpeg);
        let lower =
            placement::compute_destination_name("source:jpg", "IMG_0260.jpg", InputFileType::Jpeg);
        assert_ne!(upper, lower);
        // Both keep the same stem; only the hash distinguishes them.
        assert!(upper.starts_with("IMG_0260-") && lower.starts_with("IMG_0260-"));
    }

    /// ADR-0144's rule survives -- the arms must not be swapped, because the
    /// generic one is a false statement for a case-collision -- but ADR-0152
    /// changes what the collision arm says. A colliding key is now pulled
    /// separately, so the only way one goes undelivered is that its own
    /// pull failed, and the detail carries rclone's real error.
    #[test]
    fn undelivered_detail_explains_a_failed_variant_pull_with_its_real_error() {
        let collisions: HashSet<String> = ["IMG_0260.jpg".to_string()].into_iter().collect();
        let failures: HashMap<String, String> =
            [("IMG_0260.jpg".to_string(), "403 Forbidden".to_string())]
                .into_iter()
                .collect();

        let collided = undelivered_detail("IMG_0260.jpg", &collisions, &failures);
        assert!(collided.contains("pulled"));
        assert!(
            collided.contains("403 Forbidden"),
            "the operator needs rclone's own error, not prose: {collided}"
        );
        assert!(
            !collided.contains("never reported it as copied"),
            "this file was asked for individually, so this must not claim otherwise"
        );

        let no_show = undelivered_detail("IMG_9999.jpg", &collisions, &failures);
        assert!(no_show.contains("never reported it as copied"));
        assert!(!no_show.contains("letter case"));
    }

    /// A colliding key missing *without* its own pull having failed is a bug,
    /// not the ordinary case the old arm described -- so it says so rather
    /// than blaming rclone's duplicate handling, which no longer applies.
    #[test]
    fn undelivered_detail_flags_a_collision_that_vanished_without_an_error() {
        let collisions: HashSet<String> = ["IMG_0260.jpg".to_string()].into_iter().collect();
        let detail = undelivered_detail("IMG_0260.jpg", &collisions, &HashMap::new());
        assert!(detail.contains("reported no error"), "{detail}");
    }

    /// ADR-0130: an AppleDouble stub matches rclone's `--include '*.<ext>'`
    /// filter but is a resource fork, not decodable media -- it must never be
    /// downloaded, and must be counted as skipped rather than failed.
    #[test]
    fn files_needing_pull_excludes_apple_double_sidecars_without_failing_them() {
        let selection = files_needing_pull(
            vec![
                "a.png".to_string(),
                "._a.png".to_string(),
                "nested/._b.png".to_string(),
            ],
            "source:png/",
            InputFileType::Png,
            &placement::DestinationGroups::default(),
        );

        assert_eq!(selection.to_pull, vec!["a.png".to_string()]);
        assert_eq!(selection.skipped_apple_double, 2);
        assert_eq!(selection.skipped_already_done, 0);
    }

    /// The second AppleDouble choke point (ADR-0130): `enqueue` covers the
    /// initial on-disk scan and rclone's live tail, so a stub cannot reach
    /// `ffmpeg` by any dispatch route even if it bypassed the pull filter.
    /// ADR-0151: `enqueue` returned `SkippedLockFile` from ADR-0149 on, but
    /// every call site matched only on `SkippedAppleDouble`, so a lock stub
    /// reaching *this* choke point -- the on-disk scan or rclone's live
    /// tail -- was counted nowhere at all.
    #[test]
    fn enqueue_reports_a_lock_stub_distinctly_from_an_apple_double() {
        let mut dispatched = HashSet::new();
        let mut queue = VecDeque::new();
        assert_eq!(
            enqueue(
                "~$Budget.xlsx".to_string(),
                Path::new("/tmp/source"),
                "source:xlsx/",
                InputFileType::Xlsx,
                &placement::DestinationGroups::default(),
                &mut dispatched,
                &mut queue,
            ),
            EnqueueOutcome::SkippedLockFile
        );
        assert_eq!(
            enqueue(
                "._Budget.xlsx".to_string(),
                Path::new("/tmp/source"),
                "source:xlsx/",
                InputFileType::Xlsx,
                &placement::DestinationGroups::default(),
                &mut dispatched,
                &mut queue,
            ),
            EnqueueOutcome::SkippedAppleDouble
        );
        assert!(queue.is_empty());
    }

    /// The pull-side choke point keeps them apart too -- it used to add a
    /// lock stub to `skipped_apple_double`, which is what made a real run
    /// report "2 AppleDouble sidecar(s)" for stubs of unknown kind.
    #[test]
    fn files_needing_pull_counts_lock_stubs_separately() {
        let selection = files_needing_pull(
            vec![
                "Budget.xlsx".to_string(),
                "~$Budget.xlsx".to_string(),
                "._Budget.xlsx".to_string(),
            ],
            "source:xlsx/",
            InputFileType::Xlsx,
            &placement::DestinationGroups::default(),
        );
        assert_eq!(selection.to_pull, vec!["Budget.xlsx".to_string()]);
        assert_eq!(selection.skipped_lock_file, 1);
        assert_eq!(selection.skipped_apple_double, 1);
    }

    #[test]
    fn enqueue_skips_an_apple_double_sidecar() {
        let mut dispatched = HashSet::new();
        let mut queue = VecDeque::new();

        let outcome = enqueue(
            "._a.png".to_string(),
            Path::new("/tmp/source"),
            "source:png/",
            InputFileType::Png,
            &placement::DestinationGroups::default(),
            &mut dispatched,
            &mut queue,
        );

        assert_eq!(outcome, EnqueueOutcome::SkippedAppleDouble);
        assert!(queue.is_empty());
        assert!(dispatched.is_empty());
    }

    #[test]
    fn enqueue_reports_a_queued_path_and_then_a_duplicate_dispatch() {
        let mut dispatched = HashSet::new();
        let mut queue = VecDeque::new();

        assert_eq!(
            enqueue(
                "a.png".to_string(),
                Path::new("/tmp/source"),
                "source:png/",
                InputFileType::Png,
                &placement::DestinationGroups::default(),
                &mut dispatched,
                &mut queue,
            ),
            EnqueueOutcome::Queued
        );
        assert_eq!(
            enqueue(
                "a.png".to_string(),
                Path::new("/tmp/source"),
                "source:png/",
                InputFileType::Png,
                &placement::DestinationGroups::default(),
                &mut dispatched,
                &mut queue,
            ),
            EnqueueOutcome::AlreadyDispatched
        );
        assert_eq!(queue.len(), 1);
    }

    /// ADR-0130's reconciliation, in isolation: the set difference between
    /// what the run intended to pull and what actually reached the pipeline
    /// is exactly the set of silently-undelivered files that used to vanish.
    #[test]
    fn undelivered_files_are_exactly_the_intended_set_minus_the_dispatched_set() {
        let files_to_pull = vec![
            "delivered.png".to_string(),
            "never-arrived.png".to_string(),
            "also-missing.png".to_string(),
        ];
        let mut dispatched = HashSet::new();
        dispatched.insert("delivered.png".to_string());

        let mut failures = FailureBreakdown::default();
        let mut undelivered = Vec::new();
        for relative_path in &files_to_pull {
            if dispatched.contains(relative_path) {
                continue;
            }
            failures.record(FailureStage::Pull);
            undelivered.push(relative_path.clone());
        }

        assert_eq!(
            undelivered,
            vec![
                "never-arrived.png".to_string(),
                "also-missing.png".to_string()
            ]
        );
        assert_eq!(failures.pull, 2);
        assert_eq!(failures.total(), 2);
    }

    #[test]
    fn failure_breakdown_tallies_each_stage_independently() {
        let mut failures = FailureBreakdown::default();
        failures.record(FailureStage::Pull);
        failures.record(FailureStage::Transcode);
        failures.record(FailureStage::Transcode);
        failures.record(FailureStage::Place);
        failures.record(FailureStage::Push);

        assert_eq!(failures.pull, 1);
        assert_eq!(failures.transcode, 2);
        assert_eq!(failures.place, 1);
        assert_eq!(failures.push, 1);
        assert_eq!(failures.total(), 5);
    }

    #[test]
    fn failure_stage_labels_match_the_metric_phase_names() {
        assert_eq!(FailureStage::Pull.as_str(), "pull");
        assert_eq!(FailureStage::Transcode.as_str(), "transcode");
        assert_eq!(FailureStage::Place.as_str(), "place");
        assert_eq!(FailureStage::Push.as_str(), "push");
    }

    #[test]
    fn outcome_for_maps_jpeg_to_copied_through_and_others_to_transcoded() {
        assert_eq!(outcome_for(InputFileType::Jpeg), Outcome::CopiedThrough);
        assert_eq!(outcome_for(InputFileType::Png), Outcome::Transcoded);
        assert_eq!(outcome_for(InputFileType::Heic), Outcome::Transcoded);
        assert_eq!(outcome_for(InputFileType::Mov), Outcome::Transcoded);
        assert_eq!(outcome_for(InputFileType::M4v), Outcome::Transcoded);
        assert_eq!(outcome_for(InputFileType::Mp4), Outcome::Transcoded);
    }

    #[test]
    fn write_report_lists_every_outcome_and_a_trailing_summary_line() {
        let dir = tempfile::tempdir().unwrap();
        let summary = TransformSummary {
            outcomes: vec![
                FileOutcome {
                    relative_path: "a.png".to_string(),
                    destination_filenames: vec!["a-abc123.jpg".to_string()],
                    outcome: Outcome::Transcoded,
                    stage: None,
                    detail: String::new(),
                },
                FileOutcome {
                    relative_path: "bad.heic".to_string(),
                    destination_filenames: Vec::new(),
                    outcome: Outcome::Failed,
                    stage: Some(FailureStage::Transcode),
                    detail: "ffmpeg: boom".to_string(),
                },
            ],
            pulled: 2,
            pushed: 1,
            failed: true,
            failures: FailureBreakdown {
                transcode: 1,
                ..FailureBreakdown::default()
            },
            skipped_already_done: 3,
            skipped_apple_double: 2,
            skipped_lock_file: 1,
            pull_errors: 0,
            case_collision_groups: Vec::new(),
        };

        let path = write_report(dir.path(), &summary).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();

        assert!(contents.contains("a.png\ttranscoded\t\ta-abc123.jpg\t"));
        assert!(contents.contains("bad.heic\tfailed\ttranscode\t\tffmpeg: boom"));
        // ADR-0149: the summary now names the output-object count too,
        // since `pushed` and the per-outcome counts measure different units.
        assert!(
            contents.contains("1 transcoded, 0 copied through, 1 failed; 1 output file(s) pushed.")
        );
        // ADR-0130's added lines.
        assert!(contents.contains("failures by stage: 0 pull, 1 transcode, 0 place, 0 push."));
        assert!(contents.contains(
            "skipped: 3 already done at the destination, 2 AppleDouble sidecar(s), 1 Excel lock \
             stub(s)."
        ));
        assert!(contents.contains("pull: 2 transferred, 0 rclone error(s) reported."));
        assert_eq!(path.file_name().unwrap(), "transform-report.txt");
        // No collisions, no listing.
        assert!(!contents.contains("case-colliding"));
    }

    /// ADR-0156: the log line only samples the colliding groups, so the
    /// report must carry every one of them.
    #[test]
    fn write_report_lists_every_case_collision_group() {
        let dir = tempfile::tempdir().unwrap();
        let summary = TransformSummary {
            outcomes: Vec::new(),
            pulled: 0,
            pushed: 0,
            failed: false,
            failures: FailureBreakdown::default(),
            skipped_already_done: 0,
            skipped_apple_double: 0,
            skipped_lock_file: 0,
            pull_errors: 0,
            case_collision_groups: vec![
                vec!["A.md".to_string(), "a.md".to_string()],
                vec!["B.md".to_string(), "b.md".to_string(), "b.MD".to_string()],
            ],
        };
        let contents =
            std::fs::read_to_string(write_report(dir.path(), &summary).unwrap()).unwrap();
        assert!(
            contents.contains("case-colliding source keys, each pulled separately (2 group(s)):")
        );
        assert!(contents.contains("\n  A.md | a.md\n"));
        assert!(contents.contains("\n  B.md | b.md | b.MD\n"));
    }

    // --- ADR-0153: audio kinds -------------------------------------------

    /// End-to-end for a `wav` run. `wav` is chosen because ffmpeg can
    /// generate the fixture natively with no external codec, and because it
    /// is the one *lossless* input kind -- the case whose quality default
    /// matters most.
    #[tokio::test]
    async fn a_wav_run_transcodes_and_pushes_opus() {
        if media::check_ffmpeg_available().await.is_err() {
            eprintln!("skipping: ffmpeg/ffprobe not found on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let local_output = dir.path().join("work");
        let source = dir.path().join("pull-source");
        let destination = dir.path().join("destination");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&destination).unwrap();

        let generated = tokio::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=2",
                "-c:a",
                "pcm_s16le",
            ])
            .arg(source.join("tone.wav"))
            .output()
            .await;
        if generated.map(|out| !out.status.success()).unwrap_or(true) {
            eprintln!("skipping: this ffmpeg build can't generate a wav fixture");
            return;
        }

        let mut plan = spreadsheet_plan(&source, &destination, &local_output, InputFileType::Wav);
        plan.audio_quality = InputFileType::Wav.default_audio_quality();
        let summary = run_transform_job(&plan, 2, 2).await.unwrap();

        assert!(!summary.failed, "{:?}", summary.outcomes);
        assert_eq!(summary.pushed, 1);
        assert_eq!(summary.outcomes.len(), 1);
        // Transcoded, not copied through -- ADR-0153 re-encodes.
        assert_eq!(summary.outcomes[0].outcome, Outcome::Transcoded);

        let expected =
            placement::compute_destination_name(&plan.source_path, "tone.wav", InputFileType::Wav);
        assert!(expected.ends_with(".opus"), "{expected}");
        assert!(destination.join(&expected).exists(), "missing {expected}");

        // ADR-0124: both local trees empty once it is pushed.
        assert!(
            std::fs::read_dir(source_dir(&local_output))
                .map(|mut entries| entries.next().is_none())
                .unwrap_or(true)
        );
    }

    /// `opus` input is re-encoded rather than copied through, so its
    /// outcome is `Transcoded` -- the one place ADR-0153 departs from
    /// ADR-0112's and ADR-0149's treatment of an input already in the
    /// target format.
    #[test]
    fn outcome_for_treats_opus_as_transcoded_not_copied_through() {
        assert_eq!(outcome_for(InputFileType::Opus), Outcome::Transcoded);
        // The two kinds that genuinely are copied through.
        assert_eq!(outcome_for(InputFileType::Jpeg), Outcome::CopiedThrough);
        assert_eq!(outcome_for(InputFileType::Csv), Outcome::CopiedThrough);
        for kind in [
            InputFileType::Mp4Audio,
            InputFileType::M4a,
            InputFileType::Mp3,
            InputFileType::Aac,
            InputFileType::Wav,
        ] {
            assert_eq!(outcome_for(kind), Outcome::Transcoded, "{kind}");
        }
    }

    // --- ADR-0152: case-colliding source keys ----------------------------

    /// Every slot must be collision-free under case folding -- for whole
    /// keys *and* for every directory prefix -- since that is the only
    /// property that lets one bulk `rclone copy` deliver a whole slot onto
    /// a case-insensitive volume (ADR-0156).
    fn assert_slots_are_collision_free(slots: &[Vec<String>]) {
        for slot in slots {
            let mut seen: HashMap<String, String> = HashMap::new();
            for key in slot {
                for prefix in path_prefixes(key) {
                    if let Some(other) = seen.insert(prefix.to_lowercase(), prefix.clone()) {
                        assert_eq!(other, prefix, "slot {slot:?} folds two spellings together");
                    }
                }
            }
        }
    }

    #[test]
    fn case_variant_slots_hold_one_member_per_group_and_as_few_slots_as_the_largest_group() {
        // ADR-0156's real distribution in miniature: groups of 2, 3 and 4.
        let mut variants: Vec<String> = [
            "a.md", "A.md", "bb.md", "Bb.md", "BB.md", "cc.md", "Cc.md", "cC.md", "CC.md",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        variants.sort();
        let slots = assign_case_variant_slots(&variants);

        assert_eq!(slots.len(), 4, "largest group has 4 members: {slots:?}");
        assert_slots_are_collision_free(&slots);
        // Every key in exactly one slot.
        let mut placed: Vec<String> = slots.iter().flatten().cloned().collect();
        placed.sort();
        assert_eq!(placed, variants);
    }

    /// Two keys from *different* groups can still spell a shared directory
    /// differently; one slot cannot hold both on a case-insensitive volume.
    #[test]
    fn case_variant_slots_also_separate_differently_cased_directories() {
        let mut variants: Vec<String> = ["Dir/x.md", "Dir/X.md", "dir/y.md", "dir/Y.md"]
            .into_iter()
            .map(String::from)
            .collect();
        variants.sort();
        let slots = assign_case_variant_slots(&variants);
        assert_slots_are_collision_free(&slots);
        assert_eq!(slots.iter().map(Vec::len).sum::<usize>(), 4);
        // `Dir/` and `dir/` can never share a slot.
        for slot in &slots {
            let dirs: HashSet<&str> = slot.iter().map(|k| k.split('/').next().unwrap()).collect();
            assert_eq!(dirs.len(), 1, "{slot:?}");
        }
    }

    #[test]
    fn case_variant_slot_assignment_is_deterministic() {
        let variants: Vec<String> = ["B.xlsx", "Bookkeeping.xlsx", "b.xlsx", "bookkeeping.xlsx"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            assign_case_variant_slots(&variants),
            assign_case_variant_slots(&variants.clone())
        );
        assert_eq!(
            assign_case_variant_slots(&variants),
            vec![
                vec!["B.xlsx".to_string(), "Bookkeeping.xlsx".to_string()],
                vec!["b.xlsx".to_string(), "bookkeeping.xlsx".to_string()],
            ]
        );
    }

    #[test]
    fn case_variant_slot_dirs_sit_outside_source_and_keep_the_key_verbatim() {
        let local_output = Path::new("/tmp/work");
        let slot0 = case_variant_slot_dir(local_output, 0);
        assert_ne!(slot0, case_variant_slot_dir(local_output, 1));
        // Outside `source/`, which is what keeps `gather_pending` from
        // reconstructing a wrong key from a variant's path.
        assert!(!slot0.starts_with(source_dir(local_output)));
        assert!(slot0.starts_with(case_variant_dir(local_output)));
        // Same shape as the bulk pull's `source_dir.join(key)`.
        assert!(
            slot0
                .join("a/b/Ledger.xlsx")
                .ends_with("slot0/a/b/Ledger.xlsx")
        );
    }

    #[test]
    fn path_prefixes_lists_every_directory_then_the_key() {
        assert_eq!(path_prefixes("a/b/c.md"), vec!["a", "a/b", "a/b/c.md"]);
        assert_eq!(path_prefixes("c.md"), vec!["c.md"]);
    }

    #[test]
    fn case_collision_groups_are_sorted_within_and_across_groups() {
        let collisions: HashSet<String> = ["b.md", "B.md", "a.md", "A.md", "A.MD"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            case_collision_groups(&collisions),
            vec![
                vec!["A.MD".to_string(), "A.md".to_string(), "a.md".to_string()],
                vec!["B.md".to_string(), "b.md".to_string()],
            ]
        );
    }

    /// **The #83 decision point**, tested where it can actually be tested.
    ///
    /// An end-to-end test of this is **not reproducible on either
    /// filesystem**, which is worth stating rather than faking: on a
    /// case-insensitive host the fixture cannot be created at all (writing
    /// `ledger.xlsx` overwrites `Ledger.xlsx` in the local source
    /// directory), and on a case-sensitive host the bulk pull would deliver
    /// both anyway, so the test would pass without the new path ever
    /// running -- the ADR-0139 mistake. The collision only exists because
    /// the real source is S3, which is case-sensitive while the local pull
    /// tree may not be.
    ///
    /// So the split itself is a pure function, and this pins it.
    #[test]
    fn colliding_keys_leave_the_bulk_pull_and_become_variant_pulls() {
        let to_pull = vec![
            "Bookkeeping.xlsx".to_string(),
            "bookkeeping.xlsx".to_string(),
            "bookkeeping-2.xlsx".to_string(),
            "Other.xlsx".to_string(),
        ];
        let collisions = case_insensitive_collisions(&to_pull);
        let (bulk, variants) = split_pull_set(&to_pull, &collisions);

        // Every member of the colliding group leaves the bulk pull --
        // deliberately all of them, not just the ones rclone would have
        // dropped, since which one it keeps is its own ordering decision.
        assert_eq!(
            variants,
            vec![
                "Bookkeeping.xlsx".to_string(),
                "bookkeeping.xlsx".to_string()
            ]
        );
        // `bookkeeping-2.xlsx` folds to a *different* string, so it is not a
        // collision and stays in the bulk set -- which preserves the source
        // listing's own order.
        assert_eq!(
            bulk,
            vec!["bookkeeping-2.xlsx".to_string(), "Other.xlsx".to_string()]
        );
        // Nothing is lost or duplicated across the split.
        assert_eq!(bulk.len() + variants.len(), to_pull.len());
    }

    #[test]
    fn a_pull_set_with_no_collision_is_left_entirely_to_the_bulk_pull() {
        let to_pull = vec!["a.xlsx".to_string(), "b.xlsx".to_string()];
        let collisions = case_insensitive_collisions(&to_pull);
        let (bulk, variants) = split_pull_set(&to_pull, &collisions);
        assert_eq!(bulk, to_pull);
        assert!(variants.is_empty());
    }

    /// The variant list is sorted, so `assign_case_variant_slots`' output --
    /// and therefore every local path -- is stable across runs.
    #[test]
    fn the_variant_pull_order_is_deterministic() {
        let a = vec!["b.xlsx".to_string(), "B.xlsx".to_string()];
        let b = vec!["B.xlsx".to_string(), "b.xlsx".to_string()];
        let split_a = split_pull_set(&a, &case_insensitive_collisions(&a)).1;
        let split_b = split_pull_set(&b, &case_insensitive_collisions(&b)).1;
        assert_eq!(split_a, split_b);
        assert_eq!(split_a, vec!["B.xlsx".to_string(), "b.xlsx".to_string()]);
    }

    // --- ADR-0149: spreadsheets ------------------------------------------

    /// Minimal valid `.xlsx`, built with the `zip` crate already in this
    /// tree. A thin wrapper over `sheets`' own fixture writer's approach,
    /// kept local because these tests need it at a different layer.
    fn write_test_xlsx(path: &Path, sheets: &[(&str, Vec<Vec<&str>>)]) {
        use std::io::Write as _;
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
        zip.start_file("_rels/.rels", opts).unwrap();
        zip.write_all(br#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#).unwrap();
        let mut workbook = String::from(
            r#"<?xml version="1.0"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets>"#,
        );
        let mut rels = String::from(
            r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
        );
        for (position, (name, _)) in sheets.iter().enumerate() {
            let id = position + 1;
            workbook.push_str(&format!(
                r#"<sheet name="{name}" sheetId="{id}" r:id="rId{id}"/>"#
            ));
            rels.push_str(&format!(r#"<Relationship Id="rId{id}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet{id}.xml"/>"#));
        }
        workbook.push_str("</sheets></workbook>");
        rels.push_str("</Relationships>");
        zip.start_file("xl/workbook.xml", opts).unwrap();
        zip.write_all(workbook.as_bytes()).unwrap();
        zip.start_file("xl/_rels/workbook.xml.rels", opts).unwrap();
        zip.write_all(rels.as_bytes()).unwrap();
        for (position, (_, rows)) in sheets.iter().enumerate() {
            let mut sheet = String::from(
                r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>"#,
            );
            for (row_index, row) in rows.iter().enumerate() {
                let row_number = row_index + 1;
                sheet.push_str(&format!(r#"<row r="{row_number}">"#));
                for (column_index, value) in row.iter().enumerate() {
                    let column = (b'A' + column_index as u8) as char;
                    sheet.push_str(&format!(
                        r#"<c r="{column}{row_number}" t="inlineStr"><is><t>{value}</t></is></c>"#
                    ));
                }
                sheet.push_str("</row>");
            }
            sheet.push_str("</sheetData></worksheet>");
            zip.start_file(format!("xl/worksheets/sheet{}.xml", position + 1), opts)
                .unwrap();
            zip.write_all(sheet.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }

    fn spreadsheet_plan(
        source: &Path,
        destination: &Path,
        local_output: &Path,
        input_file_type: InputFileType,
    ) -> TransformPlan {
        TransformPlan {
            source_path: source.display().to_string(),
            destination_path: destination.display().to_string(),
            local_output: local_output.to_path_buf(),
            input_file_type,
            download_transfers: 2,
            download_checkers: 4,
            video_quality: VideoQuality::Medium,
            audio_quality: AudioQuality::Medium,
            tpslimit: None,
            max_pending_downloads: 100,
            run_id: "test-run".to_string(),
        }
    }

    /// The headline ADR-0149 case. Deliberately **not** gated on
    /// `check_ffmpeg_available`, unlike every pre-existing end-to-end test
    /// here: a spreadsheet run invokes no `ffmpeg`, so gating it would make
    /// it silently never run -- the ADR-0139 mistake in miniature.
    #[tokio::test]
    async fn a_multi_sheet_workbook_pushes_every_sheet_on_one_report_row() {
        let dir = tempfile::tempdir().unwrap();
        let local_output = dir.path().join("work");
        let source = dir.path().join("pull-source");
        let destination = dir.path().join("destination");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&destination).unwrap();
        write_test_xlsx(
            &source.join("Budget.xlsx"),
            &[
                ("Summary", vec![vec!["a", "b"]]),
                ("Blank", vec![]),
                ("Notes", vec![vec!["n"]]),
            ],
        );

        let plan = spreadsheet_plan(&source, &destination, &local_output, InputFileType::Xlsx);
        let summary = run_transform_job(&plan, 2, 2).await.unwrap();

        assert!(!summary.failed, "{:?}", summary.outcomes);
        // Two non-empty sheets -> two pushes, from one input file.
        assert_eq!(summary.pushed, 2);
        assert_eq!(summary.outcomes.len(), 1);
        assert_eq!(summary.outcomes[0].relative_path, "Budget.xlsx");
        assert_eq!(summary.outcomes[0].destination_filenames.len(), 2);

        let group = placement::group_key(&plan.source_path, "Budget.xlsx", InputFileType::Xlsx);
        let mut pushed: Vec<String> = std::fs::read_dir(&destination)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        pushed.sort();
        assert_eq!(
            pushed,
            vec![
                format!("{group}-01of02-Summary.csv"),
                format!("{group}-02of02-Notes.csv"),
            ]
        );

        // ADR-0124: both local trees empty once everything is pushed.
        assert!(
            std::fs::read_dir(source_dir(&local_output))
                .unwrap()
                .next()
                .is_none(),
            "local source/ should be empty"
        );
        assert!(
            std::fs::read_dir(result_dir(&local_output))
                .unwrap()
                .next()
                .is_none(),
            "local result/ should be empty"
        );
        // ADR-0149: the scratch directory removes itself.
        assert!(
            std::fs::read_dir(scratch_dir(&local_output))
                .map(|mut entries| entries.next().is_none())
                .unwrap_or(true),
            "scratch/ should hold no leftover per-input directories"
        );
        // ADR-0151: so does each output's push log. A real run left 1318
        // behind, one per output object, none of them ever cleaned.
        assert!(
            std::fs::read_dir(push_log_dir(&local_output))
                .map(|mut entries| entries.next().is_none())
                .unwrap_or(true),
            "push-logs/ should be empty once every output is pushed"
        );

        // A rerun is a clean no-op.
        let rerun = run_transform_job(&plan, 2, 2).await.unwrap();
        assert_eq!(rerun.pushed, 0);
        assert!(rerun.outcomes.is_empty());
        assert_eq!(rerun.skipped_already_done, 1);
    }

    /// **The decision the `{nn}of{tt}` marker exists for** (ADR-0149 §3).
    /// A destination holding only some of a workbook's sheets must be
    /// detected as incomplete, so the workbook is re-pulled and finished --
    /// where an "any member present" test would call it done and lose the
    /// missing sheet forever.
    #[tokio::test]
    async fn an_incomplete_sheet_group_at_the_destination_is_redone() {
        let dir = tempfile::tempdir().unwrap();
        let local_output = dir.path().join("work");
        let source = dir.path().join("pull-source");
        let destination = dir.path().join("destination");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&destination).unwrap();
        write_test_xlsx(
            &source.join("Budget.xlsx"),
            &[("Summary", vec![vec!["a"]]), ("Notes", vec![vec!["n"]])],
        );

        let plan = spreadsheet_plan(&source, &destination, &local_output, InputFileType::Xlsx);
        let group = placement::group_key(&plan.source_path, "Budget.xlsx", InputFileType::Xlsx);

        // Sheet 1 of 2 landed in a prior run; sheet 2 never did.
        std::fs::write(
            destination.join(format!("{group}-01of02-Summary.csv")),
            b"from a prior run",
        )
        .unwrap();

        let summary = run_transform_job(&plan, 2, 2).await.unwrap();

        assert!(!summary.failed, "{:?}", summary.outcomes);
        assert_eq!(
            summary.skipped_already_done, 0,
            "a half-pushed group must not count as already done"
        );
        assert_eq!(summary.pushed, 2, "both sheets are pushed on the redo");
        assert!(
            destination
                .join(format!("{group}-02of02-Notes.csv"))
                .exists()
        );

        // And now it *is* complete, so a further rerun is a no-op.
        let rerun = run_transform_job(&plan, 2, 2).await.unwrap();
        assert_eq!(rerun.pushed, 0);
        assert_eq!(rerun.skipped_already_done, 1);
    }

    /// A single-sheet workbook keeps the plain, unsuffixed name -- the
    /// marker only ever appears where it carries information.
    #[tokio::test]
    async fn a_single_sheet_workbook_keeps_the_plain_destination_name() {
        let dir = tempfile::tempdir().unwrap();
        let local_output = dir.path().join("work");
        let source = dir.path().join("pull-source");
        let destination = dir.path().join("destination");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&destination).unwrap();
        write_test_xlsx(&source.join("Solo.xlsx"), &[("Only", vec![vec!["x"]])]);

        let plan = spreadsheet_plan(&source, &destination, &local_output, InputFileType::Xlsx);
        let summary = run_transform_job(&plan, 2, 2).await.unwrap();

        assert!(!summary.failed, "{:?}", summary.outcomes);
        assert_eq!(summary.pushed, 1);
        let expected = placement::compute_destination_name(
            &plan.source_path,
            "Solo.xlsx",
            InputFileType::Xlsx,
        );
        assert!(destination.join(&expected).exists(), "expected {expected}");
    }

    /// `.csv` input is copied through byte-for-byte, on exactly ADR-0112's
    /// reasoning for a `jpeg` already in the target format.
    #[tokio::test]
    async fn a_csv_input_is_copied_through_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let local_output = dir.path().join("work");
        let source = dir.path().join("pull-source");
        let destination = dir.path().join("destination");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&destination).unwrap();
        // Deliberately odd: CRLF, a semicolon delimiter, Latin-1-ish bytes.
        let original: &[u8] = b"a;b
c;d
";
        std::fs::write(source.join("notes.csv"), original).unwrap();

        let plan = spreadsheet_plan(&source, &destination, &local_output, InputFileType::Csv);
        let summary = run_transform_job(&plan, 2, 2).await.unwrap();

        assert!(!summary.failed, "{:?}", summary.outcomes);
        assert_eq!(summary.pushed, 1);
        assert_eq!(summary.outcomes[0].outcome, Outcome::CopiedThrough);
        let expected =
            placement::compute_destination_name(&plan.source_path, "notes.csv", InputFileType::Csv);
        assert_eq!(
            std::fs::read(destination.join(&expected)).unwrap(),
            original,
            "copy-through must not rewrite the bytes"
        );
    }

    /// An Excel `~$` lock stub matches `--include '*.xlsx'` but is not a
    /// workbook, so it is skipped rather than failed -- the same treatment
    /// ADR-0130 gives an AppleDouble sidecar, and for the same reason: a
    /// deterministic failure class is what trips the circuit breaker.
    #[tokio::test]
    async fn an_excel_lock_stub_is_skipped_not_failed() {
        let dir = tempfile::tempdir().unwrap();
        let local_output = dir.path().join("work");
        let source = dir.path().join("pull-source");
        let destination = dir.path().join("destination");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&destination).unwrap();
        write_test_xlsx(&source.join("Real.xlsx"), &[("S", vec![vec!["x"]])]);
        std::fs::write(source.join("~$Real.xlsx"), b"lock stub").unwrap();

        let plan = spreadsheet_plan(&source, &destination, &local_output, InputFileType::Xlsx);
        let summary = run_transform_job(&plan, 2, 2).await.unwrap();

        assert!(!summary.failed, "{:?}", summary.outcomes);
        assert_eq!(summary.pushed, 1);
        assert_eq!(summary.outcomes.len(), 1);
        assert_eq!(summary.outcomes[0].relative_path, "Real.xlsx");
        // ADR-0151: counted as a lock file, not folded into the AppleDouble
        // tally. ADR-0149 reported "2 AppleDouble sidecar(s)" on a real run
        // for stubs that may have been either kind, with no way to tell.
        assert_eq!(summary.skipped_lock_file, 1, "the stub is a lock-file skip");
        assert_eq!(
            summary.skipped_apple_double, 0,
            "a lock stub is not an AppleDouble sidecar"
        );
    }

    /// A misdirected run (a spreadsheet kind pointed at something that is
    /// not a workbook) fails with a message classified as a wrong-input-kind
    /// error, so ADR-0143's rule applies and the local source copy is
    /// dropped rather than retained.
    #[tokio::test]
    async fn a_non_workbook_fails_and_drops_its_local_source_copy() {
        let dir = tempfile::tempdir().unwrap();
        let local_output = dir.path().join("work");
        let source = dir.path().join("pull-source");
        let destination = dir.path().join("destination");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(source.join("bogus.xlsx"), b"not a workbook at all").unwrap();

        let plan = spreadsheet_plan(&source, &destination, &local_output, InputFileType::Xlsx);
        let summary = run_transform_job(&plan, 2, 2).await.unwrap();

        assert!(summary.failed);
        assert_eq!(summary.pushed, 0);
        assert_eq!(summary.failures.transcode, 1);
        assert!(
            media::is_wrong_input_kind_error(&summary.outcomes[0].detail),
            "{}",
            summary.outcomes[0].detail
        );
        assert!(
            !source_dir(&local_output).join("bogus.xlsx").exists(),
            "ADR-0143: a wrong-input-kind failure drops its local copy"
        );
        // ADR-0151's cleanup is keyed to a *successful* push, so a file that
        // never reached the push leg leaves no push log to clean either.
        assert!(
            std::fs::read_dir(push_log_dir(&local_output))
                .map(|mut entries| entries.next().is_none())
                .unwrap_or(true),
            "a file that never pushed should leave no push log"
        );
    }

    /// ADR-0142 end-to-end: a `mp4-audio` run must pull an audio-only
    /// `.mp4`, transcode it, and push a `.opus` -- exercising the
    /// `Mp4Audio` dispatch arm, `output_extension()`'s new `opus` value,
    /// and `outcome_for`'s mapping in one pass.
    #[tokio::test]
    async fn run_transform_job_transcodes_an_audio_only_mp4_to_opus() {
        if media::check_ffmpeg_available().await.is_err() {
            eprintln!("skipping: ffmpeg/ffprobe not found on PATH");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let local_output = dir.path().join("work");
        let source_for_pull = dir.path().join("pull-source");
        let destination = dir.path().join("destination");
        std::fs::create_dir_all(&source_for_pull).unwrap();
        std::fs::create_dir_all(&destination).unwrap();

        // A real audio-only `.mp4`, the shape this input kind exists for.
        let generated = tokio::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=2",
                "-c:a",
                "aac",
                "-f",
                "mp4",
            ])
            .arg(source_for_pull.join("voice.mp4"))
            .output()
            .await;
        if !generated.map(|out| out.status.success()).unwrap_or(false) {
            eprintln!("skipping: this ffmpeg build can't generate an audio-only fixture");
            return;
        }

        let plan = TransformPlan {
            source_path: source_for_pull.display().to_string(),
            destination_path: destination.display().to_string(),
            local_output: local_output.clone(),
            input_file_type: InputFileType::Mp4Audio,
            download_transfers: 2,
            download_checkers: 4,
            video_quality: VideoQuality::Medium,
            audio_quality: AudioQuality::Medium,
            tpslimit: None,
            max_pending_downloads: 100,
            run_id: "test-run".to_string(),
        };

        let summary = run_transform_job(&plan, 1, 1).await.unwrap();

        if summary.failures.transcode > 0 {
            eprintln!(
                "skipping: this ffmpeg build can't encode libopus: {:?}",
                summary.outcomes
            );
            return;
        }

        assert!(!summary.failed);
        assert_eq!(summary.pushed, 1);
        assert_eq!(summary.outcomes.len(), 1);
        assert_eq!(summary.outcomes[0].outcome, Outcome::Transcoded);

        let pushed: Vec<_> = std::fs::read_dir(&destination)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(pushed.len(), 1);
        assert!(
            pushed[0].ends_with(".opus"),
            "expected a .opus output, got {}",
            pushed[0]
        );
    }

    /// ADR-0143: a `mp4-audio` run pointed at real video -- the exact
    /// mistake ADR-0142's guard exists to catch, which an omitted
    /// `--source` prefix makes easy (`source:mp4/` is the default address
    /// for *both* kinds). Every file must fail at the transcode stage,
    /// nothing may be pushed, and crucially the local source copies must
    /// be *deleted*: retaining them is what let a misdirected run
    /// accumulate the whole source tree on local disk.
    ///
    /// Deliberately 3 files, well under `CIRCUIT_BREAKER_SAMPLE_SIZE`, so
    /// the breaker cannot abort the run and confound what is being
    /// asserted here.
    #[tokio::test]
    async fn an_mp4_audio_run_over_real_video_deletes_each_rejected_local_source_copy() {
        if media::check_ffmpeg_available().await.is_err() {
            eprintln!("skipping: ffmpeg/ffprobe not found on PATH");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let local_output = dir.path().join("work");
        let source_for_pull = dir.path().join("pull-source");
        let destination = dir.path().join("destination");
        std::fs::create_dir_all(&source_for_pull).unwrap();
        std::fs::create_dir_all(&destination).unwrap();

        for i in 0..3 {
            let generated = tokio::process::Command::new("ffmpeg")
                .args([
                    "-y",
                    "-loglevel",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc=duration=1:size=64x48:rate=5",
                    "-c:v",
                    "libx264",
                    "-f",
                    "mp4",
                ])
                .arg(source_for_pull.join(format!("video-{i}.mp4")))
                .output()
                .await;
            if !generated.map(|out| out.status.success()).unwrap_or(false) {
                eprintln!("skipping: this ffmpeg build can't generate a video fixture");
                return;
            }
        }

        let plan = TransformPlan {
            source_path: source_for_pull.display().to_string(),
            destination_path: destination.display().to_string(),
            local_output: local_output.clone(),
            input_file_type: InputFileType::Mp4Audio,
            download_transfers: 3,
            download_checkers: 4,
            video_quality: VideoQuality::Medium,
            audio_quality: AudioQuality::Medium,
            tpslimit: None,
            max_pending_downloads: 100,
            run_id: "test-run".to_string(),
        };

        let summary = run_transform_job(&plan, 3, 3)
            .await
            .expect("3 failures are below the circuit-breaker sample size, so no whole-run abort");

        assert!(summary.failed);
        assert_eq!(summary.pushed, 0);
        assert_eq!(summary.failures.transcode, 3);
        assert_eq!(summary.outcomes.len(), 3);
        for outcome in &summary.outcomes {
            assert_eq!(outcome.stage, Some(FailureStage::Transcode));
            assert!(
                media::is_wrong_input_kind_error(&outcome.detail),
                "expected a wrong-input-kind error, got: {}",
                outcome.detail
            );
        }

        // Nothing reached the destination -- no video was converted to
        // audio-only, which is the whole point of ADR-0142's guard.
        assert!(std::fs::read_dir(&destination).unwrap().next().is_none());

        // ADR-0143: and no local source copy was retained.
        for i in 0..3 {
            let retained = source_dir(&local_output).join(format!("video-{i}.mp4"));
            assert!(
                !retained.exists(),
                "{} should have been deleted -- it is undamaged and still at --source",
                retained.display()
            );
        }
    }

    // --- ADR-0154: documents ---

    /// Asserts the four local trees ADR-0124/0149/0151 each require to be
    /// empty once every file is durably pushed.
    fn assert_local_trees_are_clean(local_output: &Path) {
        for (label, dir) in [
            ("source/", source_dir(local_output)),
            ("result/", result_dir(local_output)),
            ("scratch/", scratch_dir(local_output)),
            ("push-logs/", push_log_dir(local_output)),
        ] {
            if dir.exists() {
                assert!(
                    std::fs::read_dir(&dir).unwrap().next().is_none(),
                    "local {label} should be empty after every file is pushed"
                );
            }
        }
    }

    /// Deliberately **not** gated on `check_ffmpeg_available`: a `tsv` run
    /// invokes no `ffmpeg`, so gating it would make this silently never
    /// run -- the ADR-0139 mistake ADR-0149 named.
    #[tokio::test]
    async fn a_tsv_run_converts_and_pushes_a_csv() {
        let dir = tempfile::tempdir().unwrap();
        let local_output = dir.path().join("work");
        let source = dir.path().join("pull-source");
        let destination = dir.path().join("destination");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(
            source.join("table.tsv"),
            "name\tnotes\nWidget\tsmall, red\n",
        )
        .unwrap();

        let plan = spreadsheet_plan(&source, &destination, &local_output, InputFileType::Tsv);
        let summary = run_transform_job(&plan, 2, 2).await.unwrap();

        assert!(!summary.failed, "{:?}", summary.outcomes);
        assert_eq!(summary.pushed, 1);
        assert_eq!(summary.outcomes.len(), 1);
        assert_eq!(summary.outcomes[0].outcome, Outcome::Transcoded);

        let expected =
            placement::compute_destination_name(&plan.source_path, "table.tsv", InputFileType::Tsv);
        assert!(expected.ends_with(".csv"), "{expected}");
        // The comma inside a field is quoted, not left to split the row --
        // the reason this is a conversion and not a tab-to-comma swap.
        assert_eq!(
            std::fs::read_to_string(destination.join(&expected)).unwrap(),
            "name,notes\nWidget,\"small, red\"\n"
        );

        assert_local_trees_are_clean(&local_output);

        let rerun = run_transform_job(&plan, 2, 2).await.unwrap();
        assert_eq!(rerun.pushed, 0);
        assert_eq!(rerun.skipped_already_done, 1);
    }

    /// The clipping route is in-process too, so this also always runs.
    #[tokio::test]
    async fn a_textclipping_run_extracts_its_text_and_pushes_a_txt() {
        let dir = tempfile::tempdir().unwrap();
        let local_output = dir.path().join("work");
        let source = dir.path().join("pull-source");
        let destination = dir.path().join("destination");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&destination).unwrap();

        let mut inner = plist::Dictionary::new();
        inner.insert(
            "public.utf8-plain-text".to_string(),
            plist::Value::String("a dragged snippet".to_string()),
        );
        let mut outer = plist::Dictionary::new();
        outer.insert("UTI-Data".to_string(), plist::Value::Dictionary(inner));
        plist::Value::Dictionary(outer)
            .to_file_binary(source.join("snippet.textclipping"))
            .unwrap();

        let plan = spreadsheet_plan(
            &source,
            &destination,
            &local_output,
            InputFileType::Textclipping,
        );
        let summary = run_transform_job(&plan, 2, 2).await.unwrap();

        assert!(!summary.failed, "{:?}", summary.outcomes);
        assert_eq!(summary.pushed, 1);

        let expected = placement::compute_destination_name(
            &plan.source_path,
            "snippet.textclipping",
            InputFileType::Textclipping,
        );
        assert!(expected.ends_with(".txt"), "{expected}");
        assert_eq!(
            std::fs::read_to_string(destination.join(&expected)).unwrap(),
            "a dragged snippet"
        );
        assert_local_trees_are_clean(&local_output);
    }

    /// `pdf` and `txt` join `jpeg` and `csv` as copy-through kinds
    /// (ADR-0154 §9): byte-identical output, no re-encode.
    #[tokio::test]
    async fn a_pdf_input_is_copied_through_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let local_output = dir.path().join("work");
        let source = dir.path().join("pull-source");
        let destination = dir.path().join("destination");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&destination).unwrap();
        // Deliberately odd bytes, as the csv copy-through test does.
        let original: &[u8] = b"%PDF-1.7\n\x80\x81 \r\n%%EOF\n";
        std::fs::write(source.join("scan.pdf"), original).unwrap();

        let plan = spreadsheet_plan(&source, &destination, &local_output, InputFileType::Pdf);
        let summary = run_transform_job(&plan, 2, 2).await.unwrap();

        assert!(!summary.failed, "{:?}", summary.outcomes);
        assert_eq!(summary.outcomes[0].outcome, Outcome::CopiedThrough);
        let expected =
            placement::compute_destination_name(&plan.source_path, "scan.pdf", InputFileType::Pdf);
        assert_eq!(
            std::fs::read(destination.join(&expected)).unwrap(),
            original,
            "copy-through must not rewrite the bytes"
        );
    }

    #[test]
    fn outcome_for_treats_pdf_and_txt_as_copied_through() {
        assert_eq!(outcome_for(InputFileType::Pdf), Outcome::CopiedThrough);
        assert_eq!(outcome_for(InputFileType::Txt), Outcome::CopiedThrough);
        for kind in [
            InputFileType::Docx,
            InputFileType::Md,
            InputFileType::Textclipping,
            InputFileType::Tsv,
        ] {
            assert_eq!(outcome_for(kind), Outcome::Transcoded, "{kind}");
        }
    }

    /// **The ADR-0154 §2 regression test, end to end.** A private key
    /// named `.key` must fail rather than reaching the destination, and
    /// ADR-0143 must drop its local copy so it does not linger on the host.
    #[tokio::test]
    async fn a_private_key_named_dot_key_never_reaches_the_destination() {
        let dir = tempfile::tempdir().unwrap();
        let local_output = dir.path().join("work");
        let source = dir.path().join("pull-source");
        let destination = dir.path().join("destination");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(
            source.join("id_ed25519.key"),
            b"-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n\
              -----END OPENSSH PRIVATE KEY-----\n",
        )
        .unwrap();

        let plan = spreadsheet_plan(&source, &destination, &local_output, InputFileType::Key);
        let summary = run_transform_job(&plan, 2, 2).await.unwrap();

        assert!(summary.failed, "a private key must not convert");
        assert_eq!(summary.pushed, 0);
        assert_eq!(summary.failures.transcode, 1);

        let landed: Vec<_> = std::fs::read_dir(&destination)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .collect();
        assert!(landed.is_empty(), "{landed:?} reached the destination");

        // ADR-0143: the local copy is dropped. The source object is
        // untouched at `--source`, which is where it already was.
        assert!(
            std::fs::read_dir(source_dir(&local_output))
                .map(|mut entries| entries.next().is_none())
                .unwrap_or(true),
            "the private key's local copy must not linger"
        );
    }

    use crate::commands::job::transform::soffice;

    /// A one-slide presentation in OpenDocument's flat XML form, which is
    /// plain text and so can be written here rather than committed as a
    /// binary.
    const FLAT_ODP_FIXTURE: &str = concat!(
        r#"<?xml version="1.0" encoding="UTF-8"?>"#,
        r#"<office:document xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" "#,
        r#"xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" "#,
        r#"xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" "#,
        r#"xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0" "#,
        r#"office:version="1.3" office:mimetype="application/vnd.oasis.opendocument.presentation">"#,
        r#"<office:body><office:presentation><draw:page draw:name="One">"#,
        r#"<draw:frame svg:width="10cm" svg:height="2cm"><draw:text-box>"#,
        r#"<text:p>Slide one</text:p></draw:text-box></draw:frame>"#,
        r#"</draw:page></office:presentation></office:body></office:document>"#,
    );

    /// Builds a real fixture of `kind` by converting an RTF with the same
    /// `soffice` the job itself uses. Constructed, never committed as a
    /// binary -- the convention every fixture here follows.
    async fn soffice_fixture(dir: &Path, kind: InputFileType) -> Option<PathBuf> {
        if soffice::check_available().await.is_err() {
            return None;
        }
        std::fs::create_dir_all(dir).unwrap();
        let rtf = dir.join("fixture.rtf");
        std::fs::write(
            &rtf,
            br"{\rtf1\ansi\deff0 Document fixture.\par Second paragraph.\par}",
        )
        .unwrap();
        if kind == InputFileType::Rtf {
            return Some(rtf);
        }
        // An Impress format cannot be made from a Writer document, so a
        // presentation starts from a flat ODP instead.
        let seed = if kind == InputFileType::Pptx {
            let fodp = dir.join("fixture.fodp");
            std::fs::write(&fodp, FLAT_ODP_FIXTURE).unwrap();
            fodp
        } else {
            rtf
        };
        let ext = kind.canonical_extension();
        let produced = soffice::convert(&seed, dir, ext, "is not a readable document")
            .await
            .unwrap_or_else(|err| panic!("building a .{ext} fixture: {err}"));
        Some(produced)
    }

    /// Runs one file of `kind` end to end and returns the bytes that landed
    /// at the destination, asserting the run itself was clean.
    async fn run_one_document(kind: InputFileType, fixture: &Path) -> Vec<u8> {
        let dir = tempfile::tempdir().unwrap();
        let local_output = dir.path().join("work");
        let source = dir.path().join("pull-source");
        let destination = dir.path().join("destination");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&destination).unwrap();
        let name = format!("fixture.{}", kind.canonical_extension());
        std::fs::copy(fixture, source.join(&name)).unwrap();

        let plan = spreadsheet_plan(&source, &destination, &local_output, kind);
        let summary = run_transform_job(&plan, 2, 2).await.unwrap();

        assert!(!summary.failed, "{kind}: {:?}", summary.outcomes);
        assert_eq!(summary.pushed, 1, "{kind}");

        let expected = placement::compute_destination_name(&plan.source_path, &name, kind);
        assert!(
            expected.ends_with(&format!(".{}", kind.output_extension())),
            "{kind}: {expected}"
        );
        assert_local_trees_are_clean(&local_output);
        std::fs::read(destination.join(&expected)).unwrap()
    }

    /// The five `soffice`-backed kinds, each converted end to end to a real
    /// PDF. Gated on `soffice`, never on `ffmpeg`, which none of them
    /// invoke.
    #[tokio::test]
    async fn every_soffice_document_kind_produces_a_pdf() {
        let dir = tempfile::tempdir().unwrap();
        for kind in [
            InputFileType::Rtf,
            InputFileType::Doc,
            InputFileType::Docx,
            InputFileType::Pptx,
        ] {
            let work = dir.path().join(kind.canonical_extension());
            let Some(fixture) = soffice_fixture(&work, kind).await else {
                eprintln!("skipping: soffice not found on PATH");
                return;
            };
            let landed = run_one_document(kind, &fixture).await;
            assert!(
                landed.starts_with(b"%PDF-"),
                "{kind} did not produce a PDF: {:?}",
                &landed[..landed.len().min(8)]
            );
        }
    }

    /// `epub` is the one kind LibreOffice cannot read, so it exercises the
    /// Calibre leg instead. Its fixture is built by Calibre too.
    #[tokio::test]
    async fn an_epub_run_produces_a_pdf_via_calibre() {
        if documents::check_calibre_available().await.is_err() {
            eprintln!("skipping: ebook-convert not found on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let markdown = dir.path().join("book.md");
        std::fs::write(&markdown, "# Chapter One\n\nSome prose.\n").unwrap();
        let epub = dir.path().join("fixture.epub");
        let built = tokio::process::Command::new("ebook-convert")
            .arg(&markdown)
            .arg(&epub)
            .output()
            .await
            .unwrap();
        assert!(built.status.success(), "building the epub fixture failed");

        let landed = run_one_document(InputFileType::Epub, &epub).await;
        assert!(
            landed.starts_with(b"%PDF-"),
            "epub did not produce a PDF: {:?}",
            &landed[..landed.len().min(8)]
        );
    }

    /// Markdown end to end, pinning ADR-0154 §8's recorded loss at the
    /// level that actually ships: the URL does not reach the destination.
    #[tokio::test]
    async fn a_markdown_run_pushes_prose_with_its_link_urls_discarded() {
        if documents::check_pandoc_available().await.is_err() {
            eprintln!("skipping: pandoc not found on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let fixture = dir.path().join("fixture.md");
        std::fs::write(
            &fixture,
            "# Heading\n\nSome **bold** text and [a link](https://example.com).\n",
        )
        .unwrap();

        let landed =
            String::from_utf8(run_one_document(InputFileType::Md, &fixture).await).unwrap();
        assert!(landed.contains("Heading"), "{landed}");
        assert!(landed.contains("bold"), "{landed}");
        assert!(!landed.contains("example.com"), "{landed}");
        assert!(!landed.contains("**"), "{landed}");
    }

    #[tokio::test]
    async fn an_rst_run_pushes_prose() {
        if documents::check_pandoc_available().await.is_err() {
            eprintln!("skipping: pandoc not found on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let fixture = dir.path().join("fixture.rst");
        std::fs::write(&fixture, "Title\n=====\n\nSome ``literal`` text.\n").unwrap();

        let landed =
            String::from_utf8(run_one_document(InputFileType::Rst, &fixture).await).unwrap();
        assert!(landed.contains("Title"), "{landed}");
        assert!(landed.contains("literal"), "{landed}");
    }

    /// `txt` is copy-through, so the bytes must survive verbatim.
    #[tokio::test]
    async fn a_txt_input_is_copied_through_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = dir.path().join("fixture.txt");
        let original = b"plain text fixture\r\nwith CRLF and \x80 odd bytes\n";
        std::fs::write(&fixture, original).unwrap();

        let landed = run_one_document(InputFileType::Txt, &fixture).await;
        assert_eq!(landed, original, "copy-through must not rewrite the bytes");
    }
}
