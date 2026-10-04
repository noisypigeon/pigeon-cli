//! `pigeon job run reduce` (ADR-0096): runs after `deduplicate`, classifying
//! each top-level `<extension>/` directory of an already-deduplicated
//! source bucket as either genuinely valuable or an artifact/piece of
//! media easily reproduced from an external canonical source, and
//! forwarding only the valuable extensions to a mandatory destination
//! bucket. A `Reproducible` extension's objects are never downloaded at
//! all -- this codebase has no server-side bucket-to-bucket copy
//! primitive, so every forwarded object pays a download-then-reupload
//! round-trip; `reduce` exists specifically to skip that round-trip for
//! whatever it's about to discard.
//!
//! Two directory trees exist per run, mirrored under `local_output`:
//! `local_output/.staging/` holds bookkeeping only (`.processed`
//! checkpoint, raw downloaded files awaiting placement) and is never
//! uploaded; `local_output/result/<extension>/...` holds the forwarded
//! deliverable and is what the upload phase walks.

pub(crate) mod classify;
mod manifest;
pub mod wizard;
mod worker;

use std::path::PathBuf;

use crate::commands::keyring::bucket::store::BucketConfig;
use crate::core::job::Job;

pub(crate) use manifest::{ReducePlan, gather_pending};
pub(crate) use worker::ReduceSummary;

/// Unlike `DeduplicateJob`'s optional `remote` (resolved only *after*
/// `gather()`, since uploading is genuinely optional there), `reduce`'s
/// upload is mandatory -- so the wizard resolves `remote` up front, before
/// ever calling `gather()`, letting a missing `--remote-output` fail fast
/// non-interactively without paying for a bucket listing call first.
pub(crate) struct ReduceJob {
    pub source_bucket: BucketConfig,
    pub source_secret: String,
    pub local_output: PathBuf,
    pub force_valuable: Vec<String>,
    pub force_reproducible: Vec<String>,
    pub remote: (BucketConfig, String),
}

impl Job for ReduceJob {
    type Plan = ReducePlan;
    type Summary = ReduceSummary;

    async fn gather(&self) -> Result<ReducePlan, String> {
        gather_pending(
            &self.source_bucket,
            &self.source_secret,
            &self.local_output.join(".staging"),
            &self.force_valuable,
            &self.force_reproducible,
        )
        .await
    }

    async fn run(
        self,
        plan: ReducePlan,
        concurrency: usize,
        upload_concurrency: usize,
    ) -> Result<ReduceSummary, String> {
        let (remote_bucket, remote_secret) = &self.remote;
        worker::run_reduce_job(
            &self.source_bucket,
            &self.source_secret,
            &self.local_output,
            plan,
            concurrency,
            upload_concurrency,
            (remote_bucket, remote_secret),
        )
        .await
    }
}
