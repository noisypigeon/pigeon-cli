//! `pigeon job run transform` (ADR-0112, pipeline reworked by ADR-0116):
//! shells out to `rclone` to pull source files matching
//! `--input-file-type` (`png`/`jpeg`/`heic`) into a local staging tree,
//! transcodes or copies them locally into full-size, maximum-quality `.jpg`
//! at a destination filename unique by construction (never collision-
//! detected-and-fixed, and never deduplicated -- duplicate source content
//! is deliberately retained at separate destination names), then pushes
//! each file individually via `rclone copyto` the moment it's ready rather
//! than waiting for the whole batch. See `worker`'s module doc comment for
//! the per-file pipeline's full shape, `push`'s for the per-file push leg,
//! and `placement`'s for the naming scheme.

mod destination;
mod format;
mod manifest;
mod media;
mod placement;
mod push;
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
    /// manifest to discover ahead of time: the bulk pull's own `rclone copy`
    /// does its own listing, and the per-file pipeline's manifest is
    /// discovered incrementally as that pull reports each file's completion
    /// (`worker`'s module doc comment), not gathered up front.
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

    /// Ignores `upload_concurrency` -- ADR-0091 §3's intended hook for this
    /// job's push leg, but `transform`'s CLI surface has no
    /// `--upload-concurrency` flag (ADR-0116): push concurrency is bounded
    /// by a semaphore sized from `--transfers` instead, decoupled from
    /// `concurrency` (which sizes the CPU-bound transcode pool), matching
    /// `RcloneCopyJob::run`'s precedent for a job this trait parameter
    /// doesn't apply to.
    async fn run(
        self,
        plan: TransformPlan,
        concurrency: usize,
        _upload_concurrency: usize,
    ) -> Result<TransformSummary, String> {
        worker::run_transform_job(&plan, concurrency).await
    }
}
