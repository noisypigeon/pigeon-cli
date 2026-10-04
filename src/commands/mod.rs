pub mod job;
pub mod keyring;

use crate::cli::Commands;

/// Exit code returned by a command that ran but didn't succeed (e.g. a
/// failed IMAP login, an unreadable config file). Distinct from clap's own
/// usage-error exit code (2), so "input rejected" and "input accepted but
/// the operation failed" stay distinguishable.
pub const FAILURE_EXIT_CODE: i32 = 1;

/// Prints `message` to stderr and returns `FAILURE_EXIT_CODE` -- the shared
/// landing point every job wizard's `dispatch_async` uses to turn an `Err`
/// into an exit code (ADR-0097). Previously each of the six job wizards
/// defined this identically but without the `tracing::error!` call, so a
/// handled job failure reached stderr but never the structured JSONL log
/// (issue #18). `run_instrumented` still logs its own "command finished"
/// line afterward with the exit code; this is the line that explains why.
pub(crate) fn fail(message: impl std::fmt::Display) -> i32 {
    tracing::error!(error = %message, "job failed");
    eprintln!("Error: {message}");
    FAILURE_EXIT_CODE
}

pub fn dispatch(command: Commands) -> i32 {
    match command {
        Commands::Keyring(args) => crate::commands::keyring::commands::dispatch(args.command),
        Commands::Job(args) => crate::commands::job::commands::dispatch(args.command),
    }
}

/// Prints `rows` as a left-aligned table with a header row, columns padded
/// to their widest cell (except the last, which is never padded) and joined
/// by two spaces -- shared by `keyring list`/`job` summaries. Hand-rolled, no
/// table-formatting crate.
pub fn print_table(headers: &[&str], rows: &[Vec<String>]) {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }
    let format_row = |cells: &[String]| -> String {
        cells
            .iter()
            .enumerate()
            .map(|(i, cell)| format!("{:width$}", cell, width = widths[i]))
            .collect::<Vec<_>>()
            .join("  ")
            .trim_end()
            .to_string()
    };
    let header_row: Vec<String> = headers.iter().map(|h| h.to_string()).collect();
    println!("{}", format_row(&header_row));
    for row in rows {
        println!("{}", format_row(row));
    }
}
