use clap::Parser;
use pigeon::cli::Cli;
use pigeon::commands;
use pigeon::observability;

fn main() {
    let cli = Cli::parse();
    let _guard = match observability::init(cli.log_level.as_deref(), cli.log_file.as_deref()) {
        Ok(guard) => guard,
        Err(err) => {
            eprintln!("Error: failed to initialize logging: {err}");
            std::process::exit(commands::FAILURE_EXIT_CODE);
        }
    };
    observability::install_panic_hook();
    if !cli.no_metrics {
        observability::metrics::install(observability::metrics::resolve_port(cli.metrics_port));
    }
    std::process::exit(commands::dispatch(cli.command));
}
