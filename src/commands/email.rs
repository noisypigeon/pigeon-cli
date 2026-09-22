use std::path::PathBuf;

use super::NOT_YET_IMPLEMENTED_EXIT_CODE;
use crate::cli::EmailCommands;

pub fn dispatch(command: EmailCommands) -> i32 {
    match command {
        EmailCommands::Authenticate { email, alias } => authenticate(email, alias),
        EmailCommands::ListIdentities => list_identities(),
        EmailCommands::Sink { alias, directory } => sink(alias, directory),
        EmailCommands::Transform {
            input,
            output,
            normalize,
            mbox_to_markdown,
        } => transform(input, output, normalize, mbox_to_markdown),
    }
}

fn authenticate(_email: String, _alias: Option<String>) -> i32 {
    println!("Not Yet Implemented");
    NOT_YET_IMPLEMENTED_EXIT_CODE
}

fn list_identities() -> i32 {
    println!("Not Yet Implemented");
    NOT_YET_IMPLEMENTED_EXIT_CODE
}

fn sink(_alias: String, _directory: PathBuf) -> i32 {
    println!("Not Yet Implemented");
    NOT_YET_IMPLEMENTED_EXIT_CODE
}

fn transform(_input: PathBuf, _output: PathBuf, _normalize: bool, _mbox_to_markdown: bool) -> i32 {
    println!("Not Yet Implemented");
    NOT_YET_IMPLEMENTED_EXIT_CODE
}
