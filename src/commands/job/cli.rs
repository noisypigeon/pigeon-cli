use std::path::PathBuf;

use clap::{Args, Subcommand};

use crate::core::observability::Observable;

#[derive(Args, Debug)]
pub struct JobArgs {
    #[command(subcommand)]
    pub command: JobCommands,
}

#[derive(Subcommand, Debug)]
pub enum JobCommands {
    /// Run a job (see subcommands for available job types)
    Run(RunArgs),
}

#[derive(Args, Debug)]
pub struct RunArgs {
    #[command(subcommand)]
    pub job_type: JobType,
}

#[derive(Subcommand, Debug)]
pub enum JobType {
    /// Fetch, transform, deduplicate, and optionally upload mail for one or
    /// more authenticated email identities. Replaces `pigeon email sync`
    /// (ADR-0021).
    EmailSync {
        /// Aliases of the identities to sync, comma-separated. Interactively
        /// selected from the authenticated identities when omitted and
        /// stdin is a terminal; required otherwise.
        #[arg(long, value_delimiter = ',')]
        identities: Option<Vec<String>>,

        /// Local directory to stage and store output under, shared across
        /// every selected identity (each gets its own subdirectory
        /// underneath). Defaults to a directory under the OS temp directory
        /// when omitted.
        #[arg(long)]
        local_output: Option<PathBuf>,

        /// Alias of a configured bucket-config (see `pigeon dataops
        /// bucket-config new`) to upload each identity's local result tree
        /// to, once its local fetch/transform/dedupe phase is complete.
        #[arg(long)]
        remote_output: Option<String>,

        /// Alias of a configured encryption key (see `pigeon keyring add
        /// encryption-key`) to use, overriding the target bucket-config's
        /// own default (if any). Interactively selected/confirmed when
        /// omitted; falls back to the bucket's default non-interactively
        /// (ADR-0027).
        #[arg(long)]
        encryption_key: Option<String>,

        /// Maximum number of fetch/transform workers to run concurrently,
        /// spanning every selected identity's every mailbox. Interactively
        /// prompted (with a rough time estimate) when omitted and stdin is
        /// a terminal; required otherwise.
        #[arg(long)]
        concurrency: Option<usize>,

        /// Maximum number of simultaneous IMAP connections opened to any
        /// one identity, regardless of `--concurrency` (ADR-0071) -- caps
        /// worker concurrency per-account rather than only globally, so a
        /// mailbox with enough pending batches can't cause more than this
        /// many workers to log in to the same account at once and trip a
        /// provider's simultaneous-connection limit. Defaults to 6 (well
        /// under Gmail's documented 15-connection cap) when omitted; not
        /// interactively prompted.
        #[arg(long)]
        max_connections_per_identity: Option<usize>,

        /// Skip the final "proceed?" confirmation. Every other omitted
        /// input (identities, concurrency) still follows its own
        /// independent flag-or-prompt rule -- this only answers the last
        /// prompt.
        #[arg(long)]
        yes: bool,
    },

    /// Decrypts every `*.enc` file under `--input-dir` into `--output-dir`
    /// (`.enc` suffix stripped, relative structure preserved), using a
    /// configured encryption key (ADR-0028).
    DecryptFiles {
        /// Directory containing `*.enc` files to decrypt. Interactively
        /// prompted when omitted and stdin is a terminal; required
        /// otherwise.
        #[arg(long)]
        input_dir: Option<PathBuf>,

        /// Directory decrypted files are written under, mirroring
        /// `--input-dir`'s relative structure. Must not be the same
        /// directory as `--input-dir`. Interactively prompted when omitted
        /// and stdin is a terminal; required otherwise.
        #[arg(long)]
        output_dir: Option<PathBuf>,

        /// Alias of a configured encryption key (see `pigeon keyring add
        /// encryption-key`) to decrypt with. Interactively selected when
        /// omitted and stdin is a terminal; required otherwise.
        #[arg(long)]
        encryption_key: Option<String>,

        /// Maximum number of files to decrypt concurrently.
        #[arg(long)]
        concurrency: Option<usize>,

        /// Skip the final "proceed?" confirmation.
        #[arg(long)]
        yes: bool,
    },

    /// Fetches raw `.eml` files and unpacked attachments (no Markdown/
    /// frontmatter transform) for one or more authenticated email
    /// identities, deduplicating attachments by content, and optionally
    /// uploads the result unencrypted to a bucket-config (ADR-0081).
    EmailPull {
        /// Aliases of the identities to pull, comma-separated.
        /// Interactively selected from the authenticated identities when
        /// omitted and stdin is a terminal; required otherwise.
        #[arg(long, value_delimiter = ',')]
        identities: Option<Vec<String>>,

        /// Local directory to stage and store output under, shared across
        /// every selected identity. Defaults to a directory under the OS
        /// temp directory when omitted.
        #[arg(long)]
        local_output: Option<PathBuf>,

        /// Alias of a configured bucket-config to upload each identity's
        /// local result tree to, once its local fetch/dedupe phase is
        /// complete. Always uploaded unencrypted -- this job never offers
        /// encryption (ADR-0081).
        #[arg(long)]
        remote_output: Option<String>,

        /// Maximum number of fetch/extract workers to run concurrently,
        /// spanning every selected identity's every mailbox. Interactively
        /// prompted (with a rough time estimate) when omitted and stdin is
        /// a terminal; required otherwise.
        #[arg(long)]
        concurrency: Option<usize>,

        /// Maximum number of simultaneous IMAP connections opened to any
        /// one identity, regardless of `--concurrency`. Defaults to 6 when
        /// omitted; not interactively prompted.
        #[arg(long)]
        max_connections_per_identity: Option<usize>,

        /// Skip the final "proceed?" confirmation.
        #[arg(long)]
        yes: bool,
    },

    /// Recursively pulls every object from a bucket-config, expands zips,
    /// recodes media into a size-optimized canonical format per category
    /// (photo/screenshot -> jpg, video -> mp4, audio -> m4a), dates and
    /// dedups everything by content, and organizes the result by extension
    /// -- then optionally encrypts and uploads it to a (possibly
    /// different) bucket-config (ADR-0074). Requires `ffmpeg`/`ffprobe` on
    /// `PATH`.
    PullTransform {
        /// Alias of a configured bucket-config (see `pigeon keyring add
        /// bucket`) to pull from. Interactively selected from the
        /// configured bucket-configs when omitted and stdin is a terminal;
        /// required otherwise.
        #[arg(long)]
        source_bucket: Option<String>,

        /// Local directory to stage and store output under. Defaults to a
        /// directory under the OS temp directory when omitted.
        #[arg(long)]
        local_output: Option<PathBuf>,

        /// Alias of a configured bucket-config to upload the organized
        /// result to, once local processing is complete.
        #[arg(long)]
        remote_output: Option<String>,

        /// Alias of a configured encryption key, overriding the target
        /// bucket-config's own default (if any).
        #[arg(long)]
        encryption_key: Option<String>,

        /// File extensions to pull/transform/upload, comma-separated (e.g.
        /// `jpg,mp4,pdf`; use the literal `none` for extensionless keys).
        /// Everything else is left pending, untouched, for a future run --
        /// never checkpointed as done (ADR-0077). Interactively selected
        /// (all pre-checked) from the pending-summary table when omitted
        /// and stdin is a terminal; defaults to everything otherwise.
        #[arg(long, value_delimiter = ',')]
        file_types: Option<Vec<String>>,

        /// Keys of pending zip objects to expand and transform;
        /// comma-separated. Every other pending zip is uploaded as-is,
        /// untouched (ADR-0077). Interactively selected (all pre-checked)
        /// when omitted and stdin is a terminal; defaults to expanding
        /// every pending zip otherwise.
        #[arg(long, value_delimiter = ',')]
        expand_zips: Option<Vec<String>>,

        /// Recode target for photos/screenshots: `jpg` (default) or `png`.
        #[arg(long)]
        image_format: Option<String>,

        /// Recode target for video: `mp4` (default), `mkv`, or `webm`.
        #[arg(long)]
        video_format: Option<String>,

        /// Recode target for audio: `m4a` (default), `mp3`, or `flac`.
        #[arg(long)]
        audio_format: Option<String>,

        /// Maximum number of files to download/recode concurrently.
        #[arg(long)]
        concurrency: Option<usize>,

        /// Skip the final "proceed?" confirmation.
        #[arg(long)]
        yes: bool,
    },

    /// Recursively scans a bucket, always inflates every zip found (the
    /// zip container itself is never uploaded, only its inflated
    /// contents), content-hashes every file bucket-wide to keep one
    /// byte-identical copy of each, writes a human-readable merge report,
    /// and optionally uploads the result unencrypted to a (possibly
    /// different) bucket-config (ADR-0082). Unlike `pull-transform`, every
    /// file is always processed and every zip is always expanded -- there
    /// is no file-type or zip-expansion selection, and this job never
    /// offers encryption.
    Dedupe {
        /// Alias of a configured bucket-config to pull from. Interactively
        /// selected from the configured bucket-configs when omitted and
        /// stdin is a terminal; required otherwise.
        #[arg(long)]
        source_bucket: Option<String>,

        /// Local directory to stage and store output under. Defaults to a
        /// directory under the OS temp directory when omitted.
        #[arg(long)]
        local_output: Option<PathBuf>,

        /// Alias of a configured bucket-config to upload the deduped
        /// result to, once local processing is complete. Always uploaded
        /// unencrypted.
        #[arg(long)]
        remote_output: Option<String>,

        /// Maximum number of files to download/hash concurrently.
        #[arg(long)]
        concurrency: Option<usize>,

        /// Skip the final "proceed?" confirmation.
        #[arg(long)]
        yes: bool,
    },

    /// Downloads every object from a source bucket and flattens it into
    /// top-level `<extension>/` folders by each file's literal, as-found
    /// extension (no canonicalization -- `.jpg` and `.jpeg` stay
    /// separate), then uploads the result unencrypted to a mandatory
    /// output bucket (ADR-0083). A filename collision is always
    /// disambiguated, never hash-checked or merged -- this job assumes
    /// uniqueness was already established by whatever produced the
    /// source bucket's contents, e.g. a prior `job run dedupe`. Never
    /// offers encryption, never expands zips, never deduplicates.
    Sort {
        /// Alias of a configured bucket-config to pull from. Interactively
        /// selected from the configured bucket-configs when omitted and
        /// stdin is a terminal; required otherwise.
        #[arg(long)]
        source_bucket: Option<String>,

        /// Local directory to stage and store output under. Defaults to a
        /// directory under the OS temp directory when omitted.
        #[arg(long)]
        local_output: Option<PathBuf>,

        /// Alias of a configured bucket-config to upload the flattened
        /// result to. Always uploaded unencrypted. Interactively selected
        /// from the configured bucket-configs when omitted and stdin is a
        /// terminal; required otherwise -- uploading is mandatory for
        /// this job.
        #[arg(long)]
        remote_output: Option<String>,

        /// Maximum number of files to download concurrently.
        #[arg(long)]
        concurrency: Option<usize>,

        /// Skip the final "proceed?" confirmation.
        #[arg(long)]
        yes: bool,
    },
}

impl Observable for JobType {
    fn command_name(&self) -> &'static str {
        match self {
            JobType::EmailSync { .. } => "job.email-sync",
            JobType::DecryptFiles { .. } => "job.decrypt-files",
            JobType::EmailPull { .. } => "job.email-pull",
            JobType::PullTransform { .. } => "job.pull-transform",
            JobType::Dedupe { .. } => "job.dedupe",
            JobType::Sort { .. } => "job.sort",
        }
    }
}
