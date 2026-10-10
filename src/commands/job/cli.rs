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

        /// Maximum number of files to upload concurrently, independent of
        /// `--concurrency` (which sizes IMAP fetch/transform work) -- the
        /// upload phase is network-round-trip-bound, not IMAP-bound, so it
        /// benefits from its own, separately-tuned concurrency (ADR-0091).
        /// Interactively prompted when omitted and stdin is a terminal;
        /// defaults to 16 otherwise. With `--upload-only`, this is the only
        /// concurrency flag that has any effect.
        #[arg(long)]
        upload_concurrency: Option<usize>,

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

        /// Resumes uploading already-completed local runs for the selected
        /// identities instead of starting new ones: skips the IMAP
        /// connect/fetch/transform/dedup phases entirely (and the
        /// per-identity IMAP credentials they'd otherwise need) and
        /// uploads straight from each identity's existing local result
        /// tree, picking up where a prior run's upload phase left off via
        /// the same `.staging/.uploaded` index per identity (ADR-0090). An
        /// identity with no completed local run is skipped with a warning
        /// rather than failing the whole command. Requires a mandatory
        /// `--remote-output`.
        #[arg(long)]
        upload_only: bool,

        /// Alias of a configured bucket-config this run's report, the
        /// shared observability log, and a transcript of its printed
        /// output are uploaded to, always unencrypted, under a
        /// `YYYY-MM-DD-job-name-{run-id}/` prefix (ADR-0100). Mandatory --
        /// interactively selected when omitted and stdin is a terminal;
        /// required otherwise.
        #[arg(long)]
        report_bucket: Option<String>,

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

        /// Alias of a configured bucket-config this run's report, the
        /// shared observability log, and a transcript of its printed
        /// output are uploaded to, always unencrypted, under a
        /// `YYYY-MM-DD-job-name-{run-id}/` prefix (ADR-0100). Mandatory --
        /// interactively selected when omitted and stdin is a terminal;
        /// required otherwise.
        #[arg(long)]
        report_bucket: Option<String>,

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

        /// Maximum number of files to upload concurrently, independent of
        /// `--concurrency` (which sizes IMAP fetch/extract work) -- the
        /// upload phase is network-round-trip-bound, not IMAP-bound, so it
        /// benefits from its own, separately-tuned concurrency (ADR-0091).
        /// Interactively prompted when omitted and stdin is a terminal;
        /// defaults to 16 otherwise. With `--upload-only`, this is the only
        /// concurrency flag that has any effect.
        #[arg(long)]
        upload_concurrency: Option<usize>,

        /// Maximum number of simultaneous IMAP connections opened to any
        /// one identity, regardless of `--concurrency`. Defaults to 6 when
        /// omitted; not interactively prompted.
        #[arg(long)]
        max_connections_per_identity: Option<usize>,

        /// Resumes uploading already-completed local runs for the selected
        /// identities instead of starting new ones: skips the IMAP
        /// connect/fetch/dedup phases entirely (and the per-identity IMAP
        /// credentials they'd otherwise need) and uploads straight from
        /// each identity's existing local result tree, picking up where a
        /// prior run's upload phase left off via the same
        /// `.staging/.uploaded` index per identity (ADR-0090). An identity
        /// with no completed local run is skipped with a warning rather
        /// than failing the whole command. Requires a mandatory
        /// `--remote-output`.
        #[arg(long)]
        upload_only: bool,

        /// Alias of a configured bucket-config this run's report, the
        /// shared observability log, and a transcript of its printed
        /// output are uploaded to, always unencrypted, under a
        /// `YYYY-MM-DD-job-name-{run-id}/` prefix (ADR-0100). Mandatory --
        /// interactively selected when omitted and stdin is a terminal;
        /// required otherwise.
        #[arg(long)]
        report_bucket: Option<String>,

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

        /// Maximum number of files to upload concurrently, independent of
        /// `--concurrency` (which sizes download/recode work) -- the upload
        /// phase is network-round-trip-bound, not CPU-bound, so it benefits
        /// from its own, separately-tuned concurrency (ADR-0091).
        /// Interactively prompted when omitted and stdin is a terminal;
        /// defaults to 16 otherwise. With `--upload-only`, this is the only
        /// concurrency flag that has any effect.
        #[arg(long)]
        upload_concurrency: Option<usize>,

        /// Resumes uploading an already-completed local pull-transform run
        /// instead of starting a new one: skips the bucket listing/
        /// download/classify/recode/placement phases entirely (and the
        /// source bucket credentials and `ffmpeg`/`ffprobe` check they'd
        /// otherwise need) and uploads straight from an existing
        /// `--local-output`, picking up where a prior run's upload phase
        /// left off via the same `.staging/.uploaded` index (ADR-0090).
        /// Requires a `--local-output` from a completed prior run (its
        /// `.processed` checkpoint must exist and it must hold at least
        /// one placed-content subdirectory) and a mandatory
        /// `--remote-output`.
        #[arg(long)]
        upload_only: bool,

        /// Alias of a configured bucket-config this run's report, the
        /// shared observability log, and a transcript of its printed
        /// output are uploaded to, always unencrypted, under a
        /// `YYYY-MM-DD-job-name-{run-id}/` prefix (ADR-0100). Mandatory --
        /// interactively selected when omitted and stdin is a terminal;
        /// required otherwise.
        #[arg(long)]
        report_bucket: Option<String>,

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
    Deduplicate {
        /// Alias of a configured bucket-config to pull from. Repeat the
        /// flag for more than one (`--source-bucket a --source-bucket b`)
        /// -- every listed bucket is downloaded into one shared local
        /// staging tree and deduplicated across the combined set, not
        /// per-bucket (ADR-0109). Interactively multi-selected from the
        /// configured bucket-configs when omitted and stdin is a terminal;
        /// at least one is required otherwise.
        #[arg(long)]
        source_bucket: Vec<String>,

        /// Local directory to stage and store output under. Defaults to a
        /// directory under the OS temp directory when omitted.
        #[arg(long)]
        local_output: Option<PathBuf>,

        /// Alias of a configured bucket-config to upload the deduped
        /// result to, once local processing is complete. Always uploaded
        /// unencrypted. Single bucket only (ADR-0109).
        #[arg(long)]
        destination_bucket: Option<String>,

        /// Maximum number of files to download/hash concurrently.
        #[arg(long)]
        concurrency: Option<usize>,

        /// Maximum number of files to upload concurrently, independent of
        /// `--concurrency` (which sizes download/hash work) -- the upload
        /// phase is network-round-trip-bound, not CPU-bound, so it benefits
        /// from its own, separately-tuned concurrency (ADR-0091).
        /// Interactively prompted when omitted and stdin is a terminal;
        /// defaults to 16 otherwise. With `--upload-only`, this is the only
        /// concurrency flag that has any effect.
        #[arg(long)]
        upload_concurrency: Option<usize>,

        /// Resumes uploading an already-completed local deduplicate run instead
        /// of starting a new one: skips the bucket listing/download/hash/
        /// placement phases entirely (and the source bucket credentials
        /// they'd otherwise need) and uploads straight from an existing
        /// `--local-output`'s `result/` tree, picking up where a prior
        /// run's upload phase left off via the same `.staging/.uploaded`
        /// index (ADR-0089). Requires a `--local-output` from a completed
        /// prior run (its `.staging/.processed` checkpoint must exist and
        /// its `result/` must be non-empty) and a mandatory
        /// `--remote-output`.
        #[arg(long)]
        upload_only: bool,

        /// Alias of a configured bucket-config this run's report, the
        /// shared observability log, and a transcript of its printed
        /// output are uploaded to, always unencrypted, under a
        /// `YYYY-MM-DD-job-name-{run-id}/` prefix (ADR-0100). Mandatory --
        /// interactively selected when omitted and stdin is a terminal;
        /// required otherwise.
        #[arg(long)]
        report_bucket: Option<String>,

        /// Skip the final "proceed?" confirmation.
        #[arg(long)]
        yes: bool,
    },

    /// Shells out to the external `rclone` binary for copy/delete
    /// operations against raw rclone `remote:path` strings. Pigeon manages
    /// no rclone credentials/config -- `rclone.conf` is provisioned by an
    /// external process, outside this crate's scope (ADR-0101, restructured
    /// by ADR-0110). See subcommands for the available actions. Requires
    /// `rclone` on `PATH`.
    Rclone(RcloneArgs),

    /// Transcodes image files (`png`/`jpeg`/`heic`) into full-size,
    /// maximum-quality `.jpg` (ADR-0112): shells out to `rclone` to pull
    /// `--source-path` into a local staging tree, transcodes/copies them
    /// locally, then shells out to `rclone` again to push the result to
    /// `--destination-path`. `--source-path`/`--destination-path` are raw
    /// rclone `remote:path` strings, same semantics as the `rclone` job's
    /// own flags of the same name -- never pigeon `BucketConfig`/keyring
    /// aliases. Deduplication is never involved -- every output file's
    /// name is derived deterministically and is unique by construction;
    /// duplicate source content is deliberately retained at separate
    /// destination names. A single transcode failure (e.g. a HEIC file the
    /// local `ffmpeg` build can't decode) aborts the whole run, though
    /// every file placed before the failure stays checkpointed for a
    /// resumed rerun. Requires both `rclone` and `ffmpeg`/`ffprobe` on
    /// `PATH`.
    Transform {
        /// Which input format this run transcodes: `png`, `jpeg`, or
        /// `heic`. Interactively selected when omitted and stdin is a
        /// terminal; required otherwise.
        #[arg(long)]
        input_file_type: Option<String>,

        /// rclone source, e.g. `source:png/`. Interactively prompted when
        /// omitted and stdin is a terminal; required otherwise.
        #[arg(long)]
        source_path: Option<String>,

        /// rclone destination, e.g. `destination:jpg/`. Interactively
        /// prompted when omitted and stdin is a terminal; required
        /// otherwise.
        #[arg(long)]
        destination_path: Option<String>,

        /// Local directory this run stages pulled source files, transcoded
        /// results, and bookkeeping under. Defaults to a directory under
        /// the OS temp directory when omitted.
        #[arg(long)]
        local_output: Option<PathBuf>,

        /// Maximum number of files to transcode concurrently (the ffmpeg
        /// phase only -- `rclone` manages its own internal parallelism via
        /// `--transfers`/`--checkers`). Interactively prompted (a
        /// cores-based default) when omitted and stdin is a terminal;
        /// required otherwise.
        #[arg(long)]
        concurrency: Option<usize>,

        /// rclone --transfers, used by both the pull and push rclone
        /// invocations. Defaults to 8 when omitted.
        #[arg(long)]
        transfers: Option<usize>,

        /// rclone --checkers, used by both rclone invocations. Defaults to
        /// 16 when omitted.
        #[arg(long)]
        checkers: Option<usize>,

        /// rclone --tpslimit, used by both rclone invocations. Unset by
        /// default -- no rate cap.
        #[arg(long)]
        tpslimit: Option<usize>,

        /// Alias of a configured bucket-config this run's report, the
        /// shared observability log, and a transcript of its printed
        /// output are uploaded to, always unencrypted, under a
        /// `YYYY-MM-DD-job-name-{run-id}/` prefix (ADR-0100). Mandatory --
        /// interactively selected when omitted and stdin is a terminal;
        /// required otherwise.
        #[arg(long)]
        report_bucket: Option<String>,

        /// Skip the final "proceed?" confirmation.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Args, Debug)]
pub struct RcloneArgs {
    #[command(subcommand)]
    pub action: RcloneAction,
}

#[derive(Subcommand, Debug)]
pub enum RcloneAction {
    /// Copies data from a configurable source to a configurable
    /// destination by shelling out to `rclone copy`. `--source-path`/
    /// `--destination-path` are raw `remote:path` strings passed straight
    /// through to `rclone copy`'s argv, never pigeon `BucketConfig`/keyring
    /// aliases (ADR-0101). `--transfers`/`--checkers`/`--tpslimit` are
    /// overridable per-destination (ADR-0108); every other rclone
    /// performance/retry flag stays fixed.
    Copy {
        /// rclone source, e.g. `source:media/`. Interactively prompted
        /// when omitted and stdin is a terminal; required otherwise.
        #[arg(long)]
        source_path: Option<String>,

        /// rclone destination, e.g. `destination:`. Interactively
        /// prompted when omitted and stdin is a terminal; required
        /// otherwise.
        #[arg(long)]
        destination_path: Option<String>,

        /// Local directory this run's rclone log (also serving as this
        /// job's report) and transcript are written under. Defaults to a
        /// directory under the OS temp directory when omitted. Unlike
        /// every other job, this is not a staging area for transferred
        /// data -- rclone transfers directly source -> destination with no
        /// pigeon-side staging.
        #[arg(long)]
        local_output: Option<PathBuf>,

        /// Alias of a configured bucket-config this run's report (the
        /// rclone log itself, for this job), the shared observability log,
        /// and a transcript of its printed output are uploaded to, always
        /// unencrypted, under a `YYYY-MM-DD-job-name-{run-id}/` prefix
        /// (ADR-0100). Mandatory -- interactively selected when omitted
        /// and stdin is a terminal; required otherwise.
        #[arg(long)]
        report_bucket: Option<String>,

        /// rclone --transfers (concurrent file transfers). Defaults to 8
        /// (ADR-0106) when omitted.
        #[arg(long)]
        transfers: Option<usize>,

        /// rclone --checkers (concurrent list/compare operations).
        /// Defaults to 16 (ADR-0106) when omitted.
        #[arg(long)]
        checkers: Option<usize>,

        /// rclone --tpslimit (transactions/sec ceiling across transfers
        /// and checkers combined). Unset by default -- no rate cap --
        /// unless a specific destination needs one (ADR-0108).
        #[arg(long)]
        tpslimit: Option<usize>,

        /// Skip the final "proceed?" confirmation.
        #[arg(long)]
        yes: bool,
    },

    /// Recursively and permanently deletes everything under a configurable
    /// source path by shelling out to `rclone purge` (ADR-0110).
    /// Irreversible -- there is no `--dry-run` and no undo beyond the
    /// existing `--yes`/interactive confirmation. Unlike `copy`, there is
    /// no destination and no per-file transfer-rate tuning, since purge
    /// transfers no file content.
    Delete {
        /// rclone source to recursively and permanently delete, e.g.
        /// `source:media/`. Interactively prompted when omitted and stdin
        /// is a terminal; required otherwise.
        #[arg(long)]
        source_path: Option<String>,

        /// Local directory this run's rclone log (also serving as this
        /// job's report) and transcript are written under. Defaults to a
        /// directory under the OS temp directory when omitted.
        #[arg(long)]
        local_output: Option<PathBuf>,

        /// Alias of a configured bucket-config this run's report, the
        /// shared observability log, and a transcript of its printed
        /// output are uploaded to, always unencrypted, under a
        /// `YYYY-MM-DD-job-name-{run-id}/` prefix (ADR-0100). Mandatory --
        /// interactively selected when omitted and stdin is a terminal;
        /// required otherwise.
        #[arg(long)]
        report_bucket: Option<String>,

        /// rclone --checkers (concurrent list operations during
        /// enumeration). Defaults to 16 when omitted.
        #[arg(long)]
        checkers: Option<usize>,

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
            JobType::Deduplicate { .. } => "job.deduplicate",
            JobType::Rclone(args) => match args.action {
                RcloneAction::Copy { .. } => "job.rclone-copy",
                RcloneAction::Delete { .. } => "job.rclone-delete",
            },
            JobType::Transform { .. } => "job.transform",
        }
    }
}

impl JobType {
    /// The bare dash-form job name (e.g. `"deduplicate"`) used in the
    /// report-bucket upload prefix (ADR-0100) -- `command_name()`'s value
    /// with its `"job."` prefix stripped, rather than a 6th place in this
    /// codebase repeating the same job-name literals already duplicated
    /// across every job's `worker.rs` metrics call sites.
    pub(crate) fn job_name(&self) -> &'static str {
        self.command_name().trim_start_matches("job.")
    }
}
