use crate::cli::Commands;

/// Exit code returned by a command that ran but didn't succeed (e.g. a
/// failed IMAP login, an unreadable config file). Distinct from clap's own
/// usage-error exit code (2), so "input rejected" and "input accepted but
/// the operation failed" stay distinguishable.
pub const FAILURE_EXIT_CODE: i32 = 1;

pub fn dispatch(command: Commands) -> i32 {
    match command {
        Commands::Email(args) => crate::email::commands::dispatch(args.command),
        Commands::Remote(args) => crate::remote::commands::dispatch(args.command),
    }
}
