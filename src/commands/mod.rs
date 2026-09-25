use crate::cli::Commands;

/// Exit code returned by a command that ran but didn't succeed (e.g. a
/// failed IMAP login, an unreadable config file). Distinct from clap's own
/// usage-error exit code (2), so "input rejected" and "input accepted but
/// the operation failed" stay distinguishable.
pub const FAILURE_EXIT_CODE: i32 = 1;

pub fn dispatch(command: Commands) -> i32 {
    match command {
        Commands::Email(args) => crate::email::commands::dispatch(args.command),
        Commands::Dataops(args) => crate::dataops::commands::dispatch(args.command),
        Commands::Job(args) => crate::job::commands::dispatch(args.command),
    }
}

/// Prints `rows` as a left-aligned table with a header row, columns padded
/// to their widest cell (except the last, which is never padded) and joined
/// by two spaces -- shared by `email list`/`remote list`. Hand-rolled, no
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
