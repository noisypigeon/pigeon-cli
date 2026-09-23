use std::path::PathBuf;

use clap::{Args, Subcommand, ValueEnum};

use crate::email::provider::Provider;

#[derive(Args, Debug)]
pub struct EmailArgs {
    #[command(subcommand)]
    pub command: EmailCommands,
}

#[derive(Subcommand, Debug)]
pub enum EmailCommands {
    /// Authenticate an email identity and register it under a local alias
    Authenticate {
        /// Email address of the identity to authenticate, e.g. first.last@example.com
        email: String,

        /// Local alias to store this identity under (e.g. first-last).
        /// Defaults to a sanitized form of the email's local part.
        #[arg(long)]
        alias: Option<String>,

        /// Email provider. Auto-detected from the email's domain when
        /// omitted, falling back to an interactive prompt if detection fails.
        #[arg(long)]
        provider: Option<Provider>,

        /// IMAP host, e.g. imap.example.com. Required when --provider is
        /// "custom" (or resolves to it); ignored for known providers, which
        /// use their own well-known host.
        #[arg(long)]
        host: Option<String>,

        /// IMAP port. Defaults to 993 for a custom provider when omitted;
        /// ignored for known providers, which use their own well-known port.
        #[arg(long)]
        port: Option<u16>,
    },

    /// List all locally authenticated email identities
    List,

    /// Download and transform all mail for an authenticated identity in one step
    Sync {
        /// Alias of the identity to sync, as registered via `authenticate`.
        /// Interactively selected from the authenticated identities when omitted.
        alias: Option<String>,

        /// Local directory to stage raw .eml files under `staging/` and
        /// write transformed Markdown output under `result/`. The staging
        /// side is transient by default: each message is deleted once its
        /// transform is verified. Only persists when --debug sink is used
        /// and the default flow is never run against it afterward. Defaults
        /// to a per-alias directory under the OS temp directory when omitted.
        #[arg(long)]
        local_output: Option<PathBuf>,

        /// Alias of a configured `pigeon remote` (see `remote configure`) to
        /// upload each synced message's Markdown and attachments to, in
        /// addition to --local-output. Rejected as a usage error when
        /// combined with --debug (sink/transform stay local-only).
        #[arg(long)]
        remote_output: Option<String>,

        /// Run only one phase, exactly as it behaved standalone before this
        /// command existed: "sink" fetches without transforming; "transform"
        /// transforms without fetching. Both are non-destructive (never
        /// delete the source .eml).
        #[arg(long)]
        debug: Option<DebugPhase>,

        /// Maximum number of mailboxes to process concurrently. Rejected in
        /// combination with --debug, which stays single-mailbox and
        /// sequential.
        #[arg(long, default_value_t = 4)]
        concurrency: usize,
    },
}

/// A single phase of `sync`, run in isolation via `--debug`.
#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum DebugPhase {
    /// Fetch-only: write raw .eml files, never transform or delete them.
    Sink,
    /// Transform-only: read existing .eml files, never fetch or delete them.
    Transform,
}
