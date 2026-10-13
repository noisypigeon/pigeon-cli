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
//! left in place on disk for diagnosis.

use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinSet;
use tracing::Instrument;

use super::format::{InputFileType, VideoQuality};
use super::manifest::{self, PendingFile};
use super::{destination, media, placement, push};
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

/// One file's pipeline result: its placed local path, or the stage that
/// failed plus that stage's error (ADR-0130).
type ProcessResult = Result<PathBuf, (FailureStage, String)>;

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
    pub destination_filename: Option<String>,
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
    /// `rclone`'s own aggregate transfer-error count for the pull phase,
    /// retained as a cross-check against the reconciled per-file `Pull`
    /// failures rather than being dropped on the floor (ADR-0130).
    pub pull_errors: u64,
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

fn push_log_dir(local_output: &Path) -> PathBuf {
    staging_dir(local_output).join("push-logs")
}

/// One file's full pipeline: transcode (png/heic, retried) or copy-through
/// (jpeg) into a scratch path, place it under `result_dir` at its
/// deterministic, collision-free name, then push that single file. Returns
/// the final local path on success so the caller can report it --
/// checkpoint writes deliberately happen in the single-threaded dispatch
/// loop below, not here, so concurrent tasks never need to coordinate a
/// shared file write.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(
    skip_all,
    fields(relative_path = %pending_file.relative_path),
    level = "debug"
)]
async fn process_one(
    pending_file: &PendingFile,
    input_file_type: InputFileType,
    video_quality: VideoQuality,
    transcode_threads: usize,
    source_path: &str,
    destination_path: &str,
    result_dir: &Path,
    scratch_dir: &Path,
    push_log_dir: &Path,
    push_semaphore: Arc<Semaphore>,
) -> ProcessResult {
    let output_extension = input_file_type.output_extension();
    let destination_name = placement::compute_destination_name(
        source_path,
        &pending_file.relative_path,
        output_extension,
    );

    // ADR-0127: a file already sitting in `result_dir` under its
    // deterministic name survived a prior crash *after* a successful,
    // verified transcode (`placement::place_one` only ever renames a
    // scratch file into `result_dir` once `ffmpeg` has exited 0 and passed
    // its own dimension/duration guard) -- it is never a partial write.
    // Skip straight to push instead of re-transcoding and then hitting
    // `place_one`'s collision guard, which exists for a genuinely different
    // problem (two distinct source files hashing to the same name), not
    // this job's own legitimate resume.
    let existing_result_path = result_dir.join(&destination_name);
    let final_path = if existing_result_path.exists() {
        existing_result_path
    } else {
        let scratch_path = scratch_dir.join(format!("{destination_name}.scratch"));
        std::fs::create_dir_all(scratch_dir).map_err(|err| {
            (
                FailureStage::Transcode,
                format!("failed to create {}: {err}", scratch_dir.display()),
            )
        })?;

        // ADR-0130: this is the run's real disk high-water mark -- a 4K
        // video's scratch output is multi-GB, and ADR-0124's post-push GC
        // only bounds steady-state usage, not this transient peak (and is
        // best-effort, so repeated removal failures accumulate sources with
        // nothing watching free space). Fails just this one file, matching
        // `check_disk_space`'s own documented contract.
        crate::commands::job::local::check_disk_space(scratch_dir, 0)
            .map_err(|err| (FailureStage::Transcode, err))?;

        let transcode_result = match input_file_type {
            InputFileType::Jpeg => media::copy_through(&pending_file.absolute_path, &scratch_path),
            InputFileType::Png | InputFileType::Heic => {
                retry_transcode_unless_fatal(
                    TRANSCODE_RETRIES,
                    TRANSCODE_RETRY_BACKOFF,
                    &pending_file.relative_path,
                    || media::transcode_to_jpg(&pending_file.absolute_path, &scratch_path),
                )
                .await
            }
            InputFileType::Mov | InputFileType::M4v | InputFileType::Mp4 => {
                retry_transcode_unless_fatal(
                    TRANSCODE_RETRIES,
                    TRANSCODE_RETRY_BACKOFF,
                    &pending_file.relative_path,
                    || {
                        media::transcode_video(
                            &pending_file.absolute_path,
                            &scratch_path,
                            video_quality,
                            transcode_threads,
                        )
                    },
                )
                .await
            }
            InputFileType::Mp4Audio => {
                retry_transcode_unless_fatal(
                    TRANSCODE_RETRIES,
                    TRANSCODE_RETRY_BACKOFF,
                    &pending_file.relative_path,
                    || media::transcode_audio(&pending_file.absolute_path, &scratch_path),
                )
                .await
            }
        };
        crate::observability::metrics::record_phase_count(
            "transform",
            "transcode",
            if transcode_result.is_ok() {
                outcome_for(input_file_type).as_str()
            } else {
                Outcome::Failed.as_str()
            },
            1,
            None,
        );
        transcode_result.map_err(|err| (FailureStage::Transcode, err))?;

        placement::place_one(
            result_dir,
            &scratch_path,
            source_path,
            &pending_file.relative_path,
            output_extension,
        )
        .map_err(|err| (FailureStage::Place, err))?
    };

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

    let _permit = push_semaphore.acquire_owned().await.map_err(|err| {
        (
            FailureStage::Push,
            format!("push semaphore closed unexpectedly: {err}"),
        )
    })?;
    push::push_one(
        &final_path,
        destination_path,
        &destination_filename,
        push_log_dir,
    )
    .await
    .map_err(|err| (FailureStage::Push, err))?;

    // This file's bytes are now durably present at `destination_path` --
    // the local source and result copies no longer serve any purpose
    // (ADR-0124). Deleting them here, inside this file's own concurrent
    // task, is safe without any synchronization: each task only ever
    // touches its own two distinct paths.
    remove_local_copy(&pending_file.absolute_path);
    remove_local_copy(&final_path);

    Ok(final_path)
}

/// Best-effort local cleanup once a file is confirmed durably pushed
/// (ADR-0124). `transform` has no `--upload-only`/local-state-dependent
/// resume mechanism to preserve by keeping these files around -- unlike
/// `deduplicate`, which deliberately retains its own `result/` tree for
/// exactly that purpose -- every phase here is already
/// independently rerun-safe via the remote destination listing, not local
/// state. A removal failure (e.g. a permissions issue) is logged and
/// otherwise ignored: it must never turn an already-successful push into a
/// failed `Outcome`.
fn remove_local_copy(path: &Path) {
    if let Err(err) = std::fs::remove_file(path) {
        tracing::warn!(
            path = %path.display(),
            error = %err,
            "transform: failed to remove local file after successful push"
        );
    }
}

fn outcome_for(input_file_type: InputFileType) -> Outcome {
    match input_file_type {
        InputFileType::Jpeg => Outcome::CopiedThrough,
        InputFileType::Png
        | InputFileType::Heic
        | InputFileType::Mov
        | InputFileType::M4v
        | InputFileType::Mp4
        | InputFileType::Mp4Audio => Outcome::Transcoded,
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
    output_extension: &str,
    existing_destination_filenames: &HashSet<String>,
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
    let destination_name =
        placement::compute_destination_name(source_path, &relative_path, output_extension);
    if existing_destination_filenames.contains(&destination_name) {
        return EnqueueOutcome::AlreadyDone;
    }
    if !dispatched.insert(relative_path.clone()) {
        return EnqueueOutcome::AlreadyDispatched;
    }
    let absolute_path = source_dir.join(&relative_path);
    queue.push_back(PendingFile {
        relative_path,
        absolute_path,
    });
    EnqueueOutcome::Queued
}

/// Why `enqueue` did or didn't queue a path. Only `SkippedAppleDouble` is
/// counted by the caller (ADR-0130); the rest are ordinary, expected
/// outcomes of the two overlapping dispatch sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnqueueOutcome {
    Queued,
    AlreadyDone,
    AlreadyDispatched,
    SkippedAppleDouble,
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
    output_extension: &str,
    existing_destination_filenames: &HashSet<String>,
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
        let destination_name =
            placement::compute_destination_name(source_path, &relative_path, output_extension);
        if existing_destination_filenames.contains(&destination_name) {
            selection.skipped_already_done += 1;
            continue;
        }
        selection.to_pull.push(relative_path);
    }
    selection
}

/// What `files_needing_pull` decided, including the two counts it used to
/// discard silently (ADR-0130). `to_pull` doubles as the expected set the
/// pull phase is reconciled against once the pipeline drains.
#[derive(Debug, Default)]
struct PullSelection {
    to_pull: Vec<String>,
    skipped_already_done: u64,
    skipped_apple_double: u64,
}

/// Writes `relative_paths` newline-delimited under `staging_dir`, for
/// `rclone_transfer::run_rclone_copy`'s `files_from` parameter (ADR-0124).
fn write_files_from(staging_dir: &Path, relative_paths: &[String]) -> Result<PathBuf, String> {
    std::fs::create_dir_all(staging_dir)
        .map_err(|err| format!("failed to create {}: {err}", staging_dir.display()))?;
    let path = staging_dir.join("pull-files-from.txt");
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
    if let Err(err) = std::fs::remove_dir_all(&scratch_dir)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(
            path = %scratch_dir.display(),
            error = %err,
            "transform: failed to clear stale scratch directory from a prior run"
        );
    }

    let output_extension = plan.input_file_type.output_extension();

    // The authoritative "already done" source (ADR-0120) -- fetched once,
    // before the pull subprocess exists, since `--destination-path` is the
    // one thing that actually persists across this job's ephemeral VMs.
    let existing_destination_filenames =
        destination::list_existing_filenames(&plan.destination_path).await?;

    // Initial directory scan, taken once, before the pull subprocess exists
    // -- picks up anything already on disk this run before the live tail
    // would otherwise report it.
    let already_on_disk = manifest::gather_pending(&source_dir, plan.input_file_type)?;

    let mut dispatched: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<PendingFile> = VecDeque::new();
    let mut skipped_apple_double: u64 = 0;
    for pending_file in already_on_disk {
        if enqueue(
            pending_file.relative_path,
            &source_dir,
            &plan.source_path,
            output_extension,
            &existing_destination_filenames,
            &mut dispatched,
            &mut queue,
        ) == EnqueueOutcome::SkippedAppleDouble
        {
            skipped_apple_double += 1;
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
        output_extension,
        &existing_destination_filenames,
    );
    let files_to_pull = selection.to_pull;
    let selection_skipped_already_done = selection.skipped_already_done;
    // The initial on-disk scan above may have found its own AppleDouble
    // stubs; both choke points feed the same counter.
    skipped_apple_double += selection.skipped_apple_double;

    // ADR-0130: fail fast before a single byte is downloaded. Nothing has
    // been pulled yet at this point, so there is nothing to salvage -- unlike
    // the per-file check in `process_one`, which fails only its own file.
    if !files_to_pull.is_empty() {
        std::fs::create_dir_all(&source_dir)
            .map_err(|err| format!("failed to create {}: {err}", source_dir.display()))?;
        crate::commands::job::local::check_disk_space(&source_dir, 0)?;
    }

    tracing::info!(
        pigeon_job = "transform",
        files_to_pull = files_to_pull.len(),
        skipped_already_done = selection.skipped_already_done,
        skipped_apple_double,
        queued_from_local_scan = queue.len(),
        "transform: pull phase starting"
    );

    let (copied_tx, mut copied_rx) = mpsc::unbounded_channel();
    let (pull_pid_tx, mut pull_pid_rx) = tokio::sync::mpsc::channel::<u32>(1);
    let pull_log_path = plan
        .local_output
        .join(format!("rclone-pull-{}.jsonl", plan.run_id));
    let mut pull_handle = if files_to_pull.is_empty() {
        // Nothing to pull -- every matching source file is already done.
        // Skip spawning `rclone` entirely rather than running it against an
        // empty files-from list.
        tokio::spawn(async move {
            drop(copied_tx);
            Ok::<_, String>(crate::core::rclone::RcloneLogSummary::default())
        })
    } else {
        let files_from_path = write_files_from(&staging_dir(&plan.local_output), &files_to_pull)?;
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

    loop {
        while !circuit_breaker_tripped && in_flight.len() < concurrency.max(1) {
            let Some(pending_file) = queue.pop_front() else {
                break;
            };
            let relative_path = pending_file.relative_path.clone();
            let source_path = plan.source_path.clone();
            let destination_path = plan.destination_path.clone();
            let result_dir = result_dir.clone();
            let scratch_dir = scratch_dir.clone();
            let push_log_dir = push_log_dir.clone();
            let push_semaphore = push_semaphore.clone();
            let input_file_type = plan.input_file_type;
            let video_quality = plan.video_quality;
            in_flight.spawn(
                async move {
                    let outcome = process_one(
                        &pending_file,
                        input_file_type,
                        video_quality,
                        transcode_threads,
                        &source_path,
                        &destination_path,
                        &result_dir,
                        &scratch_dir,
                        &push_log_dir,
                        push_semaphore,
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
                    // systemic ffmpeg-capability gap this breaker exists for.
                    if let Err((stage, err)) = &result
                        && *stage == FailureStage::Transcode
                        && media::is_non_retryable_transcode_error(err)
                    {
                        circuit_breaker_matches += 1;
                        circuit_breaker_example.get_or_insert_with(|| err.clone());
                    }
                    if circuit_breaker_samples == CIRCUIT_BREAKER_SAMPLE_SIZE
                        && circuit_breaker_matches == CIRCUIT_BREAKER_SAMPLE_SIZE
                    {
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
                    Ok(final_path) => {
                        pushed += 1;
                        outcomes.push(FileOutcome {
                            relative_path,
                            destination_filename: final_path
                                .file_name()
                                .and_then(|name| name.to_str())
                                .map(str::to_string),
                            outcome: outcome_for(plan.input_file_type),
                            stage: None,
                            detail: String::new(),
                        });
                    }
                    Err((stage, err)) => {
                        failures.record(stage);
                        outcomes.push(FileOutcome {
                            relative_path,
                            destination_filename: None,
                            outcome: Outcome::Failed,
                            stage: Some(stage),
                            detail: err,
                        });
                    }
                }

                // ADR-0129: dynamic backpressure -- check backlog and adjust pull pause state.
                if let Some(pid) = pull_pid {
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
                        if !circuit_breaker_tripped
                            && enqueue(
                                relative_path,
                                &source_dir,
                                &plan.source_path,
                                output_extension,
                                &existing_destination_filenames,
                                &mut dispatched,
                                &mut queue,
                            ) == EnqueueOutcome::SkippedAppleDouble
                        {
                            skipped_apple_double += 1;
                        }
                    }
                    None => copied_rx_closed = true,
                }
            }
            joined = &mut pull_handle, if !pull_finished => {
                let result = joined.map_err(|err| format!("rclone pull task join failed: {err}"))?;
                pull_finished = true;
                // Drain anything buffered between the last select poll and
                // the subprocess's exit -- `try_recv` is non-blocking, so
                // this can't stall the loop.
                while let Ok(relative_path) = copied_rx.try_recv() {
                    if !circuit_breaker_tripped
                        && enqueue(
                            relative_path,
                            &source_dir,
                            &plan.source_path,
                            output_extension,
                            &existing_destination_filenames,
                            &mut dispatched,
                            &mut queue,
                        ) == EnqueueOutcome::SkippedAppleDouble
                    {
                        skipped_apple_double += 1;
                    }
                }
                pull_result = Some(result);
            }
        }
    }

    let pull_summary = pull_result.expect("loop only exits after pull_finished is set")?;

    if circuit_breaker_tripped {
        let example = circuit_breaker_example
            .as_deref()
            .unwrap_or("<no example captured>");
        return Err(format!(
            "transform circuit breaker tripped: all {CIRCUIT_BREAKER_SAMPLE_SIZE} of the first \
             {CIRCUIT_BREAKER_SAMPLE_SIZE} completed files failed transcoding with the same \
             deterministic error signature. This looks like a systemic environment/capability \
             gap (e.g. the ffmpeg build on this job's host lacking support for this input's \
             format or a feature it uses), not isolated bad source files -- the run is being \
             aborted instead of grinding through every remaining file with a \
             guaranteed-identical failure. Check the deployed ffmpeg version and its \
             decoder/demuxer support for this input type before rerunning. Example error: {example}"
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
    for relative_path in &files_to_pull {
        if dispatched.contains(relative_path) {
            continue;
        }
        failures.record(FailureStage::Pull);
        outcomes.push(FileOutcome {
            relative_path: relative_path.clone(),
            destination_filename: None,
            outcome: Outcome::Failed,
            stage: Some(FailureStage::Pull),
            detail: "file was in this run's pull set but rclone never reported it as copied, \
                     so it never entered the transcode pipeline"
                .to_string(),
        });
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
    if pull_summary.errors != failures.pull {
        tracing::warn!(
            pigeon_job = "transform",
            rclone_errors = pull_summary.errors,
            undelivered_files = failures.pull,
            "transform: rclone's pull error count disagrees with the number of undelivered files"
        );
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
        pull_errors: pull_summary.errors,
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
        String::from("source_relative_path\toutcome\tstage\tdestination_filename\tdetail\n");
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
            outcome.destination_filename.as_deref().unwrap_or(""),
            outcome.detail,
        );
    }
    let _ = writeln!(
        body,
        "\n{transcoded} transcoded, {copied_through} copied through, {failed} failed."
    );
    let failures = summary.failures;
    let _ = writeln!(
        body,
        "failures by stage: {} pull, {} transcode, {} place, {} push.",
        failures.pull, failures.transcode, failures.place, failures.push
    );
    let _ = writeln!(
        body,
        "skipped: {} already done at the destination, {} AppleDouble sidecar(s).",
        summary.skipped_already_done, summary.skipped_apple_double
    );
    let _ = writeln!(
        body,
        "pull: {} transferred, {} rclone error(s) reported.",
        summary.pulled, summary.pull_errors
    );

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
            tpslimit: None,
            max_pending_downloads: 100,
            run_id: "test-run".to_string(),
        };

        let summary = run_transform_job(&plan, 2, 2).await.unwrap();

        assert!(!summary.failed);
        assert_eq!(summary.pushed, 1);
        assert!(
            destination
                .join(summary.outcomes[0].destination_filename.as_deref().unwrap())
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
        let destination_name = placement::compute_destination_name(&source_path, "a.heic", "jpg");
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
            placement::compute_destination_name(&source_path, "a.jpeg", "jpg");
        std::fs::write(destination.join(&destination_filename), b"already pushed").unwrap();

        let plan = TransformPlan {
            source_path,
            destination_path: destination.display().to_string(),
            local_output: local_output.clone(),
            input_file_type: InputFileType::Jpeg,
            download_transfers: 2,
            download_checkers: 4,
            video_quality: VideoQuality::Medium,
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
        let existing_destination_filenames = HashSet::new();
        let mut dispatched = HashSet::new();
        let mut queue = VecDeque::new();

        enqueue(
            "a.png".to_string(),
            &source_dir,
            "source:png/",
            "jpg",
            &existing_destination_filenames,
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
            "jpg",
            &existing_destination_filenames,
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
        let mut existing_destination_filenames = HashSet::new();
        existing_destination_filenames.insert(placement::compute_destination_name(
            source_path,
            "a.png",
            "jpg",
        ));
        let mut dispatched = HashSet::new();
        let mut queue = VecDeque::new();

        enqueue(
            "a.png".to_string(),
            &source_dir,
            source_path,
            "jpg",
            &existing_destination_filenames,
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
        let mut existing_destination_filenames = HashSet::new();
        existing_destination_filenames.insert(placement::compute_destination_name(
            source_path,
            "a.png",
            "jpg",
        ));

        let result = files_needing_pull(
            vec!["a.png".to_string(), "b.png".to_string()],
            source_path,
            "jpg",
            &existing_destination_filenames,
        );

        assert_eq!(result.to_pull, vec!["b.png".to_string()]);
        assert_eq!(result.skipped_already_done, 1);
        assert_eq!(result.skipped_apple_double, 0);
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
            "jpg",
            &HashSet::new(),
        );

        assert_eq!(selection.to_pull, vec!["a.png".to_string()]);
        assert_eq!(selection.skipped_apple_double, 2);
        assert_eq!(selection.skipped_already_done, 0);
    }

    /// The second AppleDouble choke point (ADR-0130): `enqueue` covers the
    /// initial on-disk scan and rclone's live tail, so a stub cannot reach
    /// `ffmpeg` by any dispatch route even if it bypassed the pull filter.
    #[test]
    fn enqueue_skips_an_apple_double_sidecar() {
        let mut dispatched = HashSet::new();
        let mut queue = VecDeque::new();

        let outcome = enqueue(
            "._a.png".to_string(),
            Path::new("/tmp/source"),
            "source:png/",
            "jpg",
            &HashSet::new(),
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
                "jpg",
                &HashSet::new(),
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
                "jpg",
                &HashSet::new(),
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
                    destination_filename: Some("a-abc123.jpg".to_string()),
                    outcome: Outcome::Transcoded,
                    stage: None,
                    detail: String::new(),
                },
                FileOutcome {
                    relative_path: "bad.heic".to_string(),
                    destination_filename: None,
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
            pull_errors: 0,
        };

        let path = write_report(dir.path(), &summary).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();

        assert!(contents.contains("a.png\ttranscoded\t\ta-abc123.jpg\t"));
        assert!(contents.contains("bad.heic\tfailed\ttranscode\t\tffmpeg: boom"));
        assert!(contents.contains("1 transcoded, 0 copied through, 1 failed."));
        // ADR-0130's added lines.
        assert!(contents.contains("failures by stage: 0 pull, 1 transcode, 0 place, 0 push."));
        assert!(contents.contains("skipped: 3 already done at the destination, 2 AppleDouble"));
        assert!(contents.contains("pull: 2 transferred, 0 rclone error(s) reported."));
        assert_eq!(path.file_name().unwrap(), "transform-report.txt");
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
}
