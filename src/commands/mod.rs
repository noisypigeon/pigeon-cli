pub mod email;

use crate::cli::Commands;

/// Exit code returned by any not-yet-implemented stub handler. Distinct from
/// clap's own usage-error exit code (2), so "input rejected" and "feature not
/// implemented" stay distinguishable.
pub const NOT_YET_IMPLEMENTED_EXIT_CODE: i32 = 1;

/// Exit code returned by an implemented command that ran but didn't succeed
/// (e.g. a failed IMAP login, an unreadable config file). Deliberately the
/// same value as `NOT_YET_IMPLEMENTED_EXIT_CODE`: both mean "input accepted,
/// operation didn't succeed", as opposed to clap's usage-error code (2).
pub const FAILURE_EXIT_CODE: i32 = 1;

pub fn dispatch(command: Commands) -> i32 {
    match command {
        Commands::Email(args) => email::dispatch(args.command),
    }
}
