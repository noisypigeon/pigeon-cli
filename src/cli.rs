use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::provider::Provider;

/// Pigeon: authenticate, sink, and transform personal data from external services.
#[derive(Parser, Debug)]
#[command(name = "pigeon", version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Authenticate, sink, and transform email identities and their data
    Email(EmailArgs),
}

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
    ListIdentities,

    /// Download and transform all mail for an authenticated identity in one step
    Sync {
        /// Alias of the identity to sync, as registered via `authenticate`.
        /// Interactively selected from the authenticated identities when omitted.
        alias: Option<String>,

        /// Staging directory for raw .eml files. Transient by default: each
        /// message is deleted once its transform is verified. Only persists
        /// when --debug sink is used and the default flow is never run
        /// against it afterward.
        #[arg(long)]
        staging_dir: PathBuf,

        /// Destination directory for transformed Markdown output
        #[arg(long)]
        output_dir: PathBuf,

        /// Run only one phase, exactly as it behaved standalone before this
        /// command existed: "sink" fetches without transforming; "transform"
        /// transforms without fetching. Both are non-destructive (never
        /// delete the source .eml).
        #[arg(long)]
        debug: Option<DebugPhase>,
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
