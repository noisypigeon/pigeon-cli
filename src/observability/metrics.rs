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

/// No source `BucketConfig` is in scope at this call site (e.g. an IMAP- or
/// local-dir-sourced job, or a post-download pass operating on already
/// -staged local files) -- the label is always present (ADR-0097), just
/// with this sentinel value, so the metric's label set stays fixed.
const NO_BUCKET: &str = "n/a";

/// Records one item completing one phase of a job's pipeline, live, at the
/// exact moment it happens (ADR-0093) -- labeled `pigeon_job` (not `job`,
/// which Alloy's Prometheus scrape config already reserves to identify the
/// *scrape target* once every instance shares one Cockpit store; colliding
/// with it would silently relabel this as `exported_job`). `phase` reuses
/// each job's existing `FailureBreakdown` field names (e.g. deduplicate's
/// "download"/"hash"/"place"); `outcome` matches that phase's real semantics
/// rather than a forced success/failure binary (e.g. pull_transform's
/// recode step uses "recoded"/"fallback", email_pull's attachment step
/// uses "extracted"/"extraction_failed" since that still counts as synced
/// in the job's own model). Supersedes ADR-0092's `record_job_summary`,
/// which only ever reported once at the very end of a run -- a dashboard
/// built on that never showed anything mid-run. A job's running totals are
/// `sum by (pigeon_job, outcome) (pigeon_job_phase_total{pigeon_job="..."})`.
/// `source_bucket` (ADR-0097) is `Some(alias)` when a source `BucketConfig`
/// is already in scope at the call site, `None` otherwise (see `NO_BUCKET`).
pub(crate) fn record_phase(
    job: &'static str,
    phase: &'static str,
    outcome: &'static str,
    source_bucket: Option<&str>,
) {
    record_phase_count(job, phase, outcome, 1, source_bucket);
}

/// Same as `record_phase`, but for a site that already knows it's
/// reporting for several items at once (e.g. email_sync/email_pull's
/// connect/examine/batch failures, each counted per UID in the batch
/// rather than per individual item) -- avoids looping just to call
/// `record_phase` once per UID.
pub(crate) fn record_phase_count(
    job: &'static str,
    phase: &'static str,
    outcome: &'static str,
    count: u64,
    source_bucket: Option<&str>,
) {
    ::metrics::counter!(
        "pigeon_job_phase_total",
        "pigeon_job" => job,
        "phase" => phase,
        "outcome" => outcome,
        "instance" => crate::observability::instance(),
        "source_bucket" => source_bucket.unwrap_or(NO_BUCKET).to_string(),
    )
    .increment(count);
}

/// Macro-phase indicator for `job`: `0` while local work (download/process/
/// hash/classify/placement -- all pipelined, no single one of them is "the"
/// current phase at any instant) is in progress, `1` once the job has moved
/// into its upload phase. ADR-0019 guarantees this is a real, one-way
/// transition -- upload never starts until all local work is done -- so
/// unlike a per-item phase, a single gauge is an accurate fit here.
pub(crate) fn set_macro_phase(job: &'static str, uploading: bool) {
    ::metrics::gauge!(
        "pigeon_job_macro_phase",
        "pigeon_job" => job,
        "instance" => crate::observability::instance(),
    )
    .set(if uploading { 1.0 } else { 0.0 });
}
