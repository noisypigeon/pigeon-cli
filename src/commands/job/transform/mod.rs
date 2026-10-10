//! `pigeon job run transform` (ADR-0112): shells out to `rclone` to pull
//! source files matching `--input-file-type` (`png`/`jpeg`/`heic`) into a
//! local staging tree, transcodes or copies them locally into full-size,
//! maximum-quality `.jpg` at a destination filename unique by construction
//! (never collision-detected-and-fixed, and never deduplicated -- duplicate
//! source content is deliberately retained at separate destination names),
//! then shells out to `rclone` again to push the result. See `worker`'s
//! module doc comment for the phase-by-phase breakdown, and `placement`'s
//! for the naming scheme.

mod format;
mod manifest;
mod media;
mod placement;
pub mod wizard;
mod worker;

use std::path::PathBuf;

use crate::core::job::Job;

pub(crate) use format::InputFileType;
pub(crate) use worker::{TransformPlan, TransformSummary};

pub(crate) struct TransformJob {
    pub source_path: String,
    pub destination_path: String,
    pub local_output: PathBuf,
    pub input_file_type: InputFileType,
    pub transfers: usize,
    pub checkers: usize,
    pub tpslimit: Option<usize>,
    pub run_id: String,
}

impl Job for TransformJob {
    type Plan = TransformPlan;
    type Summary = TransformSummary;

    /// Trivially carries the already-validated fields forward, same
    /// reasoning as `RcloneCopyJob::gather` -- there's no file-by-file
    /// manifest to discover ahead of time: Phase A's `rclone copy` does its
    /// own listing, and Phase B's own manifest is only knowable once Phase A
    /// has actually pulled files to `<local_output>/source/`.
    async fn gather(&self) -> Result<TransformPlan, String> {
        Ok(TransformPlan {
            source_path: self.source_path.clone(),
            destination_path: self.destination_path.clone(),
            local_output: self.local_output.clone(),
            input_file_type: self.input_file_type,
            transfers: self.transfers,
            checkers: self.checkers,
            tpslimit: self.tpslimit,
            run_id: self.run_id.clone(),
        })
    }

    /// Ignores `upload_concurrency` -- `transform` has no upload phase of
    /// the shape that parameter sizes (ADR-0091 §3); both of its `rclone
    /// copy` phases manage their own parallelism via `--transfers`/
    /// `--checkers`, matching `RcloneCopyJob::run`'s precedent for a job
    /// this trait parameter doesn't apply to. `concurrency` sizes Phase B's
    /// transcode pool.
    async fn run(
        self,
        plan: TransformPlan,
        concurrency: usize,
        _upload_concurrency: usize,
    ) -> Result<TransformSummary, String> {
        worker::run_transform_job(&plan, concurrency).await
    }
}
