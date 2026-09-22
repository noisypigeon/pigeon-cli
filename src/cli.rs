use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

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

        /// Local alias to store this identity under (e.g. first-last)
        #[arg(long)]
        alias: Option<String>,
    },

    /// List all locally authenticated email identities
    ListIdentities,

    /// Sink (download) all emails and attachments for an authenticated identity
    Sink {
        /// Alias of the identity to sink, as registered via `authenticate`
        alias: String,

        /// Destination directory for sunk emails and attachments
        #[arg(long)]
        directory: PathBuf,
    },

    /// Transform sunk MBOX/EML files into normalized Markdown
    Transform {
        /// Source directory containing sunk email data
        #[arg(long)]
        input: PathBuf,

        /// Destination directory for transformed output
        #[arg(long)]
        output: PathBuf,

        /// Normalize file names and folder structure to the taxonomy scheme
        #[arg(long)]
        normalize: bool,

        /// Convert MBOX/EML files to Markdown
        #[arg(long)]
        mbox_to_markdown: bool,
    },
}
