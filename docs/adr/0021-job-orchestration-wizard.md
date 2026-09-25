# ADR-0021: Job orchestration wizard, replacing `email sync`

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-24.
- **Status**: Accepted.

## Context

`pigeon email sync` (ADR-0007, extended by ADR-0011/0012/0014/0019/0020) is today's only end-to-end pipeline: it authenticates, lists mailboxes, and runs a fetch→transform→dedup→upload flow driven entirely by CLI flags (`--local-output`, `--remote-output`, `--concurrency`, `--debug sink|transform|upload`). It works, but three problems have become clear from using it:

1. **Concurrency is capped by mailbox count, not by `--concurrency`.** `email::sync::run_local_async` does:

   ```rust
   stream::iter(mailboxes)
       .map(|(mailbox_name, delimiter)| tokio::spawn(sync_mailbox(...)))
       .buffer_unordered(concurrency.max(1))
   ```

   — one `tokio::spawn` task per *mailbox*. A user with 2 mailboxes and `--concurrency 8` only ever runs 2 workers, regardless of how many thousands of messages are pending. There is no portioning of work below mailbox granularity — "anything goes," as long as it fits in one task per mailbox.

2. **There's no up-front visibility into how much work exists, or how long it will take**, before committing to a run. A user has to just start `sync` and watch it go; there's no manifest of what's pending, no identity-selection step, no estimate to inform a concurrency choice.

3. **Dedup runs inline, mid-transform, under a shared lock.** `transform_one` is called from each mailbox's concurrent task while holding `message_index.lock()` and `attachment_index.lock()` (ADR-0012), serializing dedup-index access across every concurrent worker for the whole duration of each message's transform — real contention that grows with `--concurrency`.

Separately, the user wants this rebuilt as a **job orchestration system**: email sync as the first job, but with wizard-driven identity selection, manifest pulling, checkpoint management, staging-dir configuration, concurrency selection with time estimates, proper work scheduling, and a summary/confirm step — with room for other job types later.

Three design questions were resolved with the user directly before writing this decision:

- Should this be built as a **generic engine with email as the only exposed CLI surface**, or a **real new top-level `pigeon job` command group now**? → **A real `pigeon job` command group now.**
- Should the wizard be a **new command alongside today's scriptable `email sync`**, or **become the primary way to run a sync**, demoting today's flag-driven invocation? → **The wizard becomes the primary flow**; `pigeon email sync` is retired in its favor.
- Should dedup stay inline in transform, or **move to its own pass after every message is transformed** (more disk I/O, but a lock-free concurrent transform phase)? → **Separate post-transform dedup phase.**

This ADR supersedes ADR-0007's `--debug`-flag phase-isolation model, ADR-0012's dedup-during-transform timing, and ADR-0014's per-mailbox concurrency model. Those ADRs are left unedited as historical record; this document is the new decision governing their areas.

## Decision

### 1. New `pigeon job` command group; `pigeon email sync` is removed

`pigeon job run email-sync [args...]` becomes the one way to run an email sync — interactively as a wizard by default, or non-interactively when all required inputs are supplied as flags (see §8). `pigeon email sync`, including its `--debug sink|transform|upload` phase-isolation modes, is removed rather than kept as a parallel entry point. Checkpointed auto-resume (§4) makes explicit phase-selection unnecessary: re-running a job that was interrupted mid-fetch naturally continues fetching, with no `--debug` flag required to reach that state on purpose.

`pigeon email authenticate`/`list-identities` are unaffected — only `sync` moves.

### 2. `src/job/` is job-type-agnostic infrastructure; email is its first consumer

Per ADR-0008's per-command-group module convention, add `src/job/` as a sibling to `src/email/` and `src/dataops/`:

- `src/job/cli.rs` — `JobArgs { command: JobCommands }`, `JobCommands::Run(RunArgs)`, `RunArgs { #[command(subcommand)] job_type: JobType }`, `JobType::EmailSync { identities, local_output, remote_output, concurrency, yes }` (matching the nested-subcommand shape already used by `DataopsCommands::BucketConfig`).
- `src/job/commands.rs` — dispatch, delegating to the job-type implementation.
- `src/job/wizard.rs` — the interactive flow (§5): identity selection, manifest summary, concurrency + estimate prompt, confirm. Operates over a job-type-agnostic summary/plan structure, even though only email populates it today.
- `src/job/manifest.rs` — manifest and checkpoint types and persistence (§3, §4), and the batch scheduler/worker pool (§6).
- `src/job/email_sync.rs` — the email-specific job implementation: builds an email manifest, and plugs `email::sink`, `email::transform`, `dataops::dedup`, and `dataops::client` into the generic scheduler.

`src/cli.rs`'s `Commands` enum gains `Job(JobArgs)` alongside `Email`/`Dataops`; `src/commands/mod.rs::dispatch` gains a matching arm.

`email::sync`'s current *orchestration* — `run`, `run_local_async`, `sync_mailbox`, `run_upload_async`, `UploadedIndex` — is removed; that scheduling model is exactly what's being replaced. The lower-level functions it called are reused unchanged from the new orchestration layer: `email::sink::fetch_uids` (or its message-fetching equivalent), `email::transform::transform_one`, `dataops::dedup::ContentIndex`/`amend_frontmatter_for_duplicate`, `dataops::client::upload_if_changed`.

No second job type is implemented in this ADR; the module boundary is shaped to admit one without restructuring `src/job/`'s infrastructure pieces.

### 3. The manifest: what work exists

Pulled via IMAP `UID FETCH` requesting `UID` and `RFC822.SIZE` only — no `BODY.PEEK[]`, so message content is never transferred. This is the same fetch mechanism `email::sink` already uses (a raw macro string passed to `uid_fetch`), just a different data-item list. For each selected identity, the manifest records, per mailbox, the byte size of every pending UID (i.e. not already checkpointed as done, per §4). It feeds the wizard's summary display and the concurrency-estimate heuristic (§9).

### 4. Checkpoints: how much is already done

The manifest answers "what work exists"; a separate, persisted checkpoint answers "how much of it is already done" — distinct concerns, both needed. Once pulled, the manifest itself is persisted to the staging dir, so a resume that happens before any work started doesn't need to re-query the server. Fetch/transform progress is tracked per `(mailbox, UID)`, replacing `.processed`/`.uidvalidity`'s role (ADR-0005) for jobs run through this system. Dedup and upload progress remain tracked at the corpus/file level, matching existing `.message-hashes`/`.attachment-hashes` (ADR-0012/0020) and `.uploaded` (ADR-0019) precedent — a UID's identity blurs into a canonical file once dedup runs, so per-UID tracking stops making sense past that point.

### 5. The wizard flow

In order: select one or more identities → **prompt for the local-output directory** (an editable `Input` defaulting to a directory under the OS temp dir) → pull and persist the manifest → show a summary (message count and total size, per identity and per mailbox) → **prompt whether to upload to a bucket-config** (a `Confirm`; if accepted, a `Select` among configured bucket-configs) → prompt for concurrency, displaying a heuristic time-to-completion estimate at a few candidate concurrency levels (§9) → confirm/accept → execute the four-phase pipeline (§7) with progress reporting (reusing the existing `indicatif` `MultiProgress` pattern).

**Amendment** (post-implementation, first real use): the initial version of this ADR listed only identity selection, concurrency, and the final confirm as interactive steps, leaving `--local-output`/`--remote-output` as flag-only inputs with silent defaults (no prompt, matching the old `email sync` command). Running the wizard end to end showed that was a real usability gap: nothing told the user where mail was being staged, and nothing offered to upload it, so a run could silently complete as local-only with no indication that was the case. Both are now real wizard steps when their flag is omitted — see §8 for how this interacts with non-interactive invocation.

### 6. Scheduling: portioned work, not whole mailboxes

Each mailbox's pending UIDs are split into roughly equal-sized batches — sized so the batch count comfortably exceeds `concurrency` — instead of one task per mailbox. A fixed-size pool of `concurrency` workers pulls batches from a shared queue until it's drained. Fetch stays a single batched `uid_fetch` call per batch (still round-trip-efficient, each worker holding its own IMAP session), but actual parallelism now scales with total pending message count, not mailbox count — directly fixing the gap identified in Context item 1: a user with 2 mailboxes and thousands of pending messages now actually gets `concurrency` workers busy, not 2.

**Addendum (found in real use, first high-concurrency run):** the initial implementation didn't actually match this section's own text — it spawned a fresh `tokio::spawn` task *and a brand-new IMAP connection* for every batch, rather than a genuine pool of `concurrency` long-lived workers each "holding its own IMAP session" across every batch it processes. At `--concurrency 16` against 500 pending Gmail messages, that meant ~70+ fresh TCP+TLS+LOGIN handshakes against one account over the run instead of ~16, which reliably triggered Gmail's connection/login-rate throttling (observed as simultaneous `timed out connecting to imap.gmail.com:993` errors — Gmail's typical response to exceeding its per-account connection limits is to simply not respond to the new connection, not send a clean IMAP-level rejection). This is now corrected to the genuine persistent-worker-pool model this section already specified: each worker holds one IMAP session for as long as consecutive batches it pulls belong to the same identity, only reconnecting on an identity change and only re-`EXAMINE`-ing (no new connection) on a mailbox change within the same identity.

A second, additive change from the same finding: connection attempts (a worker's initial connect/reconnect, and `gather_pending`'s per-identity manifest connection) now retry a bounded number of times with backoff before giving up, since a burst of `concurrency` workers all connecting within the same instant at job start can still transiently exceed a provider's concurrent-connection cap even with per-worker session reuse. A batch that still can't be reached after retries is counted as failed rather than aborting the whole job — every other batch's work is already durably checkpointed, so a transient throttle now degrades to "N messages failed, re-run to pick them up" instead of failing the entire run.

### 7. Four-phase pipeline

Fetch and transform run concurrently across the worker pool from §6, and fully complete (for the whole selected work set) before dedup starts. Because dedup no longer runs inline (§ Context item 3, confirmed decision above), transform needs no cross-worker lock at all — the `Arc<Mutex<ContentIndex>>` held across `transform_one` today is gone entirely. Dedup then runs as one sequential pass over the completed local corpus, extending ADR-0019's existing "don't over-engineer concurrency into every phase" precedent: dedup's canonical-occurrence selection has real ordering semantics that get materially harder to reason about under concurrency, so it stays a single pass. Upload stays sequential, unchanged from ADR-0019, matching the user's own framing ("wait until they're all completed, then dedup. Then upload.") which describes concurrency applying to fetch/transform only.

### 8. Non-interactive operation is preserved

"Wizard is primary" does not mean "wizard is the only way to run this." The same TTY-vs-piped-stdin fallback already established for `dialoguer::Password`/`Confirm` throughout this codebase (`email authenticate`, `dataops bucket-config new/edit/remove`) applies here: supplying all of a job's inputs as flags (`--identities`, `--local-output`, `--remote-output`, `--concurrency`, `--yes`) skips the interactive prompts entirely. `mise run pigeon -- job run email-sync --identities work,personal --local-output ~/backup --concurrency 8 --yes` remains fully scriptable — the wizard and the flag-driven path are the same code, differing only in whether a given input comes from a prompt or a flag.

Not every omitted input follows the same non-interactive rule, though. `--identities`/`--concurrency` are core "what to do" decisions with no prior default, so omitting either one outside a TTY (§5's amendment doesn't change this) is a hard error naming the missing flag — silently guessing would be actively dangerous in a scripted/cron context. `--local-output`/`--remote-output` are different: both already had a safe, established default before §5's amendment added prompts for them (a temp directory; no upload). So an omitted `--local-output`/`--remote-output` outside a TTY silently falls back to that same prior default — no prompt, no error — keeping every already-scripted invocation working unchanged. The prompts these two flags gained are additive convenience for interactive use, not a new requirement.

### 9. Concurrency estimate: heuristic, not measured

No historical throughput data exists anywhere in this codebase today. The estimate shown in the wizard is computed from a simple, explicitly-labeled per-message time assumption (illustrative, not calibrated), applied to the manifest's message count/total size at a few candidate concurrency levels. It is presented as a rough guide, not a guarantee. Building real historical-throughput tracking to improve accuracy is deferred (see Out of scope).

### 10. Addendum: transform-phase filename collisions (closing a gap in §7)

§7 justifies removing the transform-phase lock (the `Arc<Mutex<ContentIndex>>` pair held across today's `transform_one` call) solely on dedup-index contention grounds. That is incomplete: the lock's own code comment in `email::sync::sync_mailbox`, and ADR-0019's decision explicitly confirming and preserving it, both establish that the lock is relied on for a *second*, independent reason — it also serializes `dataops::transform::unique_path()`'s check-then-write logic against the shared, flat, identity-scoped output tree (ADR-0006). Two concurrent workers could otherwise resolve the same "free" filename before either has written it. Removing the lock per §7 without addressing this second hazard would reintroduce that race under concurrent transform.

**Resolution**: transform-phase output no longer targets the shared flat output tree at all. Each worker writes to a **UID-keyed staging tree** instead — `<staging-dir>/transformed/<mailbox-relpath>/<uid>.md` and `<staging-dir>/transformed/<mailbox-relpath>/<uid>/attachments/...` — where `(mailbox, uid)` is inherently unique per IMAP's own guarantees, so concurrent workers never contend over a name during transform. No lock is needed for this, by construction rather than by mutex. `unique_path()` itself — the actual collision-prone, human-readable-stem namer — moves entirely into the post-transform dedup pass (§7), which is already single-threaded; it becomes that pass's only caller, closing the race by having exactly one thread ever invoke it.

This also means §2's description of `transform_one` as "reused unchanged" needs qualification: the parsing/rendering engine (message parsing, HTML-to-Markdown, frontmatter rendering) is unchanged, but its signature and responsibilities change — it stops taking `message_index`/`attachment_index` parameters, stops making any dedup decision inline, and writes to the UID-keyed staging location instead of the final flat tree. The post-transform dedup pass takes over both the dedup decision and the final `unique_path`-based placement that `transform_one` used to do itself.

## Consequences

- `pigeon email sync` is removed — a breaking, no-migration-shim change, consistent with this project's established pattern (e.g. ADR-0016/0017's renames). Every `mise run pigeon -- email sync ...` invocation in this project's own history, docs, and muscle memory needs to become `pigeon job run email-sync ...`.
- A new on-disk manifest/checkpoint format is introduced under the staging dir, distinct from and replacing `.processed`/`.uidvalidity` for jobs run through the new system.
- Transform becomes lock-free and embarrassingly parallel across workers — a direct, positive side effect of moving dedup out of the transform path. Per §10, this required introducing a UID-keyed staging tree so removing the lock doesn't reopen the `unique_path` TOCTOU race that lock also happened to prevent.
- The confirmed concurrency under-utilization (capped by mailbox count) is fixed by scheduling batches instead of whole mailboxes.
- `email::sync`'s current file effectively disappears as an orchestration layer; its lower-level callees survive with a new caller in `src/job/email_sync.rs`.
- The concurrency-estimate feature sets only rough expectations; it is explicitly not backed by measured data yet, and should be described to users as such.

## Out of scope

- Concurrent dedup or concurrent upload phases — both stay sequential, matching the user's own literal phrasing and ADR-0019's existing scoping decision.
- A generic job-type plugin/registry CLI (e.g. `pigeon job list-types`) — only `email-sync` is implemented; the module boundary allows more, but no second job type is built now. ([#28](https://github.com/noisypigeon/pigeon-cli/issues/28))
- Historical-throughput-based estimate refinement — the estimate stays a static heuristic in this ADR. ([#29](https://github.com/noisypigeon/pigeon-cli/issues/29))
- Any change to `dataops`'s own `bucket-config` commands.

Implementation is a separate, later task.
