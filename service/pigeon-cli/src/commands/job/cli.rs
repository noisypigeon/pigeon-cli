use std::path::PathBuf;

use clap::{Args, Subcommand};

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
}
