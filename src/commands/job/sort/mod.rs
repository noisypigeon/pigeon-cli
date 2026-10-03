//! `pigeon job run sort` (ADR-0083): downloads every object from a source
//! bucket, flattens it into top-level `<extension>/` folders by each
//! file's literal (non-canonicalized) extension, always disambiguates a
//! filename collision via `unique_path` (never hash-checked -- uniqueness
//! is assumed already established by whatever produced the source
//! bucket's contents, e.g. a prior `dedupe` run), and uploads the result
//! unencrypted to a mandatory output bucket.
//!
//! Same `.staging`/`result` output split ADR-0082 established:
//! `local_output/.staging/` holds bookkeeping only (`.processed`
//! checkpoint, raw downloaded files awaiting placement) and is never
//! uploaded; `local_output/result/<extension>/...` holds the real
//! deliverable and is what the upload phase walks.

mod manifest;
pub mod wizard;
mod worker;

use std::path::PathBuf;

use crate::commands::keyring::bucket::store::BucketConfig;
use crate::core::job::Job;

pub(crate) use manifest::{SortPlan, gather_pending};
pub(crate) use worker::SortSummary;

/// `remote` is a plain tuple, not `Option<...>` -- uploading is mandatory
/// for this job (ADR-0083 §1), a permanent scoping decision reflected
/// structurally here, not just validated at the wizard layer.
pub(crate) struct SortJob {
    pub source_bucket: BucketConfig,
    pub source_secret: String,
    pub local_output: PathBuf,
    pub remote: (BucketConfig, String),
}

impl Job for SortJob {
    type Plan = SortPlan;
    type Summary = SortSummary;

    async fn gather(&self) -> Result<SortPlan, String> {
        gather_pending(
            &self.source_bucket,
            &self.source_secret,
            &self.local_output.join(".staging"),
        )
        .await
    }

    async fn run(
        self,
        plan: SortPlan,
        concurrency: usize,
        upload_concurrency: usize,
    ) -> Result<SortSummary, String> {
        worker::run_sort_job(
            &self.source_bucket,
            &self.source_secret,
            &self.local_output,
            plan.tasks,
            concurrency,
            upload_concurrency,
            (&self.remote.0, self.remote.1.as_str()),
        )
        .await
    }
}
