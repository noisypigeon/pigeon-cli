//! `pigeon job run rclone copy`/`rclone delete` (CLI-0101, restructured by
//! CLI-0110): copies or recursively deletes data by shelling out to the
//! external `rclone` binary. `--source-bucket`/`--destination-bucket` are
//! pigeon keyring `BucketConfig` aliases (CLI-0136, reversing CLI-0101's
//! original "raw passthrough, never a bucket alias" design) -- `rclone` is
//! always invoked against a config file pigeon itself generates from the
//! keyring, never an externally-provisioned or user-general-purpose
//! `rclone.conf`. `copy`'s `--transfers`/`--checkers`/`--tpslimit` are
//! overridable per-destination (CLI-0108); every other rclone
//! performance/retry flag stays fixed. `delete` (`rclone purge`) has no
//! destination and no transfer-tuning flags at all.

pub mod wizard;
mod worker;

use std::path::PathBuf;

use crate::core::job::Job;

pub(crate) use worker::{RcloneCopyPlan, RcloneCopySummary, RcloneDeletePlan, RcloneDeleteSummary};

/// The `Job` implementor for `pigeon job run rclone copy`.
pub(crate) struct RcloneCopyJob {
    pub source: String,
    pub destination: String,
    pub log_path: PathBuf,
    pub transfers: usize,
    pub checkers: usize,
    pub tpslimit: Option<usize>,
}

impl Job for RcloneCopyJob {
    type Plan = RcloneCopyPlan;
    type Summary = RcloneCopySummary;

    /// Trivially carries the already-validated fields forward -- unlike
    /// every other job, there's no file-by-file manifest to discover here:
    /// `rclone` does its own source listing/diffing internally.
    async fn gather(&self) -> Result<RcloneCopyPlan, String> {
        Ok(RcloneCopyPlan {
            source: self.source.clone(),
            destination: self.destination.clone(),
            log_path: self.log_path.clone(),
            transfers: self.transfers,
            checkers: self.checkers,
            tpslimit: self.tpslimit,
        })
    }

    /// Ignores both concurrency parameters -- rclone's own
    /// `--transfers`/`--checkers` flags own that dial, resolved per-run
    /// (CLI-0108), matching `DecryptFilesJob`'s precedent (CLI-0090) for a
    /// job this trait parameter doesn't apply to.
    async fn run(
        self,
        plan: RcloneCopyPlan,
        _concurrency: usize,
        _upload_concurrency: usize,
    ) -> Result<RcloneCopySummary, String> {
        worker::run_copy_job(
            &plan.source,
            &plan.destination,
            &plan.log_path,
            plan.transfers,
            plan.checkers,
            plan.tpslimit,
        )
        .await
    }
}

/// The `Job` implementor for `pigeon job run rclone delete` (CLI-0110).
/// Kept as its own struct rather than folded into `RcloneCopyJob` with an
/// action discriminant -- its `Plan`/`Summary` shape genuinely differs (no
/// destination, no transfers/tpslimit, no bytes-transferred count), and
/// nothing holds a job value generically across both actions since
/// dispatch (`src/commands/job/commands.rs`) already branches on the
/// action before either job type is constructed.
pub(crate) struct RcloneDeleteJob {
    pub source: String,
    pub log_path: PathBuf,
    pub checkers: usize,
}

impl Job for RcloneDeleteJob {
    type Plan = RcloneDeletePlan;
    type Summary = RcloneDeleteSummary;

    async fn gather(&self) -> Result<RcloneDeletePlan, String> {
        Ok(RcloneDeletePlan {
            source: self.source.clone(),
            log_path: self.log_path.clone(),
            checkers: self.checkers,
        })
    }

    /// Ignores both concurrency parameters, same reasoning as
    /// `RcloneCopyJob::run` -- `rclone purge`'s own `--checkers` flag owns
    /// this dial.
    async fn run(
        self,
        plan: RcloneDeletePlan,
        _concurrency: usize,
        _upload_concurrency: usize,
    ) -> Result<RcloneDeleteSummary, String> {
        worker::run_delete_job(&plan.source, &plan.log_path, plan.checkers).await
    }
}
