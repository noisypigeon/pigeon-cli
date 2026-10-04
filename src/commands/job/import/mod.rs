//! `pigeon job run import` (ADR-0101): copies data from a configurable
//! source to a configurable destination by shelling out to the external
//! `rclone` binary, with its own performance/retry flags fixed. Unlike
//! every other job, `--source`/`--destination` are raw `remote:path`
//! strings passed straight through to `rclone copy`'s argv, never pigeon
//! `BucketConfig`/keyring aliases -- the backing `rclone.conf` is
//! provisioned by an external deployment process, entirely outside this
//! crate's scope. This is also why `import` can reach backends pigeon's
//! own `minio`-backed jobs never could.

mod rclone_log;
pub mod wizard;
mod worker;

use std::path::PathBuf;

use crate::core::job::Job;

pub(crate) use worker::{ImportPlan, ImportSummary};

/// The `Job` implementor for `pigeon job run import`.
pub(crate) struct ImportJob {
    pub source: String,
    pub destination: String,
    pub log_path: PathBuf,
}

impl Job for ImportJob {
    type Plan = ImportPlan;
    type Summary = ImportSummary;

    /// Trivially carries the already-validated fields forward -- unlike
    /// every other job, there's no file-by-file manifest to discover here:
    /// `rclone` does its own source listing/diffing internally.
    async fn gather(&self) -> Result<ImportPlan, String> {
        Ok(ImportPlan {
            source: self.source.clone(),
            destination: self.destination.clone(),
            log_path: self.log_path.clone(),
        })
    }

    /// Ignores both concurrency parameters -- rclone's own hardcoded
    /// `--transfers`/`--checkers` flags own that dial (ADR-0101), matching
    /// `DecryptFilesJob`'s precedent (ADR-0090) for a job this trait
    /// parameter doesn't apply to.
    async fn run(
        self,
        plan: ImportPlan,
        _concurrency: usize,
        _upload_concurrency: usize,
    ) -> Result<ImportSummary, String> {
        worker::run_import_job(&plan.source, &plan.destination, &plan.log_path).await
    }
}
