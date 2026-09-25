pub mod wizard;
mod worker;

use std::path::PathBuf;

use crate::core::crypto::Aes256GcmSivEncryptor;
use crate::core::job::Job;
use worker::{DecryptSummary, DecryptTask};

/// The `Job` implementor for `pigeon job run decrypt-files` (ADR-0028) --
/// `core::job::Job`'s second real implementor, alongside `EmailSyncJob`.
pub(crate) struct DecryptFilesJob {
    pub input_dir: PathBuf,
    pub output_dir: PathBuf,
    pub encryptor: Aes256GcmSivEncryptor,
}

impl Job for DecryptFilesJob {
    type Plan = Vec<DecryptTask>;
    type Summary = DecryptSummary;

    async fn gather(&self) -> Result<Vec<DecryptTask>, String> {
        worker::collect_decrypt_tasks(&self.input_dir, &self.output_dir)
    }

    async fn run(
        self,
        plan: Vec<DecryptTask>,
        concurrency: usize,
    ) -> Result<DecryptSummary, String> {
        Ok(worker::run_decrypt_phase(plan, &self.encryptor, concurrency).await)
    }
}
