use clap::{Parser, Subcommand};

use crate::job::cli::JobArgs;
use crate::keyring::cli::KeyringArgs;

/// Pigeon: authenticate, sink, and transform personal data from external services.
#[derive(Parser, Debug)]
#[command(name = "pigeon", version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Manage email identities and bucket-configs
    Keyring(KeyringArgs),
    /// Run job-orchestrated pipelines (e.g. email-sync)
    Job(JobArgs),
}
