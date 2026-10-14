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
    /// Email jobs addressed through the keyring. See subcommands for the
    /// available actions.
    Email(EmailArgs),

    /// Recursively scans a bucket, always inflates every zip found (the
    /// zip container itself is never uploaded, only its inflated
    /// contents), content-hashes every file bucket-wide to keep one
    /// byte-identical copy of each, writes a human-readable merge report,
    /// and optionally uploads the result unencrypted to a (possibly
    /// different) bucket-config (ADR-0082). Every file is always processed
    /// and every zip is always expanded -- there is no file-type or
    /// zip-expansion selection, and this job never offers encryption.
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

        /// rclone --transfers for the upload phase, now one bulk `rclone
        /// copy` rather than a per-file task queue (ADR-0136). Defaults to
        /// 8 when omitted. With `--upload-only`, this is the only transfer
        /// concurrency flag that has any effect.
        #[arg(long)]
        transfers: Option<usize>,

        /// rclone --checkers for the upload phase. Defaults to 16 when
        /// omitted.
        #[arg(long)]
        checkers: Option<usize>,

        /// rclone --tpslimit for the upload phase. Unset by default -- no
        /// rate cap.
        #[arg(long)]
        tpslimit: Option<usize>,

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

        /// Suppress every wizard prompt (ADR-0114): each omitted input
        /// resolves via its non-interactive fallback (or errors if none
        /// exists) instead of asking, and the final "proceed?"
        /// confirmation is auto-answered yes.
        #[arg(long)]
        non_interactive: bool,
    },

    /// Shells out to the external `rclone` binary for copy/delete
    /// operations against pigeon keyring `BucketConfig` aliases. `rclone`
    /// is always invoked against a config file pigeon itself generates
    /// from the keyring (ADR-0136, reversing ADR-0101/ADR-0110's original
    /// raw-passthrough design) -- never an externally-provisioned or
    /// user-general-purpose `rclone.conf`. See subcommands for the
    /// available actions. Requires `rclone` on `PATH`.
    Rclone(RcloneArgs),

    /// Transcodes image files (`png`/`jpeg`/`heic`) into full-size,
    /// maximum-quality `.jpg`, video files (`mov`/`m4v`/`mp4`, ADR-0122)
    /// into space-optimized H.265/HEVC `.mp4` at a chosen `--video-quality`,
    /// audio files -- an audio-only recording carrying an `.mp4` extension
    /// (`mp4-audio`, ADR-0142) or `m4a`/`mp3`/`aac`/`wav`/`opus`
    /// (ADR-0153) -- into Opus `.opus` at a chosen `--audio-quality`, or
    /// spreadsheets
    /// (`numbers`/`xlsx`/`xls`/`csv`, ADR-0149) into one `.csv` per sheet,
    /// or documents (ADR-0154) -- `doc`/`docx`/`pages`/`rtf`/`pptx`/`key`/
    /// `epub` into `.pdf` with `pdf` copied through,
    /// `txt`/`md`/`markdown`/`mkd`/`rst`/`textclipping` into `.txt`, and
    /// `tsv` into `.csv`
    /// (ADR-0112, pipeline reworked by ADR-0116): shells out to `rclone` to
    /// pull `--source` into a local staging tree, transcoding/copying each
    /// file locally and pushing it individually via `rclone copyto` the
    /// moment it's ready, rather than waiting for the whole batch. A
    /// multi-sheet workbook is the one input kind that yields several
    /// outputs, each named `{stem}-{hash}-{nn}of{tt}-{sheet}.csv` so they
    /// stay recognizably related and a half-pushed workbook is detectable
    /// on a rerun (ADR-0149).
    /// `--source`/`--destination` are pigeon keyring `BucketConfig`
    /// aliases, optionally composed with a prefix within that bucket
    /// (`alias:prefix/`, ADR-0138); an omitted prefix defaults to
    /// `--input-file-type`'s own extension (source) or output extension
    /// (destination). Deduplication is never involved -- every output file's
    /// name is derived deterministically and is unique by construction;
    /// duplicate source content is deliberately retained at separate
    /// destination names. A single file's transcode or push failure (e.g.
    /// a HEIC file the local `ffmpeg` build can't decode, retried before
    /// giving up) is recorded and reported but never stops any other file
    /// from being processed and pushed. A rerun is resumable without any
    /// local state: "already done" is answered by listing the destination
    /// (ADR-0120 removed the local checkpoint). Requires `rclone` on
    /// `PATH`, plus `ffmpeg`/`ffprobe` for an image, video, or audio input
    /// kind, `soffice` (LibreOffice) for `numbers` and for
    /// `doc`/`docx`/`pages`/`rtf`/`pptx`/`key`, `ebook-convert` (Calibre)
    /// for `epub`, and `pandoc` for `md`/`rst`;
    /// `xlsx`/`xls`/`csv`/`tsv`/`pdf`/`txt`/`textclipping` need none of
    /// them (ADR-0149, ADR-0154).
    Transform {
        /// Which input format this run transcodes: `png`, `jpeg`, `heic`,
        /// `mov`, `m4v`, `mp4`, `mp4-audio`, `m4a`, `mp3`, `aac`, `wav`,
        /// `opus`, `numbers`, `xlsx`, `xls`, `csv`, `doc`, `docx`,
        /// `pages`, `rtf`, `pdf`, `epub`, `pptx`, `key`, `txt`, `md`
        /// (or `markdown`/`mkd`), `rst`, `textclipping`, or `tsv`.
        /// Interactively selected when omitted and stdin is a terminal;
        /// required otherwise.
        /// `mp4-audio` is for audio-only recordings carrying an `.mp4`
        /// extension (voice notes), transcoded to `.opus`; a file that
        /// actually has a video stream is refused rather than silently
        /// losing its video -- embedded cover art does not count as video
        /// (ADR-0153). `opus` input is re-encoded rather than copied
        /// through, so a mixed library normalizes to one bitrate. The four spreadsheet kinds all output `.csv`,
        /// one file per non-empty sheet (ADR-0149); `csv` input is copied
        /// through byte-for-byte, as are `pdf` and `txt` (ADR-0154).
        /// `key` is sniffed before conversion and a file that is not an
        /// iWork container is refused -- that extension is also
        /// conventional for a private key, and LibreOffice would otherwise
        /// render one into a readable PDF. `md`/`rst` are rendered to
        /// prose, which **discards link URLs and all inline markup**.
        #[arg(long)]
        input_file_type: Option<String>,

        /// Compression level for a true video input kind (`mov`/`m4v`/`mp4`
        /// -- not `mp4-audio`, which has no video stream):
        /// `low`, `medium` (default), `high`, or `lossless` -- a small
        /// vetted `libx265` CRF menu (ADR-0122). Ignored for every other
        /// input kind. Interactively selected when omitted, stdin is a
        /// terminal, and the input kind is video; defaults to `medium`
        /// otherwise.
        #[arg(long)]
        video_quality: Option<String>,

        /// Opus bitrate for an audio input kind (`mp4-audio`, `m4a`,
        /// `mp3`, `aac`, `wav`, `opus`): `low` (32k, speech), `medium`
        /// (48k), `high` (96k), or `transparent` (128k) -- a small vetted
        /// menu (ADR-0153), never a raw bitrate. Ignored for every other
        /// input kind. Interactively selected when omitted, stdin is a
        /// terminal, and the input kind is audio; otherwise it defaults
        /// **per kind**: `medium` for `mp4-audio`, preserving ADR-0142's
        /// choice for voice notes, and `transparent` for the rest, which
        /// are music or (for `wav`) lossless and would be badly served by
        /// a speech bitrate.
        #[arg(long)]
        audio_quality: Option<String>,

        /// Where to pull from: a configured bucket-config alias, optionally
        /// composed with a prefix inside it -- `media` is the whole bucket,
        /// `media:mp4/` its `mp4/` directory (ADR-0138). An omitted prefix
        /// defaults to `--input-file-type`'s own extension, so
        /// `--source media --input-file-type mp4` means `media:mp4/`; an
        /// explicit prefix is always used verbatim. Interactively selected
        /// (bucket root, then the default prefix) when omitted and stdin is
        /// a terminal; required otherwise.
        #[arg(long)]
        source: Option<String>,

        /// Where to push to, same `alias[:prefix/]` form as `--source`
        /// (ADR-0138). An omitted prefix defaults to `--input-file-type`'s
        /// *output* extension (`jpg` for an image kind, `mp4` for a video
        /// one, `opus` for every audio kind, `csv` for every spreadsheet
        /// kind),
        /// so one bucket can serve as both source and destination when the
        /// prefixes differ.
        #[arg(long)]
        destination: Option<String>,

        /// Local directory this run stages pulled source files, transcoded
        /// results, and bookkeeping under. Defaults to a directory under
        /// the OS temp directory when omitted.
        #[arg(long)]
        local_output: Option<PathBuf>,

        /// Maximum number of files to convert concurrently (the local
        /// conversion phase only -- `rclone` manages its own parallelism via
        /// `--download-transfers`/`--download-checkers`). Interactively
        /// prompted (a cores-based default) when omitted and stdin is a
        /// terminal; required otherwise.
        #[arg(long)]
        concurrency: Option<usize>,

        /// rclone --transfers for the download (pull) phase. Defaults to 8
        /// when omitted.
        #[arg(long)]
        download_transfers: Option<usize>,

        /// rclone --checkers for the download (pull) phase. Defaults to 16
        /// when omitted.
        #[arg(long)]
        download_checkers: Option<usize>,

        /// Maximum number of concurrent per-file push (`rclone copyto`)
        /// operations. Defaults to 8 when omitted. Separate from
        /// `--download-transfers` to allow independent tuning of pull vs.
        /// push phases (ADR-0129).
        #[arg(long)]
        upload_transfers: Option<usize>,

        /// Maximum number of downloaded but not-yet-processed files
        /// (pending transcode/push) before the pull phase is paused to allow
        /// the pipeline to catch up. Defaults to `concurrency * 4` when
        /// omitted. Enables automatic disk-pressure backpressure without
        /// requiring manual tuning (ADR-0129).
        #[arg(long)]
        max_pending_downloads: Option<usize>,

        /// rclone --tpslimit, used by the pull phase only. Unset by
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

        /// Suppress every wizard prompt (ADR-0114): each omitted input
        /// resolves via its non-interactive fallback (or errors if none
        /// exists) instead of asking, and the final "proceed?"
        /// confirmation is auto-answered yes.
        #[arg(long)]
        non_interactive: bool,
    },
}

#[derive(Args, Debug)]
pub struct EmailArgs {
    #[command(subcommand)]
    pub action: EmailAction,
}

#[derive(Subcommand, Debug)]
pub enum EmailAction {
    /// Reads one or more authenticated IMAP identities and delivers each
    /// message's raw `.eml` plus its unpacked attachments to
    /// `--destination`, at a declared, rate-limit-respecting pace
    /// (ADR-0146). Deliberately minimal: no deduplication, no indexing, no
    /// renaming, no content transform, and no encryption -- it moves bytes
    /// and nothing else. Each message is pushed individually via `rclone
    /// copyto` the moment it is fetched and its local copy deleted
    /// immediately afterwards, so a run lasting days delivers
    /// incrementally and local disk stays bounded to the in-flight window
    /// regardless of how large the account is. A message that fails is
    /// simply not checkpointed, so a rerun refetches it; nothing is
    /// retained locally for diagnosis, because the server still has it.
    /// Requires `rclone` on `PATH`.
    Import {
        /// Aliases of the identities to import, comma-separated.
        /// Interactively multi-selected from the authenticated identities
        /// when omitted and stdin is a terminal; required otherwise.
        #[arg(long, value_delimiter = ',')]
        identities: Option<Vec<String>>,

        /// Where to deliver to: a configured bucket-config alias,
        /// optionally composed with a prefix inside it -- `archive` is the
        /// whole bucket, `archive:eml/` its `eml/` directory (ADR-0138).
        /// Unlike `transform`, an omitted prefix stays the bucket root
        /// (this job has no file-type concept to derive one from).
        /// Mandatory -- interactively selected when omitted and stdin is a
        /// terminal; required otherwise.
        #[arg(long)]
        destination: Option<String>,

        /// Local directory in-flight messages and this run's bookkeeping
        /// are staged under. Defaults to a directory under the OS temp
        /// directory when omitted. Unlike the other email jobs this is not
        /// an output tree: each message's local copy is deleted as soon as
        /// its push resolves (ADR-0146 §5), so it stays near-empty.
        #[arg(long)]
        local_output: Option<PathBuf>,

        /// Ceiling on how many messages per minute are fetched, across
        /// every selected identity -- the run's throughput governor
        /// (ADR-0146 §4). Deliberately has no "unlimited" value: the
        /// absence of a cap is what this job exists to fix. Interactively
        /// prompted when omitted and stdin is a terminal; defaults to 120
        /// otherwise, which puts a 200,000-message account at roughly 28
        /// hours.
        #[arg(long)]
        messages_per_minute: Option<usize>,

        /// How many UIDs one `UID FETCH` asks for at a time. Small enough
        /// that a failed batch loses little work, large enough to amortise
        /// the round trip. Interactively prompted when omitted and stdin is
        /// a terminal; defaults to 20 otherwise.
        #[arg(long)]
        batch_size: Option<usize>,

        /// Maximum simultaneous IMAP connections opened to any one
        /// identity. A per-identity cap set via `pigeon keyring
        /// add/modify email` still overrides this for that identity only
        /// (ADR-0080). Interactively prompted when omitted and stdin is a
        /// terminal; defaults to 2 otherwise -- well below the other email
        /// jobs' 6, since throughput here comes from
        /// `--messages-per-minute` rather than from parallelism.
        #[arg(long)]
        max_connections: Option<usize>,

        /// Maximum concurrent per-object pushes (`rclone copyto`).
        /// Interactively prompted when omitted and stdin is a terminal;
        /// defaults to 4 otherwise -- deliberately below the other jobs'
        /// upload concurrency, since these pushes interleave with a
        /// throttled fetch rather than racing to drain a finished queue.
        #[arg(long)]
        push_concurrency: Option<usize>,

        /// Alias of a configured bucket-config this run's report, the
        /// shared observability log, and a transcript of its printed
        /// output are uploaded to, always unencrypted, under a
        /// `YYYY-MM-DD-job-name-{run-id}/` prefix (ADR-0100). Mandatory --
        /// interactively selected when omitted and stdin is a terminal;
        /// required otherwise.
        #[arg(long)]
        report_bucket: Option<String>,

        /// Suppress every wizard prompt (ADR-0114): each omitted input
        /// resolves via its non-interactive fallback (or errors if none
        /// exists) instead of asking, and the final "proceed?"
        /// confirmation is auto-answered yes.
        #[arg(long)]
        non_interactive: bool,
    },
}

#[derive(Args, Debug)]
pub struct RcloneArgs {
    #[command(subcommand)]
    pub action: RcloneAction,
}

#[derive(Subcommand, Debug)]
pub enum RcloneAction {
    /// Copies data from one configured bucket-config to another by
    /// shelling out to `rclone copy`, against a config file pigeon itself
    /// generates from the keyring (ADR-0136, reversing ADR-0101's original
    /// raw-passthrough design). `--transfers`/`--checkers`/`--tpslimit` are
    /// overridable per-destination (ADR-0108); every other rclone
    /// performance/retry flag stays fixed.
    Copy {
        /// Where to pull from: a configured bucket-config alias, optionally
        /// composed with a prefix inside it -- `media` is the whole bucket,
        /// `media:mp4/` its `mp4/` directory (ADR-0138). Unlike
        /// `transform`, an omitted prefix stays the bucket root (this job
        /// has no file-type concept to derive one from). Interactively
        /// selected when omitted and stdin is a terminal; required
        /// otherwise.
        #[arg(long)]
        source_bucket: Option<String>,

        /// Where to push to, same `alias[:prefix/]` form as
        /// `--source-bucket` (ADR-0138). Interactively selected when
        /// omitted and stdin is a terminal; required otherwise.
        #[arg(long)]
        destination_bucket: Option<String>,

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

        /// Suppress every wizard prompt (ADR-0114): each omitted input
        /// resolves via its non-interactive fallback (or errors if none
        /// exists) instead of asking, and the final "proceed?"
        /// confirmation is auto-answered yes.
        #[arg(long)]
        non_interactive: bool,
    },

    /// Recursively and permanently deletes everything in a configured
    /// bucket-config by shelling out to `rclone purge` (ADR-0110).
    /// Irreversible -- there is no `--dry-run` and no undo beyond the
    /// existing `--non-interactive`/interactive confirmation. Unlike
    /// `copy`, there is no destination and no per-file transfer-rate
    /// tuning, since purge transfers no file content.
    Delete {
        /// What to recursively and permanently delete the contents of: a
        /// configured bucket-config alias, optionally composed with a
        /// prefix to scope the purge to one directory inside it
        /// (`media:mp4/`, ADR-0138). An omitted prefix purges the **whole
        /// bucket**. Interactively selected when omitted and stdin is a
        /// terminal; required otherwise.
        #[arg(long)]
        source_bucket: Option<String>,

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

        /// Suppress every wizard prompt (ADR-0114): each omitted input
        /// resolves via its non-interactive fallback (or errors if none
        /// exists) instead of asking, and the final "proceed?"
        /// confirmation is auto-answered yes.
        #[arg(long)]
        non_interactive: bool,
    },
}

impl Observable for JobType {
    fn command_name(&self) -> &'static str {
        match self {
            JobType::Email(args) => match args.action {
                EmailAction::Import { .. } => "job.email-import",
            },
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
