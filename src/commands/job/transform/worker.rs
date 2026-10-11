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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinSet;

use super::format::InputFileType;
use super::manifest::{self, PendingFile};
use super::{destination, media, placement, push};
use crate::commands::job::rclone_transfer;
use crate::core::retry::retry_with_backoff;

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

pub(crate) struct TransformPlan {
    pub source_path: String,
    pub destination_path: String,
    pub local_output: PathBuf,
    pub input_file_type: InputFileType,
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
    source_path: &str,
    destination_path: &str,
    result_dir: &Path,
    scratch_dir: &Path,
    push_log_dir: &Path,
    push_semaphore: Arc<Semaphore>,
) -> Result<PathBuf, String> {
    let scratch_path = scratch_dir.join(format!(
        "{}.scratch",
        placement::compute_destination_name(source_path, &pending_file.relative_path)
    ));
    std::fs::create_dir_all(scratch_dir)
        .map_err(|err| format!("failed to create {}: {err}", scratch_dir.display()))?;

    let transcode_result = match input_file_type {
        InputFileType::Jpeg => media::copy_through(&pending_file.absolute_path, &scratch_path),
        InputFileType::Png | InputFileType::Heic => {
            retry_with_backoff(TRANSCODE_RETRIES, TRANSCODE_RETRY_BACKOFF, || {
                media::transcode_to_jpg(&pending_file.absolute_path, &scratch_path)
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
        InputFileType::Png | InputFileType::Heic => Outcome::Transcoded,
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
    existing_destination_filenames: &HashSet<String>,
    dispatched: &mut HashSet<String>,
    queue: &mut VecDeque<PendingFile>,
) {
    let destination_name = placement::compute_destination_name(source_path, &relative_path);
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

    loop {
        while in_flight.len() < concurrency.max(1) {
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
            in_flight.spawn(async move {
                let outcome = process_one(
                    &pending_file,
                    input_file_type,
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

        if in_flight.is_empty() && pull_finished && queue.is_empty() {
            break;
        }

        tokio::select! {
            joined = in_flight.join_next(), if !in_flight.is_empty() => {
                let Some(joined) = joined else { continue; };
                let (relative_path, result) = joined.expect("transform pipeline task panicked");
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
                    Some(relative_path) => enqueue(
                        relative_path,
                        &source_dir,
                        &plan.source_path,
                        &existing_destination_filenames,
                        &mut dispatched,
                        &mut queue,
                    ),
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
                    enqueue(
                        relative_path,
                        &source_dir,
                        &plan.source_path,
                        &existing_destination_filenames,
                        &mut dispatched,
                        &mut queue,
                    );
                }
                pull_result = Some(result);
            }
        }
    }

    let pull_summary = pull_result.expect("loop only exits after pull_finished is set")?;
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
    use super::*;

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
        let destination_filename = placement::compute_destination_name(&source_path, "a.jpeg");
        std::fs::write(destination.join(&destination_filename), b"already pushed").unwrap();

        let plan = TransformPlan {
            source_path,
            destination_path: destination.display().to_string(),
            local_output: local_output.clone(),
            input_file_type: InputFileType::Jpeg,
            transfers: 2,
            checkers: 4,
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
        existing_destination_filenames
            .insert(placement::compute_destination_name(source_path, "a.png"));
        let mut dispatched = HashSet::new();
        let mut queue = VecDeque::new();

        enqueue(
            "a.png".to_string(),
            &source_dir,
            source_path,
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
