/// Behavior shared by every job type this CLI can run. Two implementors
/// exist today: `commands::job::email_sync::EmailSyncJob` (introduced per
/// explicit direction, initially understood as a consistency/extensibility
/// choice rather than a response to a second job type actually existing --
/// ADR-0021 deliberately scoped that out; ADR-0023 Consequences names this
/// cost plainly) and `commands::job::decrypt_files::DecryptFilesJob`
/// (ADR-0028), the first real validation of that choice.
///
/// `gather`/`run` use native async fn in traits (stable since Rust 1.75,
/// no `async-trait` dependency needed) -- safe here because `Job` is only
/// ever used as a concrete type parameter (`EmailSyncJob`), never boxed as
/// `dyn Job`, so AFIT's dyn-compatibility limitation never applies.
pub(crate) trait Job {
    /// Whatever `gather` discovers, needed by `run` to actually do the
    /// work -- kept as an associated type rather than folded into `Self`
    /// so a job can be inspected (e.g. to print a summary) between
    /// gathering and running.
    type Plan;
    type Summary;

    /// Discover pending work without doing any of it (ADR-0021 §3/§4).
    async fn gather(&self) -> Result<Self::Plan, String>;

    /// Execute `plan` at the given concurrency, after the wizard's confirm
    /// step. Consumes `self` since a job's identity/credentials are only
    /// ever run once per invocation. `upload_concurrency` sizes this job's
    /// upload phase independently of `concurrency` (which sizes its primary
    /// download/hash/transform/fetch work) -- a network-round-trip-bound
    /// phase has no principled reason to share a dial with CPU-bound or
    /// IMAP-bound work (ADR-0091 §3). `DecryptFilesJob`, which has no
    /// upload phase, accepts and ignores it, matching ADR-0090's "not
    /// uniformly" precedent for a concurrency-shaped change that doesn't
    /// apply to every implementor.
    async fn run(
        self,
        plan: Self::Plan,
        concurrency: usize,
        upload_concurrency: usize,
    ) -> Result<Self::Summary, String>;
}
