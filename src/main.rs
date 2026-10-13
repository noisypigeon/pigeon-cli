use clap::Parser;
use pigeon::cli::Cli;
use pigeon::commands;
use pigeon::observability;

fn main() {
    let cli = Cli::parse();
    if let Some(path) = cli.keyring_path.clone() {
        // Recorded before `dispatch` so every later resolution -- each job
        // wizard's `Store::resolve_path`, and `core::rclone`'s own
        // `rclone_config_path` deep inside `execute` -- sees it (ADR-0145).
        commands::keyring::store::set_keyring_path_override(path);
    }
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
    let exit_code = commands::dispatch(cli.command);
    // `std::process::exit` terminates immediately without running destructors,
    // so the JSONL writer's `WorkerGuard` must be dropped explicitly first --
    // otherwise its buffered tail can be lost (ADR-0073's `init()` doc comment
    // already warned "dropping it early silently truncates buffered log lines").
    drop(_guard);
    std::process::exit(exit_code);
}
