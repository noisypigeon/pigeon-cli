//! `pigeon job run transform` (ADR-0112, pipeline reworked by ADR-0116, video
//! input kinds added by ADR-0122, `mp4-audio` by ADR-0142, spreadsheets by
//! ADR-0149, the audio family by ADR-0153, documents by ADR-0154): shells out
//! to `rclone` to pull source files matching `--input-file-type` into a local
//! staging tree, transcodes or copies them locally into a destination filename
//! unique by construction (never collision-detected-and-fixed, and never
//! deduplicated -- duplicate source content is deliberately retained at
//! separate destination names), then pushes each file individually via
//! `rclone copyto` the moment it's ready rather than waiting for the whole
//! batch.
//!
//! Five input families, and since ADR-0154 they no longer share one output
//! extension apiece -- documents fan out to three, which is why
//! `format::InputFileType::output_extension` rather than any `is_*()`
//! predicate is the authority on where a kind's output lands:
//!
//! | Output | Kinds |
//! |---|---|
//! | `.jpg` | `png`, `jpeg`, `heic` |
//! | `.mp4` | `mov`, `m4v`, `mp4` (H.265 at the chosen `--video-quality`) |
//! | `.opus` | `mp4-audio`, `m4a`, `mp3`, `aac`, `wav`, `opus` (at `--audio-quality`) |
//! | `.csv` | `numbers`, `xlsx`, `xls`, `csv`, `tsv` -- one file per non-empty sheet |
//! | `.pdf` | `doc`, `docx`, `pages`, `rtf`, `pdf`, `epub`, `pptx`, `key` |
//! | `.txt` | `txt`, `md`/`markdown`/`mkd`, `rst`, `textclipping` |
//!
//! `jpeg`, `csv`, `pdf` and `txt` arrive already in their target format and are
//! copied through byte-for-byte; `opus` is the one kind that is deliberately
//! re-encoded anyway (ADR-0153 §6).
//!
//! Converters: `ffmpeg`/`ffprobe` for image/video/audio, `soffice` for
//! `numbers` and the six LibreOffice-readable document kinds, `ebook-convert`
//! for `epub` alone (LibreOffice's EPUB filter is export-only), and `pandoc`
//! for `md`/`rst`. The spreadsheet, text and tabular kinds that need none of
//! them are read in-process.
//!
//! See `worker`'s module doc for the per-file pipeline's full shape, `push`'s
//! for the per-file push leg, and `placement`'s for the naming scheme.

mod destination;
mod documents;
mod email_shape;
mod format;
mod icns;
mod lzo1x;
mod manifest;
mod media;
mod placement;
mod procreate;
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
