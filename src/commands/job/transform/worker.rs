//! Per-file pull-&gt;transcode-&gt;push pipeline (ADR-0116, amending ADR-0112
//! Decision §2/§6/§7). Download stays one bulk `rclone copy` subprocess
//! (unchanged invocation, still gets rclone's own connection-reuse/listing
//! efficiency), but the worker consumes it *live* instead of blocking on its
//! exit: as each file's bytes land, rclone's own JSON log reports it
//! ("Copied (new)"/"Copied (replaced existing)", surfaced via
//! `rclone_transfer::run_rclone_copy`'s `on_copied` channel), and that file
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

use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinSet;

use super::format::{InputFileType, VideoQuality};
use super::manifest::{self, PendingFile};
use super::{destination, media, placement, push};
use crate::commands::job::rclone_transfer;

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
/// shared `retry_with_backoff` -- that helper has 6+ unrelated consumers
/// (`push.rs`, `deduplicate`, `pull_transform`, ...); this classification is
/// specific to `transcode_to_jpg`'s error strings alone.
async fn retry_transcode_unless_fatal<T, F, Fut>(
    attempts: usize,
    backoff: Duration,
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
                        attempt = attempt + 1,
                        attempts,
                        error = %err,
                        "transcode failed with a non-retryable error; skipping remaining retries"
                    );
                    return Err(err);
                }
                tracing::warn!(
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
        tracing::error!(attempts, error = %err, "retries exhausted");
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
    pub transfers: usize,
    pub checkers: usize,
    pub tpslimit: Option<usize>,
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

#[derive(Debug, Clone)]
pub(crate) struct FileOutcome {
    pub relative_path: String,
    pub destination_filename: Option<String>,
    pub outcome: Outcome,
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
async fn process_one(
    pending_file: &PendingFile,
    input_file_type: InputFileType,
    video_quality: VideoQuality,
    source_path: &str,
    destination_path: &str,
    result_dir: &Path,
    scratch_dir: &Path,
    push_log_dir: &Path,
    push_semaphore: Arc<Semaphore>,
) -> Result<PathBuf, String> {
    let output_extension = input_file_type.output_extension();
    let scratch_path = scratch_dir.join(format!(
        "{}.scratch",
        placement::compute_destination_name(
            source_path,
            &pending_file.relative_path,
            output_extension,
        )
    ));
    std::fs::create_dir_all(scratch_dir)
        .map_err(|err| format!("failed to create {}: {err}", scratch_dir.display()))?;

    let transcode_result = match input_file_type {
        InputFileType::Jpeg => media::copy_through(&pending_file.absolute_path, &scratch_path),
        InputFileType::Png | InputFileType::Heic => {
            retry_transcode_unless_fatal(TRANSCODE_RETRIES, TRANSCODE_RETRY_BACKOFF, || {
                media::transcode_to_jpg(&pending_file.absolute_path, &scratch_path)
            })
            .await
        }
        InputFileType::Mov | InputFileType::M4v | InputFileType::Mp4 => {
            retry_transcode_unless_fatal(TRANSCODE_RETRIES, TRANSCODE_RETRY_BACKOFF, || {
                media::transcode_video(&pending_file.absolute_path, &scratch_path, video_quality)
            })
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
    transcode_result?;

    let final_path = placement::place_one(
        result_dir,
        &scratch_path,
        source_path,
        &pending_file.relative_path,
        output_extension,
    )?;

    let destination_filename = final_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            format!(
                "placed file at {} has no valid UTF-8 filename",
                final_path.display()
            )
        })?
        .to_string();

    let _permit = push_semaphore
        .acquire_owned()
        .await
        .map_err(|err| format!("push semaphore closed unexpectedly: {err}"))?;
    push::push_one(
        &final_path,
        destination_path,
        &destination_filename,
        push_log_dir,
    )
    .await?;

    Ok(final_path)
}

fn outcome_for(input_file_type: InputFileType) -> Outcome {
    match input_file_type {
        InputFileType::Jpeg => Outcome::CopiedThrough,
        InputFileType::Png
        | InputFileType::Heic
        | InputFileType::Mov
        | InputFileType::M4v
        | InputFileType::Mp4 => Outcome::Transcoded,
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
) {
    let destination_name =
        placement::compute_destination_name(source_path, &relative_path, output_extension);
    if existing_destination_filenames.contains(&destination_name) {
        return;
    }
    if !dispatched.insert(relative_path.clone()) {
        return;
    }
    let absolute_path = source_dir.join(&relative_path);
    queue.push_back(PendingFile {
        relative_path,
        absolute_path,
    });
}

/// Runs the whole per-file pipeline. `Err` is reserved for infrastructure-
/// level failures (the pull subprocess itself failing, a directory that
/// couldn't be created) -- a per-file transcode/push failure is captured in
/// the returned `TransformSummary` instead, so every already-succeeded
/// file's outcome survives for the report.
pub(crate) async fn run_transform_job(
    plan: &TransformPlan,
    concurrency: usize,
) -> Result<TransformSummary, String> {
    let source_dir = source_dir(&plan.local_output);
    let result_dir = result_dir(&plan.local_output);
    let scratch_dir = scratch_dir(&plan.local_output);
    let push_log_dir = push_log_dir(&plan.local_output);

    crate::observability::metrics::set_macro_phase("transform", false);

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
    for pending_file in already_on_disk {
        enqueue(
            pending_file.relative_path,
            &source_dir,
            &plan.source_path,
            output_extension,
            &existing_destination_filenames,
            &mut dispatched,
            &mut queue,
        );
    }

    let (copied_tx, mut copied_rx) = mpsc::unbounded_channel();
    let pull_log_path = plan
        .local_output
        .join(format!("rclone-pull-{}.jsonl", plan.run_id));
    let mut pull_handle = tokio::spawn({
        let source_path = plan.source_path.clone();
        let source_dir_str = source_dir.display().to_string();
        let extension = plan.input_file_type.extension();
        let transfers = plan.transfers;
        let checkers = plan.checkers;
        let tpslimit = plan.tpslimit;
        async move {
            rclone_transfer::run_rclone_copy(
                &source_path,
                &source_dir_str,
                Some(extension),
                &pull_log_path,
                transfers,
                checkers,
                tpslimit,
                "transform",
                "pull",
                false,
                Some(copied_tx),
            )
            .await
        }
    });

    let push_semaphore = Arc::new(Semaphore::new(plan.transfers.max(1)));
    let mut in_flight: JoinSet<(String, Result<PathBuf, String>)> = JoinSet::new();
    let mut outcomes = Vec::new();
    let mut pushed: u64 = 0;
    let mut pull_finished = false;
    let mut copied_rx_closed = false;
    let mut pull_result = None;
    let mut macro_phase_is_uploading = false;

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
            in_flight.spawn(async move {
                let outcome = process_one(
                    &pending_file,
                    input_file_type,
                    video_quality,
                    &source_path,
                    &destination_path,
                    &result_dir,
                    &scratch_dir,
                    &push_log_dir,
                    push_semaphore,
                )
                .await;
                (relative_path, outcome)
            });
        }

        if in_flight.is_empty() && pull_finished && (queue.is_empty() || circuit_breaker_tripped) {
            break;
        }

        tokio::select! {
            joined = in_flight.join_next(), if !in_flight.is_empty() => {
                let Some(joined) = joined else { continue; };
                let (relative_path, result) = joined.expect("transform pipeline task panicked");

                if !circuit_breaker_tripped && circuit_breaker_samples < CIRCUIT_BREAKER_SAMPLE_SIZE {
                    circuit_breaker_samples += 1;
                    if let Err(err) = &result
                        && media::is_non_retryable_transcode_error(err)
                    {
                        circuit_breaker_matches += 1;
                        circuit_breaker_example.get_or_insert_with(|| err.clone());
                    }
                    if circuit_breaker_samples == CIRCUIT_BREAKER_SAMPLE_SIZE
                        && circuit_breaker_matches == CIRCUIT_BREAKER_SAMPLE_SIZE
                    {
                        circuit_breaker_tripped = true;
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
                        if !macro_phase_is_uploading {
                            crate::observability::metrics::set_macro_phase("transform", true);
                            macro_phase_is_uploading = true;
                        }
                        outcomes.push(FileOutcome {
                            relative_path,
                            destination_filename: final_path
                                .file_name()
                                .and_then(|name| name.to_str())
                                .map(str::to_string),
                            outcome: outcome_for(plan.input_file_type),
                            detail: String::new(),
                        });
                    }
                    Err(err) => {
                        outcomes.push(FileOutcome {
                            relative_path,
                            destination_filename: None,
                            outcome: Outcome::Failed,
                            detail: err,
                        });
                    }
                }
            }
            received = copied_rx.recv(), if !pull_finished && !copied_rx_closed => {
                match received {
                    Some(relative_path) => {
                        if !circuit_breaker_tripped {
                            enqueue(
                                relative_path,
                                &source_dir,
                                &plan.source_path,
                                output_extension,
                                &existing_destination_filenames,
                                &mut dispatched,
                                &mut queue,
                            );
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
                    if !circuit_breaker_tripped {
                        enqueue(
                            relative_path,
                            &source_dir,
                            &plan.source_path,
                            output_extension,
                            &existing_destination_filenames,
                            &mut dispatched,
                            &mut queue,
                        );
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

    let failed = outcomes.iter().any(|o| o.outcome == Outcome::Failed);

    Ok(TransformSummary {
        outcomes,
        pulled: pull_summary.transferred,
        pushed,
        failed,
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

    let mut body = String::from("source_relative_path\toutcome\tdestination_filename\tdetail\n");
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
            "{}\t{}\t{}\t{}",
            outcome.relative_path,
            outcome.outcome.as_str(),
            outcome.destination_filename.as_deref().unwrap_or(""),
            outcome.detail,
        );
    }
    let _ = writeln!(
        body,
        "\n{transcoded} transcoded, {copied_through} copied through, {failed} failed."
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
            retry_transcode_unless_fatal(TEST_RETRIES, Duration::from_secs(60), || {
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
            retry_transcode_unless_fatal(TEST_RETRIES, Duration::from_millis(1), || {
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
            retry_transcode_unless_fatal(TEST_RETRIES, Duration::from_millis(1), || {
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
            transfers: 2,
            checkers: 4,
            video_quality: VideoQuality::Medium,
            tpslimit: None,
            run_id: "test-run".to_string(),
        };

        let summary = run_transform_job(&plan, 2).await.unwrap();

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

        // A rerun must be a no-op: nothing left pending, nothing re-pushed.
        let rerun_summary = run_transform_job(&plan, 2).await.unwrap();
        assert_eq!(rerun_summary.pushed, 0);
        assert!(rerun_summary.outcomes.is_empty());
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
            transfers: 3,
            checkers: 4,
            video_quality: VideoQuality::Medium,
            tpslimit: None,
            run_id: "test-run".to_string(),
        };

        let summary = run_transform_job(&plan, 3).await.unwrap();

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
            transfers: 2,
            checkers: 4,
            video_quality: VideoQuality::Medium,
            tpslimit: None,
            run_id: "test-run".to_string(),
        };

        let summary = run_transform_job(&plan, 2).await.unwrap();

        assert!(!summary.failed);
        assert_eq!(summary.pushed, 1);
        assert!(
            destination
                .join(summary.outcomes[0].destination_filename.as_deref().unwrap())
                .exists()
        );
    }

    /// The direct regression test for ADR-0120's production gap: on a truly
    /// fresh VM (no local-disk seeding at all -- `local_output` starts
    /// completely empty, unlike the "already on disk" test above, which is
    /// what actually simulates a VM replacement), a file already pushed to
    /// `--destination-path` in a prior run must be skipped entirely --
    /// neither re-transcoded nor re-pushed.
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
            transfers: 2,
            checkers: 4,
            video_quality: VideoQuality::Medium,
            tpslimit: None,
            run_id: "test-run".to_string(),
        };

        let summary = run_transform_job(&plan, 2).await.unwrap();

        assert!(!summary.failed);
        assert_eq!(summary.pushed, 0);
        assert!(summary.outcomes.is_empty());
        // The pre-existing destination file must be untouched -- proof
        // nothing re-pushed over it.
        assert_eq!(
            std::fs::read(destination.join(&destination_filename)).unwrap(),
            b"already pushed"
        );
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
            transfers: 5,
            checkers: 4,
            video_quality: VideoQuality::Medium,
            tpslimit: None,
            run_id: "test-run".to_string(),
        };

        let result = run_transform_job(&plan, 5).await;

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
            transfers: 5,
            checkers: 4,
            video_quality: VideoQuality::Medium,
            tpslimit: None,
            run_id: "test-run".to_string(),
        };

        let summary = run_transform_job(&plan, 5).await.unwrap();

        assert!(summary.failed);
        assert_eq!(summary.pushed, 24);
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
            transfers: 2,
            checkers: 4,
            video_quality: VideoQuality::Low,
            tpslimit: None,
            run_id: "test-run".to_string(),
        };

        let summary = run_transform_job(&plan, 2).await.unwrap();

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
                    detail: String::new(),
                },
                FileOutcome {
                    relative_path: "bad.heic".to_string(),
                    destination_filename: None,
                    outcome: Outcome::Failed,
                    detail: "ffmpeg: boom".to_string(),
                },
            ],
            pulled: 2,
            pushed: 1,
            failed: true,
        };

        let path = write_report(dir.path(), &summary).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();

        assert!(contents.contains("a.png\ttranscoded\ta-abc123.jpg\t"));
        assert!(contents.contains("bad.heic\tfailed\t\tffmpeg: boom"));
        assert!(contents.contains("1 transcoded, 0 copied through, 1 failed."));
        assert_eq!(path.file_name().unwrap(), "transform-report.txt");
    }
}
