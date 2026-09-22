use clap::Parser;
use pigeon::cli::Cli;
use pigeon::commands;

fn main() {
    let cli = Cli::parse();
    std::process::exit(commands::dispatch(cli.command));
}
