# ADR-0146: `pigeon job run email import`

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-13.
- **Status**: Accepted.

## Context

There are two email jobs today, both predating ADR-0132's `core::rclone` consolidation:

| | `email-sync` (ADR-0007/0021) | `email-pull` (ADR-0081) |
|---|---|---|
| Size | 4,744 lines / 7 files | 2,188 lines / 5 files |
| Output | `.eml` -> Markdown + frontmatter | raw `.eml` + unpacked attachments |
| Dedup | messages **and** attachments | attachments only |
| Encryption | optional AES-256-GCM-SIV (ADR-0025) | never (ADR-0081 §4) |
| Upload | `minio`, one phase after all local work | same |

What reading the current implementation directly turns up, rather than assuming from prior ADR text:

- **There is one real IMAP core, and it is already shared.** `email_sync/sink.rs` holds `fetch_uids` (`sink.rs:151` -- the only fetch loop either job has), `sanitize_mailbox_path`, `missing_uids`, `is_stale`, `read_uidvalidity`/`write_uidvalidity`, `clear_eml_files`, and `new_progress_bar`. `email_pull` has no `sink.rs` at all; it imports every one of them. `email_sync::manifest::{Batch, split_into_batches}` is shared the same way. A third job reusing them is the established shape, not a new coupling.
- **Neither job is built for a run that lasts days.** `connect_with_retry` budgets `CONNECT_RETRIES = 3` at `RETRY_BACKOFF = 5s` -- **15 seconds** of total patience (`email_sync/worker.rs:42`, `email_pull/worker.rs:39`). A failed batch gets `BATCH_RETRIES = 2` at `BATCH_RETRY_BACKOFF = 2s`. ADR-0144 already established, from a real 74-second DNS blackout, that a budget of this order is roughly an order of magnitude too short for a real outage.
- **There is no rate governor of any kind.** `fetch_uids` issues one `uid_fetch` for an entire comma-joined UID set and streams the responses as fast as the server will answer them. Throughput is whatever `--concurrency` and the server's patience happen to produce. ADR-0071 added a per-identity *connection* cap after a real "Too many simultaneous connections" failure, which bounds parallelism but says nothing about request rate.
- **Nothing reaches the destination until the very end.** Both `run_*_job`s are fetch-everything -> dedup -> *then* `upload::run_upload_phase` (ADR-0019's deliberate local-first ordering). An account with hundreds of thousands of messages therefore needs local disk for the whole corpus, and an interruption before the upload phase delivers nothing at all.
- **These two are the last `minio` data-path consumers.** ADR-0133 moved `deduplicate`'s three legs onto `core::rclone`; ADR-0131 and ADR-0134 removed the other two jobs that used `minio`. `src/commands/job/upload.rs` now exists solely for these two.
- **`email_sync::sink::run` has zero callers.** It is the legacy `pigeon email sync --debug sink` entry point (ADR-0007 §4), dead since ADR-0021 replaced that CLI, and it survives `clippy -D warnings` only because `pub mod sink` is reachable from `lib.rs`. Recorded here because §2 is where it gets dropped.
- **The pre-run summary costs a second full-corpus traversal.** `gather_pending` calls `manifest::pull_manifest`, which issues `UID FETCH <set> (UID RFC822.SIZE BODYSTRUCTURE)` for *every* pending UID -- no body content, but one round trip per message -- purely to populate the wizard table's `BYTES` and `ATTACHMENTS` columns (ADR-0032). ADR-0034 deliberately removed the persisted-manifest reuse that used to avoid repaying it, because the cached value went stale and read as zero. That was the right fix for a job whose fetch is unthrottled; it is the wrong cost for one whose fetch is not (§7).

**The want:** a third, deliberately minimal email job. Read an IMAP account named by a keyring alias; deliver raw `.eml` plus unpacked attachments to an rclone-addressed `--destination`; hold a steady, rate-limit-respecting pace; survive a run lasting multiple days. Deduplication, indexing, renaming, and transforms are out of scope -- this job moves bytes and nothing else.

`email-sync` and `email-pull` are to be removed once this lands. That is **ADR-0147, a separate ADR and PR** (§10). This one adds the new job alongside them, so `main` is never in a transitional state, and deliberately declines three hoists a third consumer would normally trigger (§2).

## Decision

### 1. Command surface -- a nested `email` group

A new `JobType::Email(EmailArgs)` in `src/commands/job/cli.rs`, mirroring the `JobType::Rclone(RcloneArgs)` nesting ADR-0110 introduced:

```rust
#[derive(Args, Debug)]
pub struct EmailArgs {
    #[command(subcommand)]
    pub action: EmailAction,
}

#[derive(Subcommand, Debug)]
pub enum EmailAction {
    Import {
        identities: Option<Vec<String>>,   // --identities, value_delimiter = ','
        destination: Option<String>,       // --destination  alias[:prefix/]
        local_output: Option<PathBuf>,
        messages_per_minute: Option<usize>,
        batch_size: Option<usize>,
        max_connections: Option<usize>,
        push_concurrency: Option<usize>,
        report_bucket: Option<String>,
        non_interactive: bool,
    },
}
```

`impl Observable for JobType` gains an arm resolving to `"job.email-import"`, so `job_name()` derives `"email-import"` for ADR-0100's report prefix with no change to that function. The dispatch arm in `src/commands/job/commands.rs` calls `email_import::wizard::dispatch(...)`.

This leaves `email-sync` and `email-pull` on their existing flat spellings, so two nesting shapes coexist for one PR. That is deliberate: renaming either shipped command would be a breaking change to a job ADR-0147 is about to delete outright.

**Flags deliberately absent**, each stated so none reads as an oversight:

- `--encryption-key` -- out of scope permanently, following ADR-0081 §4's precedent for the sibling job. A negative test pins it (§Verification).
- `--upload-only` -- meaningless by construction. ADR-0089/0090's flag exists to resume an upload phase that runs after all local work; this job has no such phase (§5), and nothing is ever left locally un-pushed.
- `--concurrency` -- the pacer (§4), not a worker count, governs throughput. Worker count is derived: `min(max_connections * identities, total_batches)`. Offering a knob that does not control the thing its name implies would be worse than omitting it.
- `--transfers`/`--checkers`/`--tpslimit` -- `CopyTo` accepts none of them (its `args()` is just `[source, destination]`), because a single-file `copyto` has no transfer set to parallelize. `--push-concurrency` is the only knob that applies.

### 2. Module layout, and three hoists deliberately declined

```
src/commands/job/email_import/
  mod.rs       ImportContext, ImportJob/ImportPlan + impl Job, gather_pending
  wizard.rs    WizardInput impls, dispatch/dispatch_async
  worker.rs    run_worker, process_batch_on_session, run_email_import_job
  manifest.rs  the .pushed checkpoint, batching at an explicit --batch-size
  pace.rs      Pacer + the pure reserve()
  gate.rs      RecoveryGate, used twice
  push.rs      deliver_one -- the CopyTo legs
  attach.rs    extract_attachments, without hashing
```

**Reused in place, with no edits to any existing file:** `email_sync::sink`'s eight live functions and `email_sync::manifest::{Batch, split_into_batches}`. Both are `pub mod` and already reached exactly this way by `email_pull`. ADR-0147 relocates them into `email_import/` as part of deleting the two directories -- the same hoist-before-delete ADR-0131 performed for `pull_transform::archive`, done at deletion time rather than pre-emptively.

**Deliberately duplicated rather than hoisted:** `IdentitiesInput`, `LocalOutputInput`, and `connect_with_retry`. A third consumer is precisely this codebase's duplicate-until-the-third-consumer trigger (ADR-0082 §0, ADR-0096, ADR-0145 §4), and ADR-0081 itself duplicated `connect_with_retry` while naming that rule. Declining it here is therefore a stated exception, not an oversight, on two grounds: ADR-0147 returns all three to a single consumer, so hoisting now means un-hoisting next PR -- the churn ADR-0094 and ADR-0131 already paid once each; and `connect_with_retry`'s whole purpose here is a *different* budget (§4), so the three copies would not be byte-identical the way `load_keyring`'s six were.

`ImportContext { identity, secret, local_dir }` is its own type rather than `email_sync::IdentityContext`, which carries a `staging_dir`/`output_dir` split this job has no equivalent of: push is inline, so there is no staged-then-published tree to represent.

### 3. Destination layout

`--destination` resolves through `RemoteTarget::parse(value, &store)` (`src/core/rclone/config.rs:152`), giving ADR-0138's `alias[:prefix/]` addressing over ADR-0140's restored two-section config. An omitted prefix is the bucket root, matching `rclone copy`'s `--source-bucket` rather than `transform`'s `--source`: this job has no `--input-file-type` to derive a default prefix from.

```
<destination>/<sanitized-email>/<mailbox-relpath>/<uid>.eml
<destination>/<sanitized-email>/<mailbox-relpath>/<uid>/attachments/<sanitized-name>
```

`identity::sanitize_segment` names the identity directory and `sink::sanitize_mailbox_path` the mailbox path, both exactly as the sibling jobs already do. Attachment filenames go through `core::data::sanitize_filename` plus `unique_path`, keeping ADR-0013's sender-controlled-MIME-name hardening and ADR-0033's control-character escaping. `(mailbox, uid)` is unique within an identity, so no message object is ever renamed to avoid a collision.

Attachments are uploaded **in addition to**, not instead of, the `.eml`, which retains its own inline base64 copy. That duplication is intentional: the `.eml` is the faithful archival artifact and the unpacked attachment is the usable one, and this job has no dedup pass that could collapse them even in principle (§Out of scope).

### 4. Pacing and bounded patience -- the load-bearing decision

**`--messages-per-minute` (default 120)** is the run's throughput governor. `pace.rs` holds a token bucket whose arithmetic is a pure function, so the schedule is unit-testable independently of any clock or lock -- ADR-0143's `circuit_breaker_should_trip` precedent:

```rust
/// Reserves `count` fetch slots spaced `interval` apart, starting no
/// earlier than `next_allowed`. Returns (sleep until, new next_allowed).
fn reserve(next_allowed: Instant, now: Instant, count: u32, interval: Duration)
    -> (Instant, Instant)
{
    let start = if next_allowed > now { next_allowed } else { now };
    (start, start + interval * count)
}
```

`Pacer { next_allowed: Mutex<Instant>, interval }`; `acquire(count)` takes the lock, calls `reserve`, stores the new value, **releases the lock, and only then sleeps**. Sleeping while holding it would serialize every worker behind one waiter and convert the pacer into a global mutex. `interval = 60s / messages_per_minute.max(1)`.

Two properties follow, and both are the point:

- Concurrent acquirers *stagger* rather than burst, because each reserves a distinct window. The fetch rate is therefore a declared property of the run, not an emergent side effect of worker count -- which is why §1 omits `--concurrency`.
- Idle time is never paid back. A gap longer than the reservation horizon yields `now`, not a burst of accumulated credit, so a run that stalls on a retry does not then hammer the server to catch up.

At the default, 200k messages is roughly 28 hours: the multi-day shape this job exists for.

**Three bounded retry layers**, each far more patient than the sibling jobs' and each with a stated ceiling:

| Layer | Budget | Why this number |
|---|---|---|
| Connect | `retry_with_backoff(4, 30s)` -> t=0/+30/+90/+180s, about **5 min** | Matches `CopyTo::retry()`'s ADR-0144 budget. The sibling jobs' `(3, 5s)` is 15s, which that ADR's own evidence shows is over 4x too short for a real outage -- and a negative DNS answer is negative-TTL cached, so retrying inside seconds is structurally futile |
| Batch | `BATCH_RETRIES = 3` at `30s` linear -> 30/60/90s | A requeued batch via `requeue_or_none`'s existing shape, scaled from `2s` for the same reason |
| Server gate | 6 rounds x 60s linear -> about **21 min**, then latches | Below |

**`gate.rs` -- `RecoveryGate`.** ADR-0144's `EndpointGate` lesson, applied from day one rather than relearned from a failed run. Same mechanism: a `watch::Sender<bool>` gate, a `Mutex<()>` election, and an `AtomicUsize` round count. Whichever task first observes a classified failure wins the election and becomes the prober: it closes the gate, sleeps `backoff * round`, and reopens it. Every other task failing the same way during that window parks on the gate instead of issuing its own doomed request. `rounds` is never reset, so once the budget is exhausted the gate latches open-and-giving-up and the remaining work fails immediately rather than each successive batch re-paying the whole budget -- the amplification ADR-0144 identified as the real cost. Like that gate and unlike ADR-0121/0143's transcode breaker, it **pauses and resumes** rather than aborting the run: a transient server fault warrants patience, not abandonment.

It is written generic over its classifier and round budget, and used **twice**, which is why it is job-local rather than in `core::rclone`:

- The IMAP leg, with `is_server_overloaded_error` and 6 rounds x 60s. Its signatures are named consts per ADR-0143 and matched case-insensitively, exactly as `core::rclone::is_endpoint_unreachable_error` does: `"too many simultaneous connections"`, `"too many connections"`, `"try again later"`, `"temporarily unavailable"`, `"server unavailable"`, `"over quota"`, `"rate limit"`, `"throttl"`.
- The push leg, reusing `core::rclone::is_endpoint_unreachable_error` directly, with 4 rounds x 30s on top of `CopyTo`'s own ~5 min.

A third consumer should hoist `RecoveryGate` into `core::rclone`, which already owns one of its two classifiers. Filed per ADR-0031 rather than pre-emptively hoisted on its second.

**No IMAP keepalive, deliberately.** The pacer guarantees continuous traffic at any rate above zero, so a session does not sit idle long enough to be reaped under normal operation; and a session that is reaped surfaces as a batch failure, which the existing drop-the-session-and-reconnect path in `run_worker` already handles correctly. Stated so a future reader does not read the absence as a gap.

### 5. Per-message delivery, and a GC that retains nothing

`process_batch_on_session`, per batch:

1. `pacer.acquire(batch.uids.len())` -- the rate reservation, before any request.
2. `local::check_disk_space(&local_dir, 0)` -- reused from `src/commands/job/local.rs`, the helper ADR-0133 kept shared for exactly this.
3. `sink::fetch_uids(...)`, writing `<mailbox_dir>/<uid>.eml`.
4. Per UID, bounded by `Semaphore::new(push_concurrency)`: `deliver_one` -- extract attachments, `CopyTo` the `.eml`, `CopyTo` each attachment, each leg parked on the push gate for a classified endpoint error.
5. **Always** delete `<uid>.eml` and `<uid>/`, on success *and* on failure.
6. Append `(mailbox, uid)` to `.pushed` only once every object for that UID is confirmed pushed.

Step 5 diverges from ADR-0124's retain-on-failure rule, and is strictly safer than it. A `.eml` whose push failed is **still on the IMAP server**, undamaged, where a rerun refetches it anyway -- the identical "non-destructive, still at the source" reasoning ADR-0143 used to justify deleting a wrong-input-kind failure's local copy. `transform` retains a failure because a transcode result is expensive and not trivially reproducible; nothing this job holds is either. So this job retains nothing on failure, and local disk is bounded to the in-flight window **unconditionally** -- which is what makes ADR-0143's ~80 GiB retention pathology structurally impossible here rather than merely unlikely.

Step 6 is why §1 has no `--upload-only`: the checkpoint records delivery, not local completion, so there is no state in which local work is done and upload is pending.

`push.rs` follows `transform/push.rs`'s shape, including **one rclone log file per object** -- many concurrent single-file `rclone` subprocesses cannot safely share one `--log-file` -- and reading `bytes` from `fs::metadata` *before* the push, since step 5 deletes the file immediately afterwards and `CopyTo::emit_upload_metrics` needs the size.

### 6. Resume

| File | Location | Format |
|---|---|---|
| `.pushed` | `<local-output>/.staging/` | `<mailbox>\t<uid>`, append-only |
| `.uidvalidity` | `<local-output>/.staging/<mailbox-relpath>/` | bare `u32`, via `sink::read_uidvalidity`/`write_uidvalidity` |

`done_uids(&entries) -> HashSet<(String, u32)>` is keyed on `(raw mailbox name, uid)` and differenced against `uid_search("ALL")` through `sink::missing_uids`, the same composite-key resume both sibling jobs use. A `UIDVALIDITY` change clears that mailbox's rows (`sink::is_stale` plus `clear_checkpoint_for_mailbox`). Malformed lines are skipped leniently and a missing file is an empty set, also matching them.

Two fields rather than `email-pull`'s three and `email-sync`'s seven: there are no content hashes to record, because there is no dedup, and no staged-path bookkeeping, because there is no placement step.

### 7. No manifest pass -- counts only

`gather_pending` lists mailboxes, `EXAMINE`s each one, checks `UIDVALIDITY`, and computes pending UIDs from `uid_search("ALL")` minus `.pushed`. It **does not call `pull_manifest`**. The pre-run table is:

```
IDENTITY     MAILBOXES   PENDING
archive          14       203,481
```

An explicit divergence from ADR-0032 and ADR-0034, with their own reasoning inverted rather than ignored. A `BODYSTRUCTURE` fetch for every pending UID is a second full-corpus traversal; under an unthrottled fetch it is cheap relative to the bodies that follow, which is why ADR-0032 could add it and ADR-0034 could make it unconditional. Under §4's deliberate throttle it costs about as much as the fetch it is estimating, which would roughly double a multi-day run to populate two display columns. One `UID SEARCH` per mailbox is already required for resume and costs one round trip regardless.

Consequently there is no `BYTES` or `ATTACHMENTS` column, no attachment estimate, and no `.manifest` file. ADR-0065's `pull_manifest` bisection and ADR-0034's fix both remain live for `email-sync` until ADR-0147, and neither is touched.

### 8. Observability, applied up front

ADR-0130 and ADR-0141 both document the same shape of failure: a job shipped without phase logging or span propagation, whose first real production problem was then undiagnosable from `pigeon.jsonl`. Those lessons are applied here before the first run, not after one.

- **Span propagation first.** Every `tokio::spawn`/`spawn_blocking` captures `tracing::Span::current()` and `.instrument()`s the future. This is exactly ADR-0141's bug -- all 1,449 of `transform`'s per-file events landed with no `command` span, so the `analyze-job-run` skill's documented `spans[].command` filter returned zero hits for every one of 483 failures -- and ADR-0098's before it, for `deduplicate`. Third time, done first.
- `#[tracing::instrument]` on `gather_pending`, `connect_with_retry`, and `process_batch_on_session`, with per-object push spans at `level = "debug"` per ADR-0099's log-volume finding (143k lines down to a handful).
- `tracing::info!` phase boundaries: run start with identity and pending counts, per-identity start and finish, gate pause and resume with `paused_ms`, and end-of-run totals. ADR-0130 found `transform` emitted **zero** info lines, leaving its log with no run data at all.
- `record_phase_count("email-import", phase, outcome, n, None)` over phases `connect`, `examine`, `fetch`, `extract`, `push`, `gate`; `pigeon_upload_*` arrives from `CopyTo { emit_upload_metrics: true }`. `source_bucket` is `None` -> `NO_BUCKET`, this being an IMAP-sourced job.
- **`set_macro_phase` is never called**, following ADR-0130's precedent for `transform`. The gauge encodes ADR-0019's one-way local-work-then-upload transition, which a job that pushes inline never performs. Emitting it would misreport the pipeline's shape, and emitting it once at start would assert a transition that never happens.
- `FailureBreakdown { connect, examine, fetch, missing_file, extract, push, gate_exhausted }`, guaranteed to sum to the flat `failed` count, per ADR-0033's per-category replacement of a bare counter.
- `ResourceSampler::spawn(5s)` held for the whole `dispatch_async`, so every early return drops and aborts it (ADR-0073).

### 9. Wizard flow

`ResourceSampler` -> `shared_wizard::load_keyring()` -> **`shared_wizard::prepare_rclone(&store)`** -> `IdentitiesInput` -> `DestinationInput` -> `LocalOutputInput` -> `MessagesPerMinuteInput` -> `BatchSizeInput` -> `MaxConnectionsInput` -> `PushConcurrencyInput` -> `report_upload::resolve(...)` -> pre-run table -> `ConfirmInput` -> run -> `write_report` -> `log_run_outcome` -> `upload_run_artifacts`.

This is the **first email job to call `prepare_rclone`**, ADR-0145 §5's helper: the two existing ones are `minio`-only and need neither the binary nor the generated config.

Non-interactive fallbacks follow the established split: `--identities` and `--destination` error, being the two inputs with no safe default; every numeric flag falls back to its default rather than erroring, per `UploadConcurrencyInput`'s ADR-0091 §3 reasoning, since unattended operation is this job's primary mode rather than an afterthought.

The report is `email-import-report.txt`, a TSV of `identity`, `mailbox`, `uid`, `outcome`, `stage`, `detail` plus summary lines, then ADR-0100's `--report-bucket` upload of report, transcript, and `pigeon.jsonl`.

### 10. ADR-0147 will remove `email-sync` and `email-pull`

A separate ADR and PR, after this one lands. The forced-deletion census is already traced, so it is recorded here rather than left for that implementation to rediscover -- ADR-0131 §Forced deletions' precedent, and the `clippy -D warnings` zero-consumer check is the mechanical proof the census is complete:

- **Relocate before delete:** `email_sync::sink`'s eight live functions and `manifest::{Batch, split_into_batches}` move into `email_import/`. `sink::run` is **dropped outright** -- zero callers (Context).
- **`src/commands/job/upload.rs` falls to zero consumers** and must be deleted. Its only two are these jobs; `deduplicate` left for `core::rclone` in ADR-0133.
- **`core/crypto.rs` and the `encryption-key` keyring kind go too** -- `Entry::EncryptionKey`, `pigeon keyring add`/`modify`/`delete encryption-key`, and the `aes-gcm-siv`/`hkdf`/`hmac` dependencies. Client-side encryption for any data path dies with `email-sync`, so leaving a keyring kind nothing can consume would be worse than removing it: the clean sweep ADR-0134 performed for `decrypt-files`. This explicitly reverses ADR-0025, ADR-0026, and ADR-0027.
- **`htmd` is pruned** -- its sole use in the crate is `email_sync/transform.rs:167`. Nothing in the toolchain detects an unused dependency (ADR-0131's own finding).
- **`minio` and `keyring/bucket/client.rs` stay.** `report_upload.rs` still uploads ADR-0100's artifacts through them. **Explicitly not dead**, despite every job-level data path having left, so a later reader does not over-prune.
- Also lost, stated rather than silently regressed: `.eml`-to-Markdown transform with frontmatter taxonomy (ADR-0006/0007), email message and attachment deduplication (ADR-0012/0030), and the `BYTES`/`ATTACHMENTS` pre-run estimate (ADR-0032/0034).

`pigeon` goes to 7 job types with this ADR and back to 5 with ADR-0147.

## Consequences

- **Two command-nesting shapes coexist** for one PR: `email import` nested, `email-sync`/`email-pull` flat. Transient by design (§1) -- ADR-0147 removes the other two rather than renaming them, so no shipped spelling ever breaks.
- **Three functions are duplicated against this codebase's own third-consumer rule** (§2). An explicit, reasoned exception with a one-PR lifetime, not an oversight; ADR-0147 returns each to a single consumer, at which point the rule would have demanded the hoist be undone.
- **A run is slower than either sibling job by construction**, and that is the feature. At the default 120 messages/minute a 200k-message account takes about 28 hours. Anyone wanting the old behavior raises `--messages-per-minute`; there is deliberately no "unlimited" value, since the absence of a cap is what this job exists to fix.
- **No pre-run byte or attachment estimate** (§7), so the wizard cannot show how much data a run will move or how long it will take. A real regression against ADR-0032's table, accepted because recovering it costs a second full-corpus traversal at throttled speed. Filed per ADR-0031.
- **`RecoveryGate` is a second implementation of ADR-0144's mechanism**, not a hoist of it. Correct under the duplicate-until-the-third-consumer rule, and the one real cost is that a future fix to the pattern must be applied in two places until that third consumer arrives. Filed per ADR-0031.
- **Attachment bytes are stored twice** at the destination -- once inline in the `.eml`, once unpacked (§3). Inherent to delivering both artifacts with no dedup pass, and stated rather than discovered from a bucket-size bill.
- **This job cannot be pointed at a non-rclone destination.** It depends on `rclone` on `PATH` and on ADR-0136's generated config, unlike both sibling jobs. That is the direction of travel ADR-0132/0133 set, and after ADR-0147 nothing in the crate's data paths will use `minio` at all.

## Out of scope

- **Deduplication of any kind** -- messages or attachments. A permanent scoping decision, not a deferral: it is the single largest source of complexity in both sibling jobs (`email_sync/dedup.rs` at 644 lines, plus ADR-0012's `also-in:` frontmatter mechanism and ADR-0030's fix to it), and the whole point of this job is to move bytes. Byte-identical duplicates at the destination are expected and are a downstream `job run deduplicate` concern, which is exactly the compose-separate-verified-steps posture ADR-0103 and ADR-0131 adopted.
- **Indexing, renaming, and any content transform** -- no Markdown, no frontmatter, no taxonomy tags, no date extraction, no filename canonicalization. `<uid>.eml` is the literal server UID. Permanent, for the same reason.
- **Encryption**, permanently, with no `--encryption-key` flag ever (§1) -- ADR-0081 §4's precedent, and ADR-0147 removes the mechanism from the crate entirely.
- **A pre-run byte/attachment estimate** (§7). Genuinely deferred rather than permanently excluded: a cheaper approximation (sampling, or `RFC822.SIZE` alone without `BODYSTRUCTURE`) might pay for itself. Filed as an issue alongside this ADR per ADR-0031.
- **Hoisting `RecoveryGate` into `core::rclone`** (§4). Deferred to its third consumer per this codebase's standing precedent, not an oversight. Filed per ADR-0031.
- **Server-side IMAP-to-S3 transfer.** No such primitive exists; every byte necessarily lands locally first, which is why §5's GC rather than streaming is what bounds disk.
- **Mailbox selection.** Every selectable mailbox is always imported, as in both sibling jobs. A `--mailboxes` filter is a plausible future flag but nothing needs it yet.

## Amends

Nothing. This ADR adds a job and amends no existing decision: §7's divergence from ADR-0032/0034 and §5's from ADR-0124 are both scoped to this new job, leaving the behavior those ADRs describe intact where they describe it. Draws on without amending: ADR-0021 and ADR-0081 (wizard and job shape), ADR-0071 and ADR-0080 (per-identity connection cap), ADR-0100 (report upload), ADR-0114 (`--non-interactive`), ADR-0132 and ADR-0136 through ADR-0140 (`core::rclone` and alias addressing), ADR-0143 and ADR-0144 (bounded recovery, pure testable thresholds), ADR-0145 (`load_keyring`, `prepare_rclone`).

ADR-0147 will carry the `## Amends` entries for the removals §10 enumerates.

## Verification

- `mise run ci` clean (fmt-check + lint + test); `cargo clippy --all-targets --all-features -- -D warnings` clean.
- New unit tests in `email_import/pace.rs`: `reserve` spaces consecutive single reservations by exactly one `interval`; `reserve` charges `interval * count` for a batch; `reserve` does not pay back idle time, yielding `now` rather than a burst after a long gap; `Pacer::acquire` advances a paused virtual clock by the expected total under `#[tokio::test(start_paused = true)]` (the dev-dependency `tokio` already carries `test-util`, added for ADR-0144's gate tests).
- New unit tests in `email_import/gate.rs`: `is_server_overloaded_error` matches each named signature case-insensitively and rejects an unrelated error; a task losing the election parks until the prober reopens the gate; a round past the budget returns `GiveUp` and stays latched. Both gate tests run on a paused clock, so the recovery backoff is asserted without being slept.
- New unit tests in `email_import/manifest.rs`: `.pushed` round-trips across separate appends; `done_uids` keys on `(mailbox, uid)` composites; a malformed line is skipped; a missing file reads as empty; `clear_checkpoint_for_mailbox` removes one mailbox's rows only; `split_into_batches` honors an explicit `--batch-size`.
- New unit tests in `email_import/attach.rs`: stages an attachment from a literal multipart `.eml` fixture; returns `Err` for unparseable input; a MIME filename containing a path separator is sanitized rather than escaping its directory (ADR-0013).
- New unit test in `email_import/push.rs`: `deliver_one` against two local temporary directories, skipped with `eprintln!("skipping: rclone not found on PATH")` when the binary is absent, following `transform/push.rs`'s existing test.
- New unit tests in `email_import/wizard.rs`: each numeric input's `flag_value` overrides its default and its `non_interactive_fallback` returns that default; `--identities` and `--destination` both error non-interactively; the default local output path.
- New tests in `tests/cli.rs`: `job run --help` lists `email`; `job run email --help` and `job run email import --help` list the command and its full flag set; **negative** assertions that `--encryption-key`, `--upload-only`, `--concurrency`, and `--tpslimit` are absent, pinning §1's scoping the way ADR-0081's own `--encryption-key` test does; a missing `--identities` and a missing `--destination` each fail with exit code 1 and the exact message; an unknown destination alias fails with `no bucket-config named`; a `..` prefix segment is rejected; an emptied `PATH` fails with `was not found on PATH`. No test reaches a real IMAP server or rclone transfer -- rclone's `--retries 5 --low-level-retries 20` turns even a refused connection into minutes of backoff (ADR-0145's recorded test-cost constraint).
- Manual, against a real identity and bucket: `mise run pigeon -- job run email import --identities <alias> --destination <bucket>:eml/ --messages-per-minute 60 --non-interactive --report-bucket <alias>`. Confirm both key layouts of §3 appear at the destination; that `--local-output` stays near-empty for the whole run, proving §5's unconditional GC; that `.pushed` grows monotonically; that an interrupt-and-rerun refetches nothing already delivered; and that `pigeon.jsonl` carries `spans[].command == "job.email-import"` on every per-object event, which is §8's span propagation and the precise thing ADR-0141 had to repair after the fact.
