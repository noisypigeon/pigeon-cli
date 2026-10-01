//! `pigeon job run pull-transform` (ADR-0074): recursively pulls every
//! object from a source bucket, expands zips, recodes media into a
//! size-optimized canonical format per category, dates each file (EXIF for
//! media, document metadata/content otherwise), dedups by SHA-256, and
//! organizes the result by extension -- then optionally encrypts and
//! uploads it to a (possibly different) bucket-config.

pub(crate) mod archive;
pub(crate) mod dedup;
pub(crate) mod documents;
mod manifest;
pub(crate) mod media;
pub mod wizard;
mod worker;

mod date;

use std::collections::HashSet;
use std::path::PathBuf;

use crate::commands::keyring::bucket::store::BucketConfig;
use crate::core::crypto::Aes256GcmSivEncryptor;
use crate::core::job::Job;

pub(crate) use manifest::{PullTransformPlan, TypeSummary, gather_pending};
pub(crate) use media::TranscodeTargets;
pub(crate) use worker::PullTransformSummary;

pub(crate) struct PullTransformJob {
    pub source_bucket: BucketConfig,
    pub source_secret: String,
    pub local_output: PathBuf,
    pub remote: Option<(BucketConfig, String)>,
    pub encryptor: Option<Aes256GcmSivEncryptor>,
    /// Extensions to pull/transform/upload; everything else is dropped
    /// before download (ADR-0077).
    pub allowed_extensions: HashSet<String>,
    /// Keys of pending zip objects to expand+transform; every other zip is
    /// uploaded as-is, untouched (ADR-0077).
    pub expand_zip_keys: HashSet<String>,
    /// Confirmed/adapted media-transcoding targets for this run (ADR-0077),
    /// never persisted.
    pub transcode_targets: TranscodeTargets,
}

impl Job for PullTransformJob {
    type Plan = PullTransformPlan;
    type Summary = PullTransformSummary;

    async fn gather(&self) -> Result<PullTransformPlan, String> {
        gather_pending(&self.source_bucket, &self.source_secret, &self.local_output).await
    }

    async fn run(
        self,
        plan: PullTransformPlan,
        concurrency: usize,
    ) -> Result<PullTransformSummary, String> {
        let remote_ref = self
            .remote
            .as_ref()
            .map(|(bucket_config, secret)| (bucket_config, secret.as_str()));
        worker::run_pull_transform_job(
            &self.source_bucket,
            &self.source_secret,
            &self.local_output,
            plan.tasks,
            concurrency,
            remote_ref,
            self.encryptor.as_ref(),
            self.allowed_extensions,
            self.expand_zip_keys,
            self.transcode_targets,
        )
        .await
    }
}
