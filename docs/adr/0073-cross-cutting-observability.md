# ADR-0073: cross-cutting observability (tracing, structured errors, telemetry)

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-27.
- **Status**: Accepted.

## Context

`pigeon job run email-sync` runs long, multi-phase, highly concurrent
pipelines (manifest gather → fetch/transform → dedup → upload) against real
IMAP providers and S3-compatible buckets. Real runs have already hit messy
failure modes — ADR-0071's connection-storm incident produced dozens of
duplicated login errors and thousands of near-identical warning lines, hard
to diagnose after the fact and severe enough to require interrupting the
run.

Today, all error handling in this crate is ad-hoc `Result<T, String>` built
via `format!(...)` at each call site — no structured type carries *which
identity*, *which mailbox/UID/file*, *which pipeline step*, or *how many
retries* an error is attached to. `FailureBreakdown` (`worker.rs`) tallies
failure counts by category (`connect`, `examine`, `batch_error`,
`verification`, `parse_skipped`, `missing_file`), but discards all
per-instance context the moment it increments — a run's final summary says
"3 batches failed verification" with no way to find out which three,
against which identity, or how many retries each one absorbed first.
Retries (`retry_with_backoff`, `requeue_or_none`) never record attempt
numbers anywhere visible. And there is no durable artifact of a run beyond
terminal scrollback and the final summary line — nothing to `grep`/`jq`
after the fact, and no visibility into latency, per-operation duration, or
CPU/memory/disk usage during a run.

This is needed now with `job run email-sync` as the motivating case, but
the ask is for a genuinely cross-cutting mechanism — every current command
should gain it, and every future command should get it for free by
implementing one small trait, consistent with this codebase's existing
trait-heavy architecture (`Job`, `Transform`, `Dedup`, `KeyringEntry`,
`WizardInput` — ADR-0023).

## Decision

Add structured logging/tracing via the `tracing` ecosystem, a durable JSONL
log file per run for post-hoc review, and lightweight latency/resource
telemetry — without touching the established `Result<T, String>` error
convention or the `indicatif`/`MultiProgress` progress-bar discipline
(ADR-0013/0014/0015).

### 1. New dependencies (`Cargo.toml`)

```toml
tracing = "0.1.40"
tracing-subscriber = { version = "0.3.18", features = ["env-filter", "json"] }
tracing-appender = "0.2.3"
tracing-error = "0.2.0"
sysinfo = "0.33"
```
and a dev-dependency, `serde_json = "1"`, for the new integration test.

- **tracing** is the span/event core. `.instrument(span)` (from the base
  crate's `Instrument` extension trait) is the Send-safe way to attach
  context to a future without a manual `span.enter()` guard living across
  an `.await`.
- **tracing-subscriber**'s `env-filter` feature drives `RUST_LOG`/
  `--log-level` filtering; `json` provides the JSONL formatter.
- **tracing-appender** gives a non-blocking, buffered file writer
  (`rolling::never(dir, filename)` + `non_blocking(..)`). Its returned
  `WorkerGuard` must be held for the whole process — dropping it early
  silently truncates buffered output.
- **tracing-error** provides `ErrorLayer` + `SpanTrace::capture()`. Rust has
  no exception stack traces; the *span stack* (identity → mailbox → uid →
  step), captured at error or panic time, is the direct, more useful
  equivalent for this codebase.
- **sysinfo** drives process CPU/memory sampling.
- Deliberately **not** added: `heim` (unmaintained), `metrics`/OpenTelemetry
  (wrong shape for a single-run CLI — no scrape endpoint or remote export
  is needed), `tracing-indicatif` (a real crate for a bar-safe console log
  layer, deliberately deferred — see Out of scope), `thiserror`/`anyhow`
  (would replace the existing `Result<T, String>` convention crate-wide,
  a separately-motivated change this ADR does not make).

### 2. One cross-cutting trait, one harness

```rust
// src/core/observability.rs
pub(crate) trait Observable {
    fn command_name(&self) -> &'static str;
}
```
Implemented directly on the CLI arg enums that already know "which
operation is this" — `JobType` (`commands/job/cli.rs`) and
`KeyringCommands` (`commands/keyring/cli.rs`) — returning stable names such
as `"job.email-sync"`, `"job.decrypt-files"`, `"keyring.add"`,
`"keyring.modify"`, `"keyring.delete"`, `"keyring.list"`. Not implemented on
`Job`/`KeyringEntry` themselves: by the time either trait's methods run,
the command is already inside the span.

One harness function, in a new `observability/` module:
```rust
// src/observability/mod.rs
pub(crate) fn run_instrumented(command_name: &'static str, f: impl FnOnce() -> i32) -> i32 {
    let span = tracing::info_span!("command", command = command_name);
    let _guard = span.enter();
    let start = std::time::Instant::now();
    let exit_code = f();
    tracing::info!(exit_code, elapsed_ms = start.elapsed().as_millis() as u64, "command finished");
    exit_code
}
```
This is sound with a plain `.enter()` because `commands::dispatch` and its
two leaf `dispatch` functions (`job::commands::dispatch`,
`keyring::commands::dispatch`) are **fully synchronous, returning `i32`** —
every async job builds and `block_on`s its own
`tokio::runtime::Builder::new_multi_thread()` internally
(`email_sync/wizard.rs`, `decrypt_files/wizard.rs`, `keyring/wizard.rs`).
From `run_instrumented`'s frame, the whole call is one opaque synchronous
closure with zero `.await` in it, so there is no Send/`.enter()`-across-
await hazard.

**House rule** for the rest of the codebase: anywhere `.await` is actually
present, use `#[tracing::instrument]` or `.instrument(span)` — never a
manual `span.enter()` guard — so nobody has to reason per-call-site about
whether a given future is ever polled across threads.

Call sites (two small edits, no change to `commands/mod.rs`):
```rust
// commands/job/commands.rs
let name = run_args.job_type.command_name();
crate::observability::run_instrumented(name, || match run_args.job_type { /* unchanged */ });

// commands/keyring/commands.rs
let name = command.command_name();
crate::observability::run_instrumented(name, || match command { /* unchanged */ });
```

### 3. Spans and fields in the email-sync pipeline

**Hard rule**: any function taking `IdentityContext`/`BucketConfig` (which
hold plaintext secrets) must `#[instrument(skip(ctx, ...))]` and hand-pick
`fields(identity = %ctx.identity.alias)` explicitly — `#[instrument]`
Debug-formats every parameter by default unless skipped; a secret must
never ride along implicitly.

Concrete sites in `src/commands/job/email_sync/`:
- `mod.rs::gather_pending` — a per-identity span with `identity`/`email`
  fields.
- `worker.rs::run_worker` — wraps each pulled batch's
  `process_batch_on_session` future in
  `.instrument(info_span!("batch", identity, mailbox, uid_count))` before
  awaiting it (genuinely Send-bounded — the future may be polled by any
  worker task).
- `worker.rs::process_batch_on_session` —
  `#[instrument(skip(session, ctx, multi_progress), fields(identity, mailbox), err)]`.
- The per-UID loop inside it: the existing missing-file skip
  (ADR-0071) and the transform/verify failure paths gain
  `tracing::warn!`/`error!` calls carrying `uid` and
  `step = "transform" | "verify"`, inheriting `identity`/`mailbox` from the
  enclosing span for free.
- `sink.rs::fetch_uids`'s silent-drop branch (`let (Some(uid), Some(body))
  = ... else { continue; }`) gains
  `tracing::warn!(mailbox, uid = fetch.uid, "dropped FETCH response missing uid or body")`.
  This closes the gap ADR-0071's Out of scope section explicitly deferred
  ("still doesn't record *which* UID was skipped... would need a
  `fetch_uids` signature change") — structured logging closes it with no
  signature change at all.
- `retry_with_backoff`/`requeue_or_none` — log `attempt`/`attempts`/
  `attempts_remaining` on every retry and on final exhaustion. Because
  these are called from already-span-wrapped callers (`connect_with_retry`,
  `upload_one`, `run_worker`), the events automatically inherit
  `identity`/`mailbox`/`uid` from the ambient span — the concrete payoff of
  a span-based design over threading a bespoke error-context struct through
  every signature.
- `dedup.rs::run_dedup_pass` — fully synchronous, so `span.enter()`/
  `.in_scope(...)` is safe to use directly; a per-entry span carries
  `mailbox`/`uid`.
- `worker.rs::run_upload_phase` (`stream::buffer_unordered`) — each
  per-file future is `.instrument()`'d with `identity`/`file` before
  entering the stream — the other genuinely Send-bounded site, so
  `upload_one`'s own retries inherit context the same way.

Standard field names across the pipeline: `identity`, `mailbox`, `uid`,
`file`/`relpath`, `step`, `attempt`/`attempts_remaining`, `error`.

### 4. Durable JSONL output

A new env var mirrors `PIGEON_CONFIG_DIR`'s existing precedent
(`commands/keyring/store.rs`): `PIGEON_LOG_DIR`. Default, when neither the
env var nor a flag is given:
`directories::ProjectDirs::from("", "", "pigeon")?.data_local_dir().join("logs")`
— deliberately not `config_dir()` (that's `keyring.toml`'s concern) and not
`cache_dir()` (this is a review artifact, not disposable).

Two new **global** flags land directly on the top-level `Cli` struct
(`cli.rs`), so `global = true` propagates them into every subcommand:
```rust
#[arg(long, global = true)]
pub log_level: Option<String>,   // e.g. "info" or "pigeon=debug"; default "warn,pigeon=info"

#[arg(long, global = true)]
pub log_file: Option<PathBuf>,   // overrides the default <PIGEON_LOG_DIR>/pigeon.jsonl
```

Output is one single ever-appending file, `<log_dir>/pigeon.jsonl`, via
`tracing_appender::rolling::never(dir, "pigeon.jsonl")` (a user-supplied
`--log-file` is split into parent + filename for the same call). **No
console/stdout layer is registered in this iteration** — only the JSON
file layer plus `ErrorLayer` (for `SpanTrace::capture()`). This is what
guarantees zero interference with live `indicatif` progress bars
(ADR-0015's hard constraint) by construction, at the cost of no
live-tailable console output — the file itself is still `tail -f`-able.

`main.rs` gains:
```rust
fn main() {
    let cli = Cli::parse();
    let _guard = observability::init(cli.log_level.as_deref(), cli.log_file.as_deref());
    observability::install_panic_hook();
    std::process::exit(commands::dispatch(cli.command));
}
```
`_guard` (the `WorkerGuard`) must stay bound for the whole process — it is
the mechanism that flushes buffered log lines on normal exit.

### 5. Telemetry

**Duration, for free**: the JSON layer is registered with
`.with_span_events(FmtSpan::CLOSE)`, so every span from §3 auto-emits a
close event carrying busy/idle duration — no manual `Instant::now()`
bookkeeping needed at any of those call sites.

**CPU/memory sampling**: a `ResourceSampler` (`observability/resources.rs`),
spawned via `tokio::spawn` only inside a job's own runtime — at the top of
`email_sync`/`decrypt_files`'s `dispatch_async` (both already run inside
their own `block_on`'d runtime). Keyring commands don't get a sampler:
they're too fast for periodic sampling to observe anything.
```rust
pub(crate) struct ResourceSampler { handle: tokio::task::JoinHandle<()> }

impl ResourceSampler {
    pub(crate) fn spawn(interval: Duration) -> Self { /* tokio::spawn loop:
        sleep(interval), refresh_process, tracing::info!(kind = "resource_sample",
        cpu_percent, mem_bytes, disk_bytes, ...) */ }
}

impl Drop for ResourceSampler {
    fn drop(&mut self) { self.handle.abort(); }
}
```
Used as `let _sampler = ResourceSampler::spawn(Duration::from_secs(5));` at
the top of each job's `dispatch_async` — every return path (including the
many early `return fail(err)` sites already present) drops it and aborts
the sampling task automatically, with no manual cleanup at each call site.
Disk usage reuses `core/data.rs::collect_files` to sum staged/output file
sizes as an additional field on the same sample event, rather than adding a
new directory-walking helper.

### 6. Panic handling

A global `std::panic::set_hook` (`observability/panic.rs`) captures
`std::backtrace::Backtrace::force_capture()` plus
`tracing_error::SpanTrace::capture()` — so a panic mid-batch still carries
full identity/mailbox/uid context, not just a raw frame dump — emits one
`tracing::error!` bundling both, then **calls through to the previous
hook** (preserving today's stderr crash output unchanged). This repo's
`Cargo.toml` has no `panic = "abort"` profile override, so this doesn't
change today's unwind/catch semantics (a panic inside a spawned task is
already caught by that task's `JoinHandle` and does not crash the process)
— it only adds a durable structured record alongside the existing
behavior.

## Consequences

- Every current and future `Job`/keyring command is instrumented uniformly
  through `run_instrumented`, without either trait needing to know about
  tracing itself.
- A run now leaves a durable, structured `pigeon.jsonl` behind — with full
  identity/mailbox/uid/step/attempt context on every warning and error,
  and periodic CPU/memory/disk samples — that can be `grep`/`jq`'d after
  the fact, closing the "what actually happened, in detail" gap that
  `FailureBreakdown`'s aggregate counters left open.
- `sink.rs`'s previously-silent dropped-FETCH-response case (`sink.rs`,
  ADR-0071's deferred item) now names the exact UID it dropped.
- No change to the `Result<T, String>` error convention, to progress-bar
  rendering, or to any command's user-facing exit codes/output.
- A run's structured detail lives only in the JSONL file in this
  iteration — there is no live console view of it, by design (see Out of
  scope).

## Out of scope

- OpenTelemetry or any remote/network log or trace export.
- A live, bar-safe console tracing layer — `tracing-indicatif` is real
  prior art for exactly this, deliberately deferred to a future ADR rather
  than adding another dependency now.
- Log rotation/retention policy for `pigeon.jsonl` (a single ever-growing
  file in this iteration; the user manages disk space manually).
- Bridging other crates' `log`-crate output (e.g. from `async-imap`/
  `minio`, if any) into the same sink — `tracing-log` would be the tool,
  not adopted here.
- Per-provider telemetry tuning.
- A codebase-wide `thiserror`/`anyhow` refactor — structured context rides
  span/event fields only; `Result<T, String>` is unchanged.
- Resource sampling for keyring commands.
- Any query/analysis tooling over the JSONL beyond "it's a file you can
  `jq`/`grep`" — no new `pigeon logs ...` subcommand.
- Fixing the pre-existing, unrelated risk that a raw panic mid-render can
  garble a live `indicatif` bar.
