use crate::commands::keyring::cli::KeyringCommands;
use crate::commands::keyring::wizard;

pub fn dispatch(command: KeyringCommands) -> i32 {
    match command {
        KeyringCommands::Add(args) => wizard::add(args.kind),
        KeyringCommands::Modify { alias } => wizard::modify(alias),
        KeyringCommands::Delete { alias } => wizard::delete(alias),
        KeyringCommands::List => wizard::list(),
    }
}
