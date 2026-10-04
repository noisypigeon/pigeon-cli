pub mod metrics;
pub(crate) mod panic;
pub(crate) mod resources;
pub(crate) mod transcript;

use std::path::{Path, PathBuf};

use tracing_error::ErrorLayer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, fmt};

/// Overrides the default `<data-dir>/logs` location for the durable JSONL log
/// file this module writes -- mirrors `commands::keyring::store::Store`'s
/// `PIGEON_CONFIG_DIR` precedent, for the same reason: isolating tests (and
/// manual use) from the real per-OS data directory (ADR-0073).
pub const LOG_DIR_ENV_VAR: &str = "PIGEON_LOG_DIR";

const LOG_FILE_NAME: &str = "pigeon.jsonl";

/// `<log-dir>`, resolved when `--log-file` isn't given: `$PIGEON_LOG_DIR` if
/// set, otherwise the OS-conventional local-data directory for `pigeon`.
/// Deliberately not `config_dir()` (that's `keyring.toml`'s concern) or
/// `cache_dir()` (this is a review artifact, not disposable).
fn default_log_dir() -> Result<PathBuf, String> {
    if let Ok(dir) = std::env::var(LOG_DIR_ENV_VAR) {
        return Ok(PathBuf::from(dir));
    }
    let project_dirs = directories::ProjectDirs::from("", "", "pigeon")
        .ok_or("could not determine the data directory for this platform")?;
    Ok(project_dirs.data_local_dir().join("logs"))
}

/// Splits `log_file` (when given) into its parent directory and file name,
/// falling back to `default_log_dir()`/`pigeon.jsonl` otherwise --
/// `tracing_appender::rolling::never` takes a directory and a file name as
/// two separate arguments, not one path.
fn resolve_log_path(log_file: Option<&Path>) -> Result<(PathBuf, String), String> {
    match log_file {
        Some(path) => {
            let dir = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."));
            let file_name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| LOG_FILE_NAME.to_string());
            Ok((dir, file_name))
        }
        None => Ok((default_log_dir()?, LOG_FILE_NAME.to_string())),
    }
}

/// Initializes the global tracing subscriber: a JSON-formatted, non-blocking
/// file layer (the durable "review this run later" artifact, ADR-0073) plus
/// an `ErrorLayer` (so `tracing_error::SpanTrace::capture()` works from
/// anywhere spans are active). Deliberately registers no console layer --
/// ADR-0013/0014/0015's `indicatif`/`MultiProgress` progress bars must never
/// share stdout/stderr with a second, independent writer. Returns a
/// `WorkerGuard` that must be kept alive for the whole process; dropping it
/// early silently truncates buffered log lines.
pub fn init(
    log_level: Option<&str>,
    log_file: Option<&Path>,
) -> Result<tracing_appender::non_blocking::WorkerGuard, String> {
    let (dir, file_name) = resolve_log_path(log_file)?;
    std::fs::create_dir_all(&dir)
        .map_err(|err| format!("failed to create {}: {err}", dir.display()))?;
    let _ = LOG_FILE_PATH.set(dir.join(&file_name));

    let appender = tracing_appender::rolling::never(&dir, &file_name);
    let (writer, guard) = tracing_appender::non_blocking(appender);

    let directive = log_level
        .map(str::to_string)
        .or_else(|| std::env::var("RUST_LOG").ok())
        .unwrap_or_else(|| "warn,pigeon=info".to_string());
    let filter = EnvFilter::try_new(&directive)
        .map_err(|err| format!("invalid log filter '{directive}': {err}"))?;

    let json_layer = fmt::layer()
        .json()
        .with_writer(writer)
        .with_span_events(fmt::format::FmtSpan::CLOSE);

    tracing_subscriber::registry()
        .with(filter)
        .with(json_layer)
        .with(ErrorLayer::default())
        .init();

    Ok(guard)
}

/// Runs `f` (one full command dispatch) inside a `command`-named tracing
/// span carrying `command_name`, logging its elapsed time and exit code on
/// completion (ADR-0073). Sound with a plain `span.enter()` guard here
/// specifically because every command dispatch in this crate is fully
/// synchronous end to end -- each async job builds and `block_on`s its own
/// tokio runtime internally, so from this function's frame the whole call is
/// one opaque synchronous closure with zero `.await` in it. Anywhere
/// `.await` is actually present, use `#[tracing::instrument]`/
/// `.instrument(span)` instead -- never a manual `span.enter()` guard held
/// across an await point.
/// Re-exports `panic::install_panic_hook` for `main.rs`, which lives in a
/// separate (binary) crate and so can't reach a `pub(crate)` item directly.
pub fn install_panic_hook() {
    panic::install_panic_hook();
}

/// This process's hostname, resolved once and cached for the rest of the
/// process's life (ADR-0097) -- reused for both the `command` span's
/// `instance` field and every custom metric's `instance` label, so a run
/// touching tens of thousands of items never re-queries `sysinfo` per item.
pub(crate) fn instance() -> &'static str {
    static INSTANCE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    INSTANCE.get_or_init(|| sysinfo::System::host_name().unwrap_or_else(|| "unknown".to_string()))
}

/// The durable JSONL log file's resolved path, cached by `init()` (ADR-0100)
/// -- lets a job upload the shared `pigeon.jsonl` to a report bucket without
/// recomputing `resolve_log_path`'s `--log-file`-vs-default logic itself.
static LOG_FILE_PATH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

pub(crate) fn log_file_path() -> Result<PathBuf, String> {
    LOG_FILE_PATH
        .get()
        .cloned()
        .ok_or_else(|| "log file path not resolved -- observability::init must run first".into())
}

pub(crate) fn run_instrumented(command_name: &'static str, f: impl FnOnce() -> i32) -> i32 {
    // Every event in this run inherits both fields via `spans[]` (ADR-0093)
    // -- "what job" (`command`) and "what instance" (`instance`), closing
    // the gap a shared, multi-instance Cockpit store otherwise has no way
    // to disambiguate on the logs side (Prometheus's scrape-level `job`/
    // `instance` labels have no log-side equivalent).
    let span = tracing::info_span!("command", command = command_name, instance = instance());
    let _guard = span.enter();
    // Brackets "command finished" below (ADR-0097) -- previously a job that
    // crashed or hung left no trace that it had even started.
    tracing::info!("command started");
    let start = std::time::Instant::now();
    let exit_code = f();
    let elapsed = start.elapsed();
    tracing::info!(
        exit_code,
        elapsed_ms = elapsed.as_millis() as u64,
        "command finished"
    );
    let status = if exit_code == 0 { "success" } else { "failure" };
    ::metrics::histogram!(
        "pigeon_command_duration_seconds",
        "command" => command_name,
        "instance" => instance(),
    )
    .record(elapsed.as_secs_f64());
    ::metrics::counter!(
        "pigeon_command_runs_total",
        "command" => command_name,
        "status" => status,
        "instance" => instance(),
    )
    .increment(1);
    exit_code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_log_path_splits_a_given_file_into_dir_and_name() {
        let (dir, file_name) = resolve_log_path(Some(Path::new("/tmp/foo/out.jsonl"))).unwrap();
        assert_eq!(dir, PathBuf::from("/tmp/foo"));
        assert_eq!(file_name, "out.jsonl");
    }

    #[test]
    fn resolve_log_path_falls_back_to_the_default_dir_and_name_when_omitted() {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: tests run single-threaded within this process for this env var
        // (no other test reads/writes PIGEON_LOG_DIR concurrently).
        unsafe {
            std::env::set_var(LOG_DIR_ENV_VAR, dir.path());
        }
        let (resolved_dir, file_name) = resolve_log_path(None).unwrap();
        unsafe {
            std::env::remove_var(LOG_DIR_ENV_VAR);
        }
        assert_eq!(resolved_dir, dir.path());
        assert_eq!(file_name, LOG_FILE_NAME);
    }

    #[test]
    fn run_instrumented_returns_the_wrapped_closure_s_exit_code() {
        assert_eq!(run_instrumented("test.command", || 0), 0);
        assert_eq!(run_instrumented("test.command", || 1), 1);
    }
}
