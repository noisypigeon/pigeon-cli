use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

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

    /// Sink (download) all emails and attachments for an authenticated identity
    Sink {
        /// Alias of the identity to sink, as registered via `authenticate`.
        /// Interactively selected from the authenticated identities when omitted.
        alias: Option<String>,

        /// Destination directory for sunk emails and attachments
        #[arg(long)]
        directory: PathBuf,
    },

    /// Transform sunk EML files into normalized Markdown with frontmatter
    Transform {
        /// Alias of the identity to transform, as registered via `authenticate`.
        /// Interactively selected from the authenticated identities when omitted.
        alias: Option<String>,

        /// Source directory containing sunk email data (from `sink`)
        #[arg(long)]
        input: PathBuf,

        /// Destination directory for transformed Markdown output
        #[arg(long)]
        output: PathBuf,
    },
}
