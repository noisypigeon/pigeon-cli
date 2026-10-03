use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::commands::job::cli::JobArgs;
use crate::commands::keyring::cli::KeyringArgs;

/// Pigeon: authenticate, sink, and transform personal data from external services.
#[derive(Parser, Debug)]
#[command(name = "pigeon", version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// Tracing filter directive for the durable JSONL log (e.g. "info" or
    /// "pigeon=debug"). Defaults to $RUST_LOG, or "warn,pigeon=info" if that
    /// isn't set either (ADR-0073).
    #[arg(long, global = true)]
    pub log_level: Option<String>,

    /// Overrides where the durable JSONL log is written. Defaults to
    /// $PIGEON_LOG_DIR/pigeon.jsonl, or the OS-conventional local-data
    /// directory for `pigeon` if that isn't set either (ADR-0073).
    #[arg(long, global = true)]
    pub log_file: Option<PathBuf>,

    /// Port for the local Prometheus metrics endpoint an on-host
    /// observability agent (e.g. Grafana Alloy) can scrape (ADR-0092).
    /// Defaults to $PIGEON_METRICS_PORT, or 9091 if that isn't set either.
    #[arg(long, global = true)]
    pub metrics_port: Option<u16>,

    /// Disables the local Prometheus metrics endpoint entirely (ADR-0092).
    #[arg(long, global = true)]
    pub no_metrics: bool,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Manage email identities and bucket-configs
    Keyring(KeyringArgs),
    /// Run job-orchestrated pipelines (e.g. email-sync)
    Job(JobArgs),
}
