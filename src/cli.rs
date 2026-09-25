use clap::{Parser, Subcommand};

use crate::dataops::cli::DataopsArgs;
use crate::email::cli::EmailArgs;
use crate::job::cli::JobArgs;

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
    /// Manage S3-compatible bucket configurations
    Dataops(DataopsArgs),
    /// Run job-orchestrated pipelines (e.g. email-sync)
    Job(JobArgs),
}
