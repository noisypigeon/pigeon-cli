# ADR-0024: concurrent, exclusive upload phase with progress reporting

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-24.
- **Status**: Accepted.

## Context

`pigeon job run email-sync`'s upload phase (`run_upload_phase`, `src/commands/job/email_sync/worker.rs`) was explicitly scoped as sequential-only by ADR-0021 §7/"Out of scope": each identity is fully deduped and then fully uploaded, one identity at a time, and within an identity, files upload one at a time in a plain `for path in files` loop. Two problems, observed from real use and named directly by the user:

1. **No concurrency for uploads at all**, even though fetch/transform already established a real concurrency model (ADR-0021 §6/§7: a worker pool sized by `--concurrency`) and the wizard already asks for a concurrency value that upload never uses. A real run (209 MB across 568 messages, `--concurrency 16`) showed fetch/transform finishing quickly while upload — invisible and single-threaded — became the run's actual bottleneck.
2. **No progress bar for uploads.** Fetch has one per mailbox (`sink::new_progress_bar`, ADR-0013, kept redraw-safe by ADR-0015); upload has none — the same run above produced no visible feedback at all during its upload phase.

This ADR reverses the "stays sequential" half of ADR-0021 §7's upload scoping decision. Dedup itself is untouched and stays sequential — it has real cross-message ordering semantics (canonical-occurrence selection) per ADR-0021 §7's own reasoning, which this ADR does not revisit.

Two design questions were resolved directly with the user before writing this decision:

- Should concurrent upload stay **scoped per-identity** (identities still handled one after another, but each identity's own files upload concurrently), or become **fully cross-identity** (wait for every identity's dedup pass to finish, then run one shared upload pool over every identity's leftover files at once, mirroring how fetch/transform already spans every identity)? → **Cross-identity shared upload pool.**
- Should the concurrent upload phase **mirror fetch/transform's manual worker-pool shape** (`Arc<Mutex<VecDeque<_>>>` plus `tokio::spawn`'d persistent workers — justified there by expensive, rate-limited IMAP session reuse), or use a **simpler `futures::stream::buffer_unordered(concurrency)`** over independent upload tasks (uploads have no session to reuse — `client::upload_if_changed` already builds a fresh S3 client on every call)? → **`stream::buffer_unordered`.**

## Decision

### 1. Dedup passes run first for every identity (unchanged, sequential); uploads become one cross-identity phase after

`run_email_sync_job`'s per-identity loop still runs each identity's dedup pass sequentially, exactly as today. What changes: instead of also calling the upload phase inline per identity, each identity's dedup step additionally builds a list of **upload tasks** — one per not-yet-uploaded file in that identity's final output tree — and appends them to one job-wide list. Once every identity's dedup pass (and task-building) is done, exactly one call to the new concurrent upload phase uploads every task from every identity together, at the job's already-chosen `--concurrency`.

```rust
/// One file queued for upload, carrying everything the upload phase needs
/// without re-deriving it: which identity's `.uploaded` index to commit
/// into, the absolute path to read, and its already-computed S3 key.
struct UploadTask {
    staging_dir: PathBuf,
    path: PathBuf,
    key: String,
}
```

Building the task list (replacing today's inline call to the upload phase) happens right after each identity's dedup pass, inside the existing per-identity loop:

```rust
let uploaded_index = UploadedIndex::load(&ctx.staging_dir)?;
for path in collect_files(&identity_dir)? {
    let key = upload_key(&ctx.output_dir, &path)?;
    if !uploaded_index.contains(&key) {
        tasks.push(UploadTask { staging_dir: ctx.staging_dir.clone(), path, key });
    }
}
uploaded_indexes.insert(ctx.staging_dir.clone(), Arc::new(Mutex::new(uploaded_index)));
```

This is the same `UploadedIndex`/`upload_key`/`collect_files` machinery the upload phase already uses today (`src/commands/job/email_sync/worker.rs`, `core::data::collect_files`) — only *when* it runs and *what it produces* changes: a task list handed to one shared phase, instead of an upload done inline per identity.

### 2. Exclusivity: each file is uploaded by exactly one worker, by construction

The concurrent upload phase processes `tasks` via:

```rust
let bar = sink::new_progress_bar("upload".to_string(), tasks.len() as u64, multi_progress);
let summary = stream::iter(tasks)
    .map(|task| upload_one(task, &uploaded_indexes, bucket_config, secret, &bar, multi_progress))
    .buffer_unordered(concurrency.max(1))
    .fold(UploadSummary::default(), |mut summary, outcome| async move {
        // accumulate outcome into summary
        summary
    })
    .await;
```

`stream::iter(tasks)` yields each `UploadTask` **exactly once**, moved by value into its own future; `buffer_unordered` polls up to `concurrency` of those futures concurrently but never re-polls or duplicates an item. This is what actually guarantees two workers never upload the same file twice — there is no shared, poppable work queue to race over in the first place, unlike fetch/transform's `Arc<Mutex<VecDeque<_>>>` (needed there specifically because IMAP batches are pulled by long-lived, session-holding workers). No `tokio::spawn` is used or needed: `buffer_unordered` interleaves await points on the calling task, which is exactly the right tool for overlapping many I/O-bound HTTP requests without needing real OS-thread parallelism — a blocking `fs::read` per task briefly occupies that one thread, same as today's sequential version, just interleaved with other tasks' in-flight network waits.

### 3. Safe, deterministic `.uploaded` persistence under concurrency

The only genuinely shared mutable state is each identity's `.uploaded` index (`UploadedIndex`, `src/commands/job/email_sync/worker.rs`) — multiple in-flight uploads can belong to the *same* identity and would otherwise race on its in-memory set and its on-disk file. Fix: each identity gets one `Arc<Mutex<UploadedIndex>>` (built once, alongside its task list, in §1), looked up by `task.staging_dir` inside each upload future before committing a success. This mirrors this codebase's existing `Arc<Mutex<T>>` idiom for cross-task shared state (`core::data::ContentIndex` already documents and tests concurrent-commit safety the same way). Locking is scoped per identity, not job-wide, so uploads for different identities never contend on each other's lock.

Determinism here means: regardless of which worker finishes which file first, the final durable state — the *set* of keys recorded in each identity's `.uploaded` file, and the job's aggregate `uploaded`/`unchanged`/`upload_failed` counts — is identical. Timing only affects the order lines are appended to `.uploaded`, never which files end up recorded or double-recorded. A failed upload is not committed (unchanged from today), so a partial or interrupted run remains safely resumable on re-invocation.

### 4. Progress bar, following ADR-0013/0015's established pattern

One shared bar, `sink::new_progress_bar("upload".to_string(), tasks.len() as u64, multi_progress)`, registered on the same `MultiProgress` instance `run_email_sync_job` already creates for fetch/transform (kept alive through the dedup/upload section instead of implicitly dropped once the fetch/transform workers finish, as it already lexically is — no lifetime change needed, just threading `&multi_progress` into the new upload code path). One bar spans every identity's uploads, incrementing by one per completed task (success, unchanged, or failed alike — matching `sink::fetch_uids`'s "increment on completion, not just on success" convention). A single global bar is the natural shape now that upload is a genuine cross-identity phase (§1), rather than one bar per identity.

Per ADR-0015's precedent — `MultiProgress` corrupts every live bar's redraw if anything bypasses it with a raw `println!`/`eprintln!` while bars are active — the existing `eprintln!("Warning: upload failed for {}: {err}", ...)` becomes `multi_progress.println(...)`. This is now load-bearing, since upload bars are live for the first time; previously it was harmless only because the upload phase itself had no bars to corrupt.

### 5. Concurrency reuse, no new flag

The upload phase's `buffer_unordered(concurrency)` uses the exact same `concurrency: usize` value already resolved by the wizard for fetch/transform (`ConcurrencyInput`, `src/commands/job/email_sync/wizard.rs`) — no new CLI flag, no second concurrency prompt. This is the literal sense in which upload "builds on the same concurrency model": one number, chosen once, governs how much is in flight at every concurrent phase of the job.

### 6. Individual upload failures retry with backoff

Each upload attempt (`client::upload_if_changed`) is wrapped in the same `retry_with_backoff` helper `connect_with_retry` already uses for IMAP connects (`src/commands/job/email_sync/worker.rs`) — generic over any `Result`-returning async closure, so reusing it here needs no new abstraction, just a second call site with its own attempt count/backoff duration tuned for HTTP rather than IMAP. A file that still fails after exhausting retries is counted toward `upload_failed` exactly as before (not committed to `.uploaded`, safely retried again on the job's next invocation) — this changes *how many times* a transient failure is given a chance to succeed within one run, not the failure-accounting model itself.

## Consequences

- Upload throughput scales with `--concurrency` for the first time — directly addresses the real-world case in Context (209 MB across hundreds of files uploading one at a time with no feedback).
- A user finally sees upload progress, consistent with every other phase of the job.
- The upload phase's signature changes from "one identity's files, called once per identity" to "every identity's tasks, called once for the whole job" — its caller (`run_email_sync_job`) restructures accordingly, but `UploadedIndex`, `upload_key`, and `collect_files` are all reused unchanged.
- This reverses the "stays sequential" half of ADR-0021 §7/"Out of scope"'s upload scoping; that ADR is left unedited as historical record, per this project's established convention (e.g. ADR-0019 superseding ADR-0011/0012, ADR-0021 superseding ADR-0007/0012/0014).
- Dedup remains untouched and sequential — its own real ordering semantics (ADR-0021 §7) are out of scope for this ADR.
- The first upload can no longer start until every identity's dedup pass has finished (a consequence of the chosen cross-identity scope), whereas today's sequential pipeline lets identity 1 start uploading while identity 2 is still being deduped. Dedup is a local, single-pass file-move operation with no network I/O, so this delay is expected to be small relative to the upload phase itself; not measured or guaranteed here.
- Upload failures get the same bounded-retry treatment IMAP connects already have, absorbing transient S3/network blips within one run instead of counting them as failed on the first error.

## Out of scope

- Any change to dedup's sequential, single-pass design.
- A new CLI flag or separate concurrency knob for uploads specifically. ([#30](https://github.com/noisypigeon/pigeon-cli/issues/30))
- Any on-disk format change to `.uploaded`, `keyring.toml`, or any other existing dotfile.

Implementation is a separate, later task.
