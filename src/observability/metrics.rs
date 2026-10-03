use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use metrics_exporter_prometheus::PrometheusBuilder;

/// Overrides the default port for the local Prometheus metrics endpoint --
/// mirrors `PIGEON_LOG_DIR`'s precedent (ADR-0073), for the same reason:
/// letting an on-host agent (e.g. Grafana Alloy, ADR-0092) and manual/test
/// use pin a known value without a CLI flag.
pub const METRICS_PORT_ENV_VAR: &str = "PIGEON_METRICS_PORT";

const DEFAULT_PORT: u16 = 9091;

/// `--metrics-port` if given, else `$PIGEON_METRICS_PORT`, else `9091`.
pub fn resolve_port(flag: Option<u16>) -> u16 {
    flag.or_else(|| {
        std::env::var(METRICS_PORT_ENV_VAR)
            .ok()
            .and_then(|value| value.parse().ok())
    })
    .unwrap_or(DEFAULT_PORT)
}

/// Installs the global Prometheus recorder, exposing a pull-based
/// `/metrics` endpoint on localhost for an on-host observability agent
/// (e.g. Grafana Alloy) to scrape and forward to Scaleway Cockpit
/// (ADR-0092). Installed once for the whole process, regardless of which
/// command runs -- cheaper and simpler than threading a port through every
/// job wizard to scope it to job commands only (the way `ResourceSampler`
/// is scoped), and a keyring command binding an unused listener for a few
/// milliseconds costs nothing. Binding failure (e.g. two `pigeon`
/// invocations racing for the same port) is non-fatal: metrics are an
/// observability nicety, never worth failing a command over (same posture
/// as ADR-0068's best-effort IMAP logout).
pub fn install(port: u16) {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    if let Err(err) = PrometheusBuilder::new().with_http_listener(addr).install() {
        tracing::warn!(
            port,
            %err,
            "failed to start the Prometheus metrics endpoint; continuing without it"
        );
    }
}

/// Records one job run's final summary counts as Prometheus counters,
/// labeled by `job` (e.g. "sort", "dedupe"). Every job's summary struct
/// converges on this same shape (a job-specific "processed" count plus
/// `failed`/`uploaded`/`unchanged`/`upload_failed`, ADR-0033) even though
/// each has its own, differently-named `FailureBreakdown` fields -- this
/// first iteration deliberately stops at per-job totals rather than
/// per-category labels, since the five jobs' breakdown categories don't
/// share a vocabulary (e.g. sort's "download"/"placement" vs. email-sync's
/// "connect"/"examine"/"batch_error"/...); a per-category cut can be added
/// later if the per-job total proves too coarse for a dashboard.
pub(crate) fn record_job_summary(
    job: &'static str,
    processed: u64,
    failed: u64,
    uploaded: u64,
    unchanged: u64,
    upload_failed: u64,
) {
    ::metrics::counter!("pigeon_job_items_processed_total", "job" => job).increment(processed);
    ::metrics::counter!("pigeon_job_items_failed_total", "job" => job).increment(failed);
    ::metrics::counter!("pigeon_job_uploaded_total", "job" => job).increment(uploaded);
    ::metrics::counter!("pigeon_job_unchanged_total", "job" => job).increment(unchanged);
    ::metrics::counter!("pigeon_job_upload_failed_total", "job" => job).increment(upload_failed);
}
