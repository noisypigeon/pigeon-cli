//! `pigeon job run deduplicate` (ADR-0082): recursively pulls every object from a
//! source bucket, always inflates every zip encountered (containers
//! themselves never uploaded, only their inflated contents), content-hashes
//! (SHA-256) everything bucket-wide to keep one byte-identical copy of each
//! file, writes a human-readable merge report, and optionally uploads the
//! result unencrypted to a (possibly different) bucket-config.
//!
//! Two directory trees exist per run, mirrored under `local_output`:
//! `local_output/.staging/` holds bookkeeping only (`.processed` checkpoint,
//! `.content-hashes` index, raw downloaded/extracted files awaiting hash and
//! placement) and is never uploaded; `local_output/result/<extension>/...`
//! holds the real deliverable and is what the upload phase walks.
//! `local_output/deduplicate-report.txt` sits at the top level, deliberately
//! outside `result/`, so it can never be swept into the destination bucket
//! (`core::data::collect_files` does not skip dotfiles/dot-directories, so
//! this split is load-bearing, not cosmetic -- see `dedup.rs`).

pub mod dedup;
mod manifest;
pub mod wizard;
mod worker;

use std::path::PathBuf;

use crate::commands::keyring::bucket::store::BucketConfig;
use crate::core::job::Job;

pub(crate) use manifest::{DeduplicatePlan, gather_pending};
pub(crate) use worker::DeduplicateSummary;

/// No `encryptor` field at all -- a permanent scoping decision (ADR-0082
/// §0), not a temporarily-unused slot. `source_buckets` (ADR-0109) can name
/// more than one bucket-config -- every one is downloaded into one shared
/// local staging tree and deduplicated across the combined set, not
/// per-bucket. `transfers`/`checkers`/`tpslimit` (ADR-0136) size the upload
/// phase's bulk `rclone copy` -- `Job::run`'s own `upload_concurrency`
/// parameter no longer applies once that phase is one bulk transfer rather
/// than a per-file task queue, so it's ignored here (same "not uniformly"
/// precedent ADR-0090 already established for a dial that doesn't apply to
/// every implementor).
pub(crate) struct DeduplicateJob {
    pub source_buckets: Vec<(BucketConfig, String)>,
    pub local_output: PathBuf,
    pub remote: Option<(BucketConfig, String)>,
    pub transfers: usize,
    pub checkers: usize,
    pub tpslimit: Option<usize>,
}

impl Job for DeduplicateJob {
    type Plan = DeduplicatePlan;
    type Summary = DeduplicateSummary;

    async fn gather(&self) -> Result<DeduplicatePlan, String> {
        gather_pending(&self.source_buckets, &self.local_output.join(".staging")).await
    }

    async fn run(
        self,
        plan: DeduplicatePlan,
        concurrency: usize,
        _upload_concurrency: usize,
    ) -> Result<DeduplicateSummary, String> {
        let remote_ref = self
            .remote
            .as_ref()
            .map(|(bucket_config, secret)| (bucket_config, secret.as_str()));
        worker::run_deduplicate_job(
            &self.source_buckets,
            &self.local_output,
            plan.tasks,
            concurrency,
            self.transfers,
            self.checkers,
            self.tpslimit,
            remote_ref,
        )
        .await
    }
}
