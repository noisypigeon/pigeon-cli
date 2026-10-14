//! `pigeon job run transform` (ADR-0112, pipeline reworked by ADR-0116, video
//! input kinds added by ADR-0122, `mp4-audio` by ADR-0142): shells out to
//! `rclone` to pull source files matching `--input-file-type`
//! (`png`/`jpeg`/`heic`/`mov`/`m4v`/`mp4`/`mp4-audio`)
//! into a local staging tree, transcodes or copies them locally into a
//! destination filename unique by construction (never collision-detected-
//! and-fixed, and never deduplicated -- duplicate source content is
//! deliberately retained at separate destination names) -- full-size,
//! maximum-quality `.jpg` for image kinds, `.mp4` (H.265/HEVC at the chosen
//! `--video-quality`) for video kinds, Opus `.opus` for `mp4-audio` -- then
//! pushes each file individually
//! via `rclone copyto` the moment it's ready rather than waiting for the
//! whole batch. See `worker`'s module doc comment for the per-file
//! pipeline's full shape, `push`'s for the per-file push leg, and
//! `placement`'s for the naming scheme.

mod destination;
mod documents;
mod format;
mod manifest;
mod media;
mod placement;
mod push;
mod sheets;
mod soffice;
pub mod wizard;
mod worker;

use std::path::PathBuf;

use crate::core::job::Job;

pub(crate) use format::{AudioQuality, InputFileType, VideoQuality};
pub(crate) use worker::{TransformPlan, TransformSummary};

pub(crate) struct TransformJob {
    pub source_path: String,
    pub destination_path: String,
    pub local_output: PathBuf,
    pub input_file_type: InputFileType,
    pub video_quality: VideoQuality,
    pub audio_quality: AudioQuality,
    pub download_transfers: usize,
    pub download_checkers: usize,
    pub tpslimit: Option<usize>,
    pub max_pending_downloads: usize,
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
            video_quality: self.video_quality,
            audio_quality: self.audio_quality,
            download_transfers: self.download_transfers,
            download_checkers: self.download_checkers,
            tpslimit: self.tpslimit,
            max_pending_downloads: self.max_pending_downloads,
            run_id: self.run_id.clone(),
        })
    }

    /// Honors `upload_concurrency` (ADR-0091 §3) to size the push phase's
    /// semaphore, bringing `transform` in line with every other
    /// upload-capable job (ADR-0129). Previously this parameter was
    /// ignored with a hardcoded value of 1; now it's the resolved
    /// `--upload-transfers` value from the wizard.
    async fn run(
        self,
        plan: TransformPlan,
        concurrency: usize,
        upload_concurrency: usize,
    ) -> Result<TransformSummary, String> {
        worker::run_transform_job(&plan, concurrency, upload_concurrency).await
    }
}
