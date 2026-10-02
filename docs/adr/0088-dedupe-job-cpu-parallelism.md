# ADR-0088: Parallelize the dedupe job's hash phase across all available cores

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-01.
- **Status**: Accepted.

## Context

`pigeon job run dedupe` (ADR-0082) is CPU-bound during its fetch+hash phase:
every bucket object is downloaded, zips are expanded, and every resulting
file is SHA-256-hashed before the (deliberately sequential) placement pass
runs. Live observation of a large real run on an 8-core machine via `htop`:
only 2 cores were pegged at 100% (process at 198% total CPU) while the other
six sat idle, and RSS had grown to 2.3GB — expected, since the job holds
every hashed file's metadata in memory for the whole run, and not something
this ADR addresses.

Root cause, found by reading `src/commands/job/dedupe/worker.rs` and
`src/commands/job/shared_wizard.rs` directly:

- `run_dedupe_job` spawns exactly `concurrency` persistent `tokio::spawn`
  workers (`worker.rs:268-342`) pulling from one shared
  `Arc<Mutex<VecDeque<QueueItem>>>` queue. Each worker both downloads *and*
  hashes/expands an item synchronously inline — `download::sha256_file`
  (`worker.rs:194`) and `archive::expand_to_dir` (`worker.rs:163`) are plain
  blocking calls made directly inside an `async fn`, with no
  `tokio::task::spawn_blocking`.
- `concurrency` is resolved via the **generic, shared** `ConcurrencyInput`
  (`shared_wizard.rs:132-155`), whose interactive prompt defaults to a flat
  **4** — a value tuned for I/O-bound jobs (IMAP connection counts, S3
  upload fan-out, per ADR-0014/ADR-0024), not a CPU-bound hashing phase. On
  an 8-core machine this caps the phase at half the available cores before
  even accounting for the tail-draining effect inherent to a shared-queue
  worker pool: workers that run out of queued items simply exit, so only
  the stragglers still holding large files keep running near the end of the
  phase — exactly the "2 threads carrying the whole job" symptom observed.
- `email_sync` and `email_pull` already establish the precedent of shadowing
  this shared struct with a job-local `ConcurrencyInput` when the generic
  default doesn't fit (`src/commands/job/email_sync/wizard.rs:207-232`,
  `src/commands/job/email_pull/wizard.rs:152-177`) — `dedupe/wizard.rs`
  currently imports and uses the shared one unmodified.
- Separately: `mise run build`/`mise run pigeon` (`.mise.toml`) both build
  **debug** (`cargo build`, no `--release`), which disproportionately hurts
  a CPU-bound hashing/sorting phase. There is no release-build task in the
  repo today.

No `rayon`, `num_cpus`, or `std::thread::available_parallelism` usage exists
anywhere in the codebase yet (confirmed by grep) — `rayon` is present only
as a transitive dependency (via `zip`), unused directly.

## Decision

Three changes, all scoped to the dedupe job, plus one repo-wide tooling
addition.

### 1. CPU-aware default concurrency for `dedupe`, not a global default change

A dedupe-local `ConcurrencyInput` in `src/commands/job/dedupe/wizard.rs`
shadows the shared one (mirroring the `email_sync`/`email_pull` precedent
exactly). Its `prompt()` suggests a new pure helper,
`default_concurrency() -> usize` (`std::thread::available_parallelism()`,
falling back to `4` if the OS can't report it), instead of the hardcoded
`4`. `--concurrency <N>` and the non-interactive required-flag behavior
(`non_interactive_fallback` still errors) are unchanged — this only changes
what an interactive run *suggests* when the user doesn't override it. The
shared `ConcurrencyInput` used by `sort`, `decrypt-files`, and others is
untouched: their work is I/O-bound and the tuned default of 4 still applies
there, so this is a job-specific fix, not a blanket default bump.

### 2. Move blocking CPU work off the async runtime's worker threads

In `src/commands/job/dedupe/worker.rs::process_item`, the
`download::sha256_file(&path)` call and the `archive::expand_to_dir(...)`
call are each wrapped in `tokio::task::spawn_blocking`. This is a
first-in-codebase use of `spawn_blocking` — every other CPU-ish call
elsewhere in the codebase is either already async I/O or, like this one
before this change, blocking inline on a multi-thread runtime where that
happened not to matter. It matters here specifically because, after change
1, `concurrency` workers can all be mid-hash simultaneously, occupying
*every* one of the runtime's async worker threads for the whole hash
duration if left inline — leaving none free for other async bookkeeping
sharing the same runtime (progress-bar ticks, the queue-poll
`tokio::time::sleep` path, disk-space checks). `spawn_blocking` hands the
work to tokio's separate blocking-thread pool instead, the standard way to
run synchronous CPU work from async code, and decouples realized hash/expand
parallelism from the fixed N-worker loop shape.

### 3. Add an opt-in release-build task pair

`mise run build-release` and `mise run pigeon-release` are added to
`.mise.toml`, mirroring the existing `build`/`pigeon` tasks but running
`cargo build --release` and signing `target/release/pigeon`. `build`,
`pigeon`, and `ci` all stay on the debug profile — fast dev-loop iteration
is the right default for day-to-day work — while the new tasks exist for
CPU-heavy production runs like a large `dedupe` job, where an unoptimized
SHA-256 loop and queue/lock overhead cost far more than the extra compile
time.

## Consequences

- `dedupe`'s default concurrency now scales with the machine it runs on
  instead of a flat `4`, so a run left at its interactive default uses all
  available cores during the hash phase rather than half (or fewer) of
  them on typical modern hardware.
- The dedupe job gains its own `ConcurrencyInput`/`default_concurrency()`,
  a small, intentional duplication of the shared struct's shape — consistent
  with this codebase's existing `email_sync`/`email_pull` precedent for
  job-specific concurrency semantics, not a new pattern.
- `process_item`'s CPU-bound calls now hop through `tokio::task::spawn_blocking`,
  the first use of that primitive in the codebase; future CPU-bound work
  added to any async job should follow the same pattern rather than calling
  blocking functions inline.
- Running `pigeon job run dedupe` via the new `pigeon-release` task is
  measurably faster for large buckets than the debug binary; this doesn't
  change any default, so existing muscle-memory (`mise run pigeon -- ...`)
  keeps working exactly as before for everything else.

## Out of scope

- **Reducing peak memory** (`Vec<HashedFile>` accumulated for the whole
  bucket before placement starts, `worker.rs:264,326`) — a real but separate
  problem from core utilization, and current RSS levels were confirmed
  acceptable for the observed run size.
- **Parallelizing the placement/sort pass** (`dedup.rs::place_and_report`) —
  ADR-0082 already made this sequential deliberately, to avoid racing
  `unique_path` calls against a shared output directory; it also does no
  hashing (just `HashMap` lookups and `fs::rename`), so it was never a
  CPU-bound phase to begin with.
- **Parallelizing extraction within a single zip** — `archive::expand_to_dir`
  stays a sequential per-entry loop; multiple distinct zips already run
  concurrently across workers once change 1 lands, and splitting one zip's
  member extraction further is unlikely to be the bottleneck.
- **Changing the shared `ConcurrencyInput` default (4)** used by `sort`,
  `decrypt-files`, and other I/O-bound jobs — conflating an I/O-bound-tuned
  default with a CPU-bound one would be wrong for both.
