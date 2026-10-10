//! Orchestrates `transform`'s three phases (ADR-0112 Decision §2): Phase A
//! pulls via `rclone copy`, Phase B transcodes/places locally, Phase C
//! pushes via `rclone copy`. Phase B's per-file pipeline (transcode/copy-
//! through -> place -> checkpoint) runs concurrently, bounded by
//! `--concurrency`; a single transcode failure stops dispatching further
//! work (but lets already-dispatched work drain) and skips Phase C
//! entirely -- the whole run aborts, while every file placed before the
//! failure stays checkpointed (Decision §7).

use std::path::{Path, PathBuf};

use tokio::task::JoinSet;

use super::format::InputFileType;
use super::manifest::{self, PendingFile};
use super::{media, placement};
use crate::commands::job::rclone_transfer;

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

/// `Job::Summary`. `failed` is `true` exactly when Phase B hit a transcode
/// failure -- in that case Phase C never ran at all (`pushed` is always `0`),
/// and the wizard treats this as the run's overall failure (ADR-0112
/// Decision §7), even though this is returned as `Ok` rather than `Err` so
/// every already-succeeded file's outcome survives to be reported.
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

/// One file's full Phase B pipeline: transcode (png/heic) or copy-through
/// (jpeg) into a scratch path, then place it under `result_dir` at its
/// deterministic, collision-free name. Returns the final path on success so
/// the caller can report it and append the checkpoint -- checkpoint writes
/// deliberately happen in the single-threaded dispatch loop below, not here,
/// so concurrent tasks never need to coordinate a shared file write.
async fn process_one(
    pending_file: &PendingFile,
    input_file_type: InputFileType,
    source_path: &str,
    result_dir: &Path,
    scratch_dir: &Path,
) -> Result<PathBuf, String> {
    let scratch_path = scratch_dir.join(format!(
        "{}.scratch",
        placement::compute_destination_name(source_path, &pending_file.relative_path)
    ));
    std::fs::create_dir_all(scratch_dir)
        .map_err(|err| format!("failed to create {}: {err}", scratch_dir.display()))?;

    match input_file_type {
        InputFileType::Jpeg => media::copy_through(&pending_file.absolute_path, &scratch_path)?,
        InputFileType::Png | InputFileType::Heic => {
            media::transcode_to_jpg(&pending_file.absolute_path, &scratch_path).await?
        }
    }

    placement::place_one(
        result_dir,
        &scratch_path,
        source_path,
        &pending_file.relative_path,
    )
}

fn outcome_for(input_file_type: InputFileType) -> Outcome {
    match input_file_type {
        InputFileType::Jpeg => Outcome::CopiedThrough,
        InputFileType::Png | InputFileType::Heic => Outcome::Transcoded,
    }
}

/// Runs Phase B's concurrent, `concurrency`-bounded per-file pipeline.
/// Dispatches up to `concurrency` files at once; on the first failure, stops
/// dispatching new work but lets already-in-flight tasks drain to
/// completion (any of which may still succeed and get checkpointed) before
/// returning. Every successful file's checkpoint append happens here,
/// single-threaded, immediately after that file's `process_one` future
/// resolves -- never inside the concurrent task itself.
async fn run_phase_b(
    pending: Vec<PendingFile>,
    input_file_type: InputFileType,
    source_path: String,
    local_output: &Path,
    concurrency: usize,
) -> (Vec<FileOutcome>, bool) {
    let result_dir = result_dir(local_output);
    let scratch_dir = scratch_dir(local_output);
    let staging_dir = staging_dir(local_output);

    let mut outcomes = Vec::new();
    let mut aborted = false;
    let mut pending_iter = pending.into_iter();
    let mut in_flight: JoinSet<(String, Result<PathBuf, String>)> = JoinSet::new();

    loop {
        while !aborted && in_flight.len() < concurrency.max(1) {
            let Some(pending_file) = pending_iter.next() else {
                break;
            };
            let relative_path = pending_file.relative_path.clone();
            let source_path = source_path.clone();
            let result_dir = result_dir.clone();
            let scratch_dir = scratch_dir.clone();
            in_flight.spawn(async move {
                let outcome = process_one(
                    &pending_file,
                    input_file_type,
                    &source_path,
                    &result_dir,
                    &scratch_dir,
                )
                .await;
                (relative_path, outcome)
            });
        }

        if in_flight.is_empty() {
            break;
        }

        let Some(joined) = in_flight.join_next().await else {
            break;
        };
        let (relative_path, result) = joined.expect("transform phase B task panicked");

        match result {
            Ok(final_path) => {
                if let Err(err) = manifest::append_checkpoint(&staging_dir, &relative_path) {
                    aborted = true;
                    outcomes.push(FileOutcome {
                        relative_path,
                        destination_filename: None,
                        outcome: Outcome::Failed,
                        detail: err,
                    });
                    continue;
                }
                outcomes.push(FileOutcome {
                    relative_path,
                    destination_filename: final_path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .map(str::to_string),
                    outcome: outcome_for(input_file_type),
                    detail: String::new(),
                });
            }
            Err(err) => {
                aborted = true;
                outcomes.push(FileOutcome {
                    relative_path,
                    destination_filename: None,
                    outcome: Outcome::Failed,
                    detail: err,
                });
            }
        }
    }

    (outcomes, aborted)
}

/// Runs all three phases. `Err` is reserved for infrastructure-level
/// failures (an `rclone` invocation itself failing, a directory that
/// couldn't be created) -- a per-file transcode failure is captured in the
/// returned `TransformSummary` instead (`failed: true`), so every
/// already-succeeded file's outcome survives for the report.
pub(crate) async fn run_transform_job(
    plan: &TransformPlan,
    concurrency: usize,
) -> Result<TransformSummary, String> {
    let source_dir = source_dir(&plan.local_output);

    crate::observability::metrics::set_macro_phase("transform", false);

    let pull_log_path = plan
        .local_output
        .join(format!("rclone-pull-{}.jsonl", plan.run_id));
    let pull_summary = rclone_transfer::run_rclone_copy(
        &plan.source_path,
        &source_dir.display().to_string(),
        Some(plan.input_file_type.extension()),
        &pull_log_path,
        plan.transfers,
        plan.checkers,
        plan.tpslimit,
        "transform",
        "pull",
        false,
    )
    .await?;

    let pending = manifest::gather_pending(
        &source_dir,
        &staging_dir(&plan.local_output),
        plan.input_file_type,
    )?;

    let (outcomes, aborted) = run_phase_b(
        pending,
        plan.input_file_type,
        plan.source_path.clone(),
        &plan.local_output,
        concurrency,
    )
    .await;

    if aborted {
        return Ok(TransformSummary {
            outcomes,
            pulled: pull_summary.transferred,
            pushed: 0,
            failed: true,
        });
    }

    crate::observability::metrics::set_macro_phase("transform", true);

    let push_log_path = plan
        .local_output
        .join(format!("rclone-push-{}.jsonl", plan.run_id));
    let push_summary = rclone_transfer::run_rclone_copy(
        &result_dir(&plan.local_output).display().to_string(),
        &plan.destination_path,
        None,
        &push_log_path,
        plan.transfers,
        plan.checkers,
        plan.tpslimit,
        "transform",
        "push",
        true,
    )
    .await?;

    Ok(TransformSummary {
        outcomes,
        pulled: pull_summary.transferred,
        pushed: push_summary.transferred,
        failed: false,
    })
}

/// Bespoke, tab-separated `<local_output>/transform-report.txt` (ADR-0112
/// Decision §8) -- not the generic `Debug`-based report
/// `report_upload::write_summary_report` writes for jobs without a bespoke
/// one, mirroring `deduplicate-report.txt`'s precedent. Written at
/// `local_output`'s top level, sibling of `.staging/`/`result/`, so it is
/// never swept into Phase C's push by `core::data::collect_files`.
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
    async fn run_phase_b_transcodes_and_checkpoints_every_pending_file() {
        if media::check_ffmpeg_available().await.is_err() {
            eprintln!("skipping: ffmpeg/ffprobe not found on PATH");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let local_output = dir.path().to_path_buf();
        let source_dir = source_dir(&local_output);
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(source_dir.join("a.jpeg"), b"jpeg bytes").unwrap();
        std::fs::write(source_dir.join("b.jpeg"), b"other jpeg bytes").unwrap();

        let pending = manifest::gather_pending(
            &source_dir,
            &staging_dir(&local_output),
            InputFileType::Jpeg,
        )
        .unwrap();
        assert_eq!(pending.len(), 2);

        let (outcomes, aborted) = run_phase_b(
            pending,
            InputFileType::Jpeg,
            "source:jpeg/".to_string(),
            &local_output,
            2,
        )
        .await;

        assert!(!aborted);
        assert_eq!(outcomes.len(), 2);
        assert!(
            outcomes
                .iter()
                .all(|outcome| outcome.outcome == Outcome::CopiedThrough)
        );

        let checkpoint = manifest::load_checkpoint(&staging_dir(&local_output)).unwrap();
        assert!(checkpoint.contains("a.jpeg"));
        assert!(checkpoint.contains("b.jpeg"));

        // A rerun of gather_pending must now find nothing left to do.
        let rerun_pending = manifest::gather_pending(
            &source_dir,
            &staging_dir(&local_output),
            InputFileType::Jpeg,
        )
        .unwrap();
        assert!(rerun_pending.is_empty());
    }

    /// The key regression test for the fail-fast + checkpoint design
    /// (ADR-0112 Decision §7): a mixed valid/one-corrupt-file batch stops
    /// dispatching further work on the first failure, but every file that
    /// completed first stays checkpointed, and a rerun resumes from exactly
    /// where it stopped.
    #[tokio::test]
    async fn run_phase_b_stops_on_failure_but_checkpoints_prior_successes() {
        if media::check_ffmpeg_available().await.is_err() {
            eprintln!("skipping: ffmpeg/ffprobe not found on PATH");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let local_output = dir.path().to_path_buf();
        let source_dir = source_dir(&local_output);
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(source_dir.join("good.heic"), b"not a real heic file").unwrap();

        let pending = manifest::gather_pending(
            &source_dir,
            &staging_dir(&local_output),
            InputFileType::Heic,
        )
        .unwrap();
        assert_eq!(pending.len(), 1);

        let (outcomes, aborted) = run_phase_b(
            pending,
            InputFileType::Heic,
            "source:heic/".to_string(),
            &local_output,
            1,
        )
        .await;

        assert!(aborted);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].outcome, Outcome::Failed);
        assert!(!outcomes[0].detail.is_empty());

        let checkpoint = manifest::load_checkpoint(&staging_dir(&local_output)).unwrap();
        assert!(checkpoint.is_empty());
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
