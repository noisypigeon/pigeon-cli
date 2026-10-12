# ADR-0132: consolidate every `rclone` invocation behind one `core::rclone` trait

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-12.
- **Status**: Proposed.

## Context

`pigeon` reaches the external `rclone` binary from four different modules, and
no single place knows how to run an rclone command. Six distinct invocations
are hand-written, each re-deriving the same subprocess scaffolding:

| Invocation | Where | Discipline |
|---|---|---|
| `rclone copy` | `src/commands/job/rclone_transfer.rs:169-327` | spawn + live JSON-log tail |
| `rclone purge` | `src/commands/job/rclone/worker.rs:149-257` | spawn + live JSON-log tail |
| `rclone lsjson` (flat) | `src/commands/job/transform/destination.rs:83-112` | `output()` |
| `rclone lsjson` (recursive) | `src/commands/job/transform/destination.rs:114-150` | `output()` |
| `rclone copyto` | `src/commands/job/transform/push.rs:104-150` | `output()` |
| `rclone version` | 2 prod + 3 test sites | `output()` |

This started as one invocation (ADR-0101's `import`), gained live log polling
(ADR-0102), was hoisted into `rclone_transfer::run_rclone_copy` on its second
consumer (ADR-0112), then accreted `purge` (ADR-0110), two `lsjson` listings
(ADR-0120/0124), and a per-file `copyto` push (ADR-0116) -- each added next to
the shared helper rather than through it. The resulting state:

1. **`run_rclone_copy` takes 13 positional parameters** and carries
   `#[allow(clippy::too_many_arguments)]`. Both call sites end in a run of
   bare `None, None, None` (`rclone/worker.rs:118-133`,
   `transform/worker.rs:734-750`) that says nothing at the call site about
   what was declined.
2. **`run_delete_job` duplicates roughly 110 lines** of `run_rclone_copy`'s
   exact body -- spawn, piped-stderr drain task, `tokio::select!` poll loop
   racing `child.wait()`, final flush poll, exit-status error formatting --
   including a second independent copy of `LOG_POLL_INTERVAL`
   (`rclone/worker.rs:46` vs. `rclone_transfer.rs:21`).
3. **The two `lsjson` functions are near-identical**, differing only in
   `--recursive`, an `--include` filter, and which JSON field they read. Each
   carries its own copy of the exit-code/stderr error format and its own copy
   of the version-fragile `"directory not found"` -> empty-listing tolerance
   already filed as [#63](https://github.com/noisypigeon/pigeon-cli/issues/63).
4. **The "fixed" flag bundle has silently drifted.** `copy` and `purge` carry
   `--retries 5 --low-level-retries 20 --stats 30s --use-json-log --log-level
   INFO --log-file`; `copyto` carries that minus `--stats`; `lsjson` carries
   no logging flags at all. Nothing enforces or even documents a baseline, so
   the drift is invisible until someone diffs three files by hand.
5. **`check_rclone_available` exists four times** -- `rclone/worker.rs:52`
   (the real one), a private duplicate at `transform/wizard.rs:313`, and
   test-only copies in `rclone_transfer.rs:333`, `destination.rs:156`, and
   `push.rs:156`.
6. **Observability coverage is uneven, and in one case absent.**
   `copy`/`purge` emit tracing start/complete lines plus live
   `pigeon_job_phase_total` deltas. `copyto` emits `record_phase_count` and
   `pigeon_upload_*`, but from its *caller* (`push.rs:52-63`) rather than from
   the invocation. `lsjson` emits **nothing at all** -- no span, no tracing
   line, no metric -- even though the recursive variant is a single blocking
   listing of an entire source tree sitting on `transform`'s critical path
   before any byte is pulled. A slow or hanging listing is indistinguishable
   from a hung job in `pigeon.jsonl`.
7. **The report leg is the last keyring dependency inside otherwise
   pure-rclone jobs.** `--report-bucket` (ADR-0100) is a `BucketConfig`
   keyring alias uploaded through the `minio` crate, so `transform` and
   `rclone copy`/`delete` load `keyring.toml` and reach the OS keychain for
   the sole purpose of writing three small artifacts: `transform/wizard.rs:403-408`
   and `rclone/wizard.rs:268-273`/`428-433` construct a `Store` and use it for
   nothing but `report_upload::resolve`. These jobs address all their real
   data as raw rclone `remote:path` strings; only their report speaks a
   different language.

Items 1-6 are one problem wearing six hats: there is no abstraction over "run
an rclone command," so every new invocation copies the nearest one and drifts.
Item 7 is adjacent and worth fixing in the same pass, because the fix is a
fifth consumer of the very abstraction items 1-6 call for.

## Decision

### 1. A new `src/core/rclone/` module

```
src/core/rclone/mod.rs      trait RcloneCommand, the shared runner, check_available()
src/core/rclone/command.rs  the Copy / Purge / Lsjson / CopyTo implementors
src/core/rclone/log.rs      moved verbatim from src/commands/job/rclone_log.rs
```

`src/core/` is the right home on this codebase's own terms: all seven existing
traits are declared there (ADR-0023), and it already holds concrete utility
modules alongside them (`crypto`, `retry`, `data`, `keyring::credentials`), so
the four command implementors living next to their trait is the established
shape rather than an exception to it.

`src/commands/job/rclone_transfer.rs` is deleted outright.
`src/commands/job/rclone_log.rs` moves to `core/rclone/log.rs` with its 19
inline tests. `src/commands/job/rclone/` keeps only what is genuinely
job-shaped: its two `Job` implementors, their plan/summary structs, and the
wizard.

### 2. One trait, four implementors, one runner

Modeled on `WizardInput` (`src/core/wizard.rs:10-45`), the existing precedent
for "implementors declare small descriptive pieces; the trait's own provided
method holds the shared algorithm." Native async fn in trait, `Result<T,
String>`, `pub(crate)` -- no `async_trait`, no error enum, consistent with
every existing trait in the crate.

```rust
pub(crate) trait RcloneCommand {
    type Output;

    /// `"copy"`, `"purge"`, `"lsjson"`, `"copyto"`.
    fn subcommand(&self) -> &'static str;
    /// Operands, then this command's own flags, in argv order. The shared
    /// reliability/logging baseline is appended by the runner.
    fn args(&self) -> Vec<String>;
    /// Where this invocation's `--use-json-log` file goes.
    fn log_path(&self) -> &Path;
    /// Streamed (spawn + live tail) or one-shot (`output()`).
    fn observe(&self) -> Observation;
    /// This invocation's metric labels and log `step` field.
    fn labels(&self) -> Labels;
    /// Turns a finished invocation into `Output` -- the one place a
    /// command-specific success judgment is allowed to live.
    fn finish(&self, outcome: Outcome) -> Result<Self::Output, String>;
    /// `Some((attempts, backoff))` wraps the whole invocation in
    /// `core::retry::retry_with_backoff`; `None` relies on rclone's own
    /// `--retries`/`--low-level-retries`.
    fn retry(&self) -> Option<(usize, Duration)> { None }

    /// Build argv, spawn, observe, emit tracing + metrics, then `finish`.
    async fn run(&self) -> Result<Self::Output, String> { execute(self).await }
}

pub(crate) enum Observation {
    /// `--stats 30s`; stderr piped and drained concurrently; the JSON log
    /// tailed every 10s while the child runs (ADR-0102).
    Streamed(StreamHooks),
    /// Run to completion; stdout captured for `finish` to parse.
    OneShot,
}

pub(crate) struct Labels {
    /// `pigeon_job`: "rclone-copy" | "rclone-delete" | "transform" | ...
    pub job: &'static str,
    /// `phase` metric label and log `step`: "transfer"|"delete"|"pull"|"push"|"list".
    pub step: &'static str,
    /// The `outcome` label a success carries: "transferred"|"deleted"|"listed".
    pub success_outcome: &'static str,
    /// Also emit `pigeon_upload_bytes_total`/`pigeon_upload_outcomes_total`.
    pub upload_metrics: bool,
}

pub(crate) struct Outcome {
    pub exit_code: Option<i32>,
    pub success: bool,
    pub stdout: Vec<u8>,
    pub stderr: String,
    pub log: RcloneLogSummary,
}

async fn execute<C: RcloneCommand + ?Sized>(cmd: &C) -> Result<C::Output, String>;
```

`log_path` is a separate method rather than a field inside `Observation`
specifically so `Observation` carries no lifetime: the runner holds `&self`
across awaits, and a borrowing enum there is the one genuine borrow-checker
trap in this design.

The runner (`execute`) owns everything currently duplicated: argv
construction, the shared flag baseline, spawn, the concurrent stderr drain,
the poll loop, the exit-status error text, the tracing start/complete pair,
and metric emission. Implementors own only what differs.

`Labels.job` is `&'static str` because `record_phase_count` takes one. That
makes `Copy`'s job label a per-call-site field (`"rclone-copy"` for the
`rclone` job, `"transform"` for `transform`'s pull) rather than something
derivable from the type -- a small cost of reusing one struct across two jobs.

### 3. Execution modes, and what this deliberately does *not* unify

`Copy`/`Purge` are `Observation::Streamed`; `Lsjson`/`CopyTo` are
`Observation::OneShot`. The runner therefore has one real branch in it. That
is the honest shape of the problem -- two subprocess disciplines -- so "one
runner" means one runner with two arms sharing argv construction, error
formatting, tracing, and metrics, not a single linear code path.

Stated plainly rather than over-promised, these stay per-command:

- `Copy`'s per-object completion channel (ADR-0116) and its child-PID channel
  for SIGSTOP/SIGCONT backpressure (ADR-0129) are `Copy`-only, carried in a
  `StreamHooks { on_copied, pid_tx }` struct that is `Default`-empty for the
  other three. Both sender types are `Clone`, so the runner clones them out of
  `&self`.
- `Lsjson`'s `"directory not found"` -> empty-listing tolerance lives in
  `Lsjson::finish`. It is re-homed unchanged and remains exactly as
  version-fragile as [#63](https://github.com/noisypigeon/pigeon-cli/issues/63)
  already records; moving it into `core` makes it more prominent without
  making it more correct.
- `CopyTo`'s per-file log path (`{destination_filename}.jsonl`) is still
  computed by its caller. Many concurrent `copyto` subprocesses cannot safely
  share one `--log-file`; only the writing moves into the runner, not the
  naming.
- `set_macro_phase` stays at the job level (`rclone/worker.rs:116`/`:199`).
  Whether a given invocation *is* "uploading" depends on the caller's phase,
  not on the command -- and `transform` deliberately never emits that gauge at
  all (ADR-0130).
- `rclone version` does **not** become an `RcloneCommand`. `check_available()`
  stays a plain function: it runs before any job identity or log path exists,
  so it has no meaningful `Labels` and no `log_path`. Wrapping a liveness
  probe in the trait would mean inventing both.

`Outcome.stdout` is read only by `Lsjson::finish`; the other three ignore it.
That is a mild `Option`-shaped smell carried in the shared type, accepted in
exchange for one `Outcome` rather than two.

### 4. `Lsjson` is one struct, not two

```rust
pub(crate) struct Lsjson {
    pub path: String,
    pub recursive: bool,
    pub include_extension: Option<String>,
    pub job: &'static str,
}
// type Output = Vec<LsjsonEntry>;  LsjsonEntry { name: String, path: String }
```

The two existing functions differ only in `--recursive`, `--include`, and
which field they read out, so two bool/Option fields cover both and each
caller maps what it wants: `.map(|e| e.name).collect::<HashSet<_>>()` for the
destination listing (`list_existing_filenames`),
`.map(|e| e.path).collect::<Vec<_>>()` for the source listing
(`list_source_relative_paths`).

This rests on an assumption to confirm empirically during implementation:
that `rclone lsjson` emits both `Name` and `Path` on every entry even under
`--no-modtime --no-mimetype` (for a flat listing the two are equal). If the
deployed rclone version omits either, `LsjsonEntry`'s fields become
`Option<String>` and the callers handle the absence -- a smaller change than
splitting the struct back in two, but one this ADR flags rather than
discovers.

### 5. The flag baseline, stated once

The shared reliability/observability bundle -- `--retries 5
--low-level-retries 20 --use-json-log --log-level INFO --log-file <path>` --
moves into the runner, applied to every command, with `--stats 30s` added for
the streamed mode only. Each command's genuinely-own flags stay on its own
implementor: `--transfers`/`--checkers`/`--tpslimit`/`--include`/
`--files-from-raw`/`--buffer-size`/`--multi-thread-streams`/
`--multi-thread-cutoff` on `Copy`, `--fast-list` on `Copy` and `Purge`,
`--files-only`/`--no-modtime`/`--no-mimetype`/`--recursive` on `Lsjson`.

This resolves the drift in Context item 4 by construction. Every existing
per-command flag set is preserved exactly as it is today -- including
`lsjson`'s current lack of a `--log-file`, which it now gains along with the
baseline. No transfer behavior changes.

### 6. Retry becomes a declared property of each command

`retry()` returns `Some((3, Duration::from_secs(2)))` for `Lsjson` and
`CopyTo` -- precisely the `retry_with_backoff(3, 2s)` their callers apply
today (`destination.rs:30-31`, `push.rs:22-23`) -- and `None` for
`Copy`/`Purge`, which rely on rclone's own internal retries. Retry stops being
something each call site must remember and becomes a documented property of
the command, removing two call-site closures.

### 7. What the 13-argument call becomes

Struct literal with `..Default::default()`, not a builder. It names only the
fields that matter at each site and lets the irrelevant ones disappear instead
of appearing as a run of bare `None`s -- which is exactly the defect. A builder
would add four `impl` blocks of boilerplate for no gain, since every field is
a plain value with an obvious default.

```rust
// rclone/worker.rs::run_copy_job -- was a 13-arg call ending in three bare Nones
let log = rclone::Copy {
    source, destination, log_path, transfers, checkers, tpslimit,
    job: "rclone-copy", step: "transfer", upload_metrics: true,
    ..Default::default()
}.run().await?;

// transform/worker.rs pull phase -- hooks and files_from named, nothing else
rclone::Copy {
    source: source_path, destination: source_dir_str, log_path: pull_log_path,
    transfers: download_transfers, checkers: download_checkers, tpslimit,
    job: "transform", step: "pull", upload_metrics: false,
    files_from: Some(files_from_path),
    hooks: StreamHooks { on_copied: Some(copied_tx), pid_tx: Some(pull_pid_tx_clone) },
    ..Default::default()
}.run().await

// rclone/worker.rs::run_delete_job -- deletes ~110 lines of duplicated scaffolding
rclone::Purge { source, log_path, checkers }.run().await?

// transform/push.rs -- retry now declared by the command, not wrapped by the caller
rclone::CopyTo { source: local_path, destination, log_path, job: "transform" }.run().await
```

`#[allow(clippy::too_many_arguments)]` is deleted along with the function it
guarded.

### 8. One `check_available()`

`core::rclone::check_available()` replaces all four copies.
`rclone/wizard.rs:264`/`:424` and `transform/wizard.rs:396` call it; the three
test-local duplicates are deleted and the moved tests call the real one.
`deduplicate`'s wizard gains a call too, for the reason in §10.

### 9. Observability coverage and parity

- **`lsjson` gains observability it has never had**: a tracing start/complete
  pair carrying its path, entry count, and elapsed time, plus
  `record_phase_count(job, "list", "listed"|"failed", 1, None)`. A slow or
  failing recursive source listing becomes visible in `pigeon.jsonl` instead
  of looking like a hang -- the gap Context item 6 names.
- **`copyto`'s metric emission moves from its caller into the runner**, so
  every command's metrics come from one place. The emitted series are
  unchanged: `pigeon_job="transform"`, `phase="push"`, plus
  `pigeon_upload_bytes_total`/`pigeon_upload_outcomes_total` with
  `destination_bucket=NO_BUCKET`.
- **The runner emits from `labels()` alone**, so it needs no per-command
  special cases. Streamed commands keep emitting live deltas from the poll
  loop exactly as today. One-shot commands emit `count = 1` per invocation
  with `outcome = success_outcome` or `"failed"` -- which preserves `copyto`'s
  existing per-file series exactly and gives `lsjson` its first metric.
  `Lsjson`'s entry count goes in its tracing line rather than as a metric
  count, so the runner stays generic.
- Every invocation gets a span carrying `step`, following
  `src/observability/metrics.rs`'s conventions: `pigeon_job` as the job label
  key (never `job`, which Alloy reserves), an `instance` label always present,
  and the `NO_BUCKET` sentinel so label sets stay fixed.
- **Not claimed**: `pigeon_upload_attempts_total` and
  `pigeon_upload_duration_seconds` stay unimplemented for every rclone-backed
  leg. They are genuinely per-file measurements rclone's aggregate JSON stats
  cannot produce without the `-v`-level logging these jobs do not enable --
  the gap ADR-0102 already recorded and filed, restated here rather than
  silently implied to be closed. `set_macro_phase` behavior is unchanged.

### 10. `--report-bucket` becomes `--report-path` on the four rclone-addressed jobs

| Job variant | Report flag before | After |
|---|---|---|
| `deduplicate` | `--report-bucket <alias>` | `--report-path <rclone path>` |
| `transform` | `--report-bucket <alias>` | `--report-path <rclone path>` |
| `rclone copy` | `--report-bucket <alias>` | `--report-path <rclone path>` |
| `rclone delete` | `--report-bucket <alias>` | `--report-path <rclone path>` |
| `email-sync` | `--report-bucket <alias>` | unchanged |
| `email-pull` | `--report-bucket <alias>` | unchanged |
| `decrypt-files` | `--report-bucket <alias>` | unchanged |

The name is `--report-path`, not `--report-bucket` with changed semantics,
for the reason ADR-0110 renamed `--source`/`--destination` in the first place:
`bucket` is this codebase's word for a keyring-resolved `BucketConfig` alias,
so reusing it for a raw passthrough string would recreate exactly the
ambiguity that rename set out to kill. The value is an opaque rclone
`remote:path` (e.g. `reports:`), never resolved against the keyring.

The three email/decrypt jobs keep the alias form deliberately: they already
depend on the keyring for identities and encryption keys, so converting them
would buy nothing and would cost `decrypt-files` -- which has no other rclone
involvement -- a hard dependency on the binary.

Mechanics:

- `cli.rs`'s `JobType::Deduplicate`, `JobType::Transform`,
  `RcloneAction::Copy`, and `RcloneAction::Delete` swap
  `report_bucket: Option<String>` for `report_path: Option<String>`, with a
  doc comment stating it is a raw rclone path, never a keyring alias.
- A new `ReportPathInput` joins `ReportBucketInput` in
  `src/commands/job/shared_wizard.rs` -- four consumers from day one, the same
  bar `ReportBucketInput` itself cleared. Its prompt follows
  `rclone/wizard.rs`'s `SourcePathInput`; its non-interactive fallback is
  `Err("--report-path is required when not running interactively")`.
- `report_upload.rs` gains a `ReportTarget` enum -- `Bucket(BucketConfig,
  String)` for the three keyring jobs, `Path(String)` for the four rclone
  jobs -- and `upload_run_artifacts` dispatches on it: `client::upload_if_changed`
  as today, or three `core::rclone::CopyTo` invocations to
  `{report_path}/{prefix}/report.txt`, `/transcript.txt`, `/pigeon.jsonl`.
  Both arms stay best-effort (`tracing::warn!`, never changing the job's exit
  code) and always unencrypted, per ADR-0100 §7/§8.
  `generate_run_id`/`run_prefix` and the three fixed artifact names are
  untouched.
- `transform/wizard.rs` and `rclone/wizard.rs` stop constructing a `Store`
  entirely. Those three dispatchers become keyring-free: no `keyring.toml`, no
  OS-keychain access, no `PIGEON_SECRET_*`.
- **`deduplicate` gains a hard `rclone`-on-`PATH` dependency it does not have
  today.** Its data legs still use `minio` -- ADR-0113, which would change
  that, remains `Proposed` and unimplemented -- so routing its report through
  `rclone copyto` makes rclone newly mandatory for a job that has never needed
  it. Its wizard must call `check_available()` in preflight, and this is a real
  cost of the uniformity, not a free win.

Six dispatch functions carry the change: `deduplicate/wizard.rs:217` and
`:430`, `rclone/wizard.rs:244` and `:413`, `transform/wizard.rs:372`. An
implementation note: `deduplicate/wizard.rs:356` and `transform/wizard.rs:544`
already bind a local `report_path` meaning the *local report file*, so the new
flag needs a distinct variable name at those two sites.

## Consequences

- **Breaking CLI change, no migration shim**, on four job variants --
  consistent with this codebase's established precedent for CLI reshapes
  (ADR-0017, ADR-0094, ADR-0096, ADR-0103, ADR-0110, ADR-0114). Every cron
  entry, deployment script, and runbook passing `--report-bucket` to
  `deduplicate`, `transform`, `rclone copy`, or `rclone delete` must switch to
  `--report-path` with an rclone path value before this ships. The same flag
  keeps working, unchanged, on the three email/decrypt jobs -- so the two
  spellings coexist in the CLI, which is a documented split rather than an
  inconsistency to be tidied later.
- `deduplicate` acquires a hard runtime dependency on the `rclone` binary
  (§10), where previously it needed only `minio` and the keyring.
- Report artifacts for those four jobs land wherever the external
  `rclone.conf` points, outside pigeon's own credential management --
  consistent with how those jobs already treat their primary data (ADR-0101),
  but a real reduction in what pigeon can vouch for about its own
  observability trail.
- `transform` and `rclone copy`/`delete` become fully keyring-free. One fewer
  failure mode (no keychain prompt, no `keyring.toml` to provision) on the
  jobs most likely to run unattended.
- **No metric label values change.** Every `pigeon_job`/`phase`/`outcome`
  value in use today is preserved; the only additions are `lsjson`'s new
  `phase="list"` series. Existing Grafana panels and alert rules keep working
  -- deliberately unlike ADR-0110 and ADR-0113, both of which had to flag
  dashboard breakage.
- 31 inline tests re-home onto the new runner (`rclone_log.rs` 19,
  `rclone_transfer.rs` 4, `rclone/worker.rs` 3, `transform/destination.rs` 4,
  `transform/push.rs` 1). The rclone-dependent ones keep skipping rather than
  failing when the binary is absent.
- A sixth rclone invocation added later has one obvious place to go, and
  cannot silently drift from the flag baseline the way `copyto` and `lsjson`
  already did.

## Out of scope

- **Implementing ADR-0113.** `deduplicate`'s source/destination data legs stay
  on `minio`; only its report leg moves. Converting them is that ADR's job,
  still `Proposed`, and nothing here depends on it landing.
- **Converting the three email/decrypt jobs' report flag** -- explicitly kept
  as a keyring alias, per §10.
- **Fixing [#63](https://github.com/noisypigeon/pigeon-cli/issues/63)'s
  stderr-substring fragility.** The tolerance is re-homed byte-for-byte into
  `Lsjson::finish`, not made robust. That issue stays open and is the right
  place for a real fix.
- **Any `BucketConfig`-to-rclone-remote translation layer** -- rejected by
  ADR-0113 and reaffirmed here. `--report-path` is a raw passthrough string
  only.
- **Adding rclone subcommands pigeon does not already use** (`sync`, `check`,
  `moveto`, `size`, ...). The trait makes them cheap to add; this ADR adds
  none.
- **Changing any transfer behavior, flag value, or default.** Every existing
  per-command flag set is preserved exactly (§5).
- **Retroactively relabeling historical log or metric data.** Nothing needs
  it, since no label value changes.
- **The implementation itself** -- this ADR documents the design; a follow-up
  PR implements it, matching ADR-0113's precedent.

## Amends

ADR-0101 and ADR-0102 (the subprocess and live-JSON-log-polling mechanics,
preserved behaviorally and re-homed into `core::rclone`), ADR-0110 (`purge`'s
dedicated poll loop folded into the shared runner; its `step` labels and
per-action job names preserved), ADR-0112 (`rclone_transfer`'s hoist is
superseded -- `run_rclone_copy` is deleted and its role taken by
`RcloneCommand` + `Copy`), ADR-0116/ADR-0124/ADR-0129 (`Copy`'s per-object
channel, `--files-from-raw`, and PID channel all preserved, now as
implementor-specific fields rather than trailing function parameters),
ADR-0120 (`lsjson`'s destination listing re-homed and finally instrumented),
ADR-0130 (`transform`'s push-leg `pigeon_upload_*` metrics preserved, emission
site moved from `push.rs` into the runner), and ADR-0100 (the report-artifact
destination, for four of seven job variants). Each gets a short
forward-pointing "Amendment" section, matching the pattern ADR-0101 already
carries from ADR-0106/0108/0110.

## Verification

- `mise run ci` clean (fmt-check + lint + test). `cargo clippy --all-targets
  --all-features -- -D warnings` clean specifically without the deleted
  `#[allow(clippy::too_many_arguments)]`.
- The 19 moved `rclone_log.rs` tests pass unchanged at their new path, and the
  4 real end-to-end `rclone_transfer.rs` tests plus `rclone/worker.rs`'s 3
  (copy, copy-with-tpslimit, purge) pass against the new runner rather than
  the deleted functions -- the regression check that the trait really is
  behavior-preserving for the two streamed commands.
- New per-implementor unit tests asserting each command's built argv matches
  its current flag set exactly, so §5's "no behavior change" claim is
  mechanically enforced rather than asserted.
- `Lsjson` round-trips both shapes (flat/`Name`, recursive/`Path`) and still
  returns an empty result for a path that does not exist; the `Name`/`Path`
  co-presence assumption in §4 is confirmed against the installed rclone
  version, and `LsjsonEntry` switched to `Option<String>` fields if it does
  not hold.
- `tests/cli.rs`: `--report-path` appears and `--report-bucket` does not in
  `--help` for `deduplicate`, `transform`, `rclone copy`, `rclone delete`; the
  reverse for `email-sync`, `email-pull`, `decrypt-files`. Seven existing
  assertions become four flipped and three unchanged.
- Manual: end-to-end `rclone copy`, `rclone delete`, and `transform` runs
  against plain local directories (rclone needs no `rclone.conf` for a bare
  path -- the same approach ADR-0112/0113 used) and then against a real
  `rclone.conf`-backed remote, confirming `pigeon.jsonl` now carries `lsjson`
  phase lines it never did before, that `deduplicate` fails fast with a clear
  message when `rclone` is absent from `PATH`, and that the three report
  artifacts land under `reports:{date}-{job-name}-{run-id}/`.

## Amendment (2026-10-12): `deduplicate` becomes a fifth consumer (ADR-0133)

ADR-0133 migrates `deduplicate`'s three data legs onto this ADR's
`RcloneCommand` trait -- `Lsjson` for the pre-run preview and the
skip-before-pull listing, `Copy` for the pull (with `files_from` and
`on_copied`) and for the push -- making it `core::rclone`'s fifth consumer
after `rclone copy`, `rclone delete`, and `transform`'s pull and push.

It also completes one piece of cleanup this ADR could not: with
`deduplicate` no longer downloading via `minio`, `download.rs`'s
`download_with_retry` and `DownloadAnnounce` reach zero consumers and must be
deleted, leaving the module holding only disk-space and hashing helpers --
so it is renamed to `local.rs`. The `check_disk_space`/`sha256_hex` sharing
rationale ADR-0131 recorded, and this ADR relied on, survives the rename.
