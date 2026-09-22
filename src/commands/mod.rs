pub mod email;

use crate::cli::Commands;

/// Exit code returned by any not-yet-implemented stub handler. Distinct from
/// clap's own usage-error exit code (2), so "input rejected" and "feature not
/// implemented" stay distinguishable.
pub const NOT_YET_IMPLEMENTED_EXIT_CODE: i32 = 1;

pub fn dispatch(command: Commands) -> i32 {
    match command {
        Commands::Email(args) => email::dispatch(args.command),
    }
}
