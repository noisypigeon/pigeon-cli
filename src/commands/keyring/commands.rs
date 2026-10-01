use crate::commands::keyring::cli::KeyringCommands;
use crate::commands::keyring::wizard;
use crate::core::observability::Observable as _;

pub fn dispatch(command: KeyringCommands) -> i32 {
    let name = command.command_name();
    crate::observability::run_instrumented(name, move || match command {
        KeyringCommands::Add(args) => wizard::add(args.kind),
        KeyringCommands::Modify { alias } => wizard::modify(alias),
        KeyringCommands::Delete { alias } => wizard::delete(alias),
        KeyringCommands::List => wizard::list(),
    })
}
