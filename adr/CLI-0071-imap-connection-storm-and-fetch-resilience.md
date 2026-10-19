# CLI-0071: per-identity connection cap, batch retry, and warning collapse for `job run email-sync`

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-27.
- **Status**: Accepted.

## Context

A real `mise run pigeon -- job run email-sync` run against 7 identities at `--concurrency
16` degenerated into dozens of repeated login errors and thousands of lines of duplicate
warning spam, and had to be interrupted. Three compounding problems, all traced to current
code:

1. **Connection storm.** `Error: login failed: no response: code: Some(Alert), info:
   Some("Too many simultaneous connections. (Failure)")` repeated dozens of times, across
   several identities (a Fastmail identity's `B-Sources/kara.graysen`/`B-Sources/willow.graysen`
   folders, and a Gmail identity's `[Gmail]/All Mail`). `--concurrency` sizes one single
   worker pool shared across *every* selected identity's mailboxes combined
   (`run_email_sync_job`, `worker.rs:552-580`), and `all_batches` is built by iterating
   identities in order and extending (`worker.rs:560-564`) — so all of one identity's
   batches sit contiguously at the front of the shared queue. Once that identity has enough
   pending batches, all `concurrency` workers can pop its batches and open a login to that
   one account within the same instant — comfortably over most providers' simultaneous-IMAP
   connection caps (Gmail documents 15). CLI-0021 §6's addendum already named this exact
   risk ("a burst of concurrency workers all connecting within the same instant at job
   start can still transiently exceed a provider's concurrent-connection cap") but only
   mitigated *churn* (one connection per identity per worker, not one per batch), not the
   burst-at-start case itself.

2. **No retry on fetch.** `connect_with_retry` (`worker.rs:59-70`) and `upload_one`
   (`worker.rs:427-471`) both already wrap their I/O in the shared `retry_with_backoff`
   helper (`worker.rs:33-53`). `sink::fetch_uids` (`sink.rs:148-194`) has none: a single
   `session.uid_fetch`/`try_next` failure (the observed `io: connection closed via error`)
   is a hard, unretried `Err` that permanently drops that batch's UIDs for the run
   (`run_worker`'s `batch_error` path, `worker.rs:184-198`). CLI-0014's own "Out of scope"
   explicitly deferred "retry/backoff on provider rate-limit rejections" to "a separate
   future ADR" — never written until now.

3. **Warning spam hides the real error.** Once a mailbox's fetch degrades short of a hard
   `Err` (a dropped connection partway through, or `sink.rs:182`'s
   `let (Some(uid), Some(body)) = ... else { continue; }` silently skipping any FETCH
   response missing a body), `process_batch_on_session`'s transform loop
   (`worker.rs:242-283`) still iterates the *entire planned* `batch.uids` list and calls
   `EmailTransform::transform` on every one, unconditionally. `transform()`
   (`transform.rs:98-104`) `eprintln!`s "Warning: failed to read ...: No such file or
   directory" once per missing file — hundreds of near-identical lines for one degraded
   mailbox, drowning out the one line (`Error: login failed...`) that actually explains
   what happened.

Per this repo's dev-cycle convention (CLI-0029), this warrants a decision record rather
than a silent fix, since (1) is a real architectural gap CLI-0021 already flagged but didn't
close, and (2)/(3) are consistency gaps against precedent this codebase already
establishes elsewhere (retry-with-backoff for connect/upload; `FailureBreakdown` for
structured failure accounting, CLI-0033).

## Decision

Three fixes land together, since all three contributed to the same incident and are small
enough not to need separate ADRs.

### 1. Per-identity connection cap, tunable via a new flag

A `tokio::sync::Semaphore` per identity, sized
`concurrency.min(max_connections_per_identity).max(1)`. `max_connections_per_identity` is a
new `--max-connections-per-identity <N>` flag on `job run email-sync`, defaulting to `6`
when omitted — comfortably under Gmail's documented 15-simultaneous-connection cap, leaving
headroom for other IMAP clients (Apple Mail, Thunderbird, ...) already connected to the same
account. Unlike `--concurrency` (`ConcurrencyInput`, `wizard.rs:319-344`), this is a plain
optional flag with a hardcoded default applied in code, not routed through the wizard's
interactive-prompt machinery — it's a rarely-tuned safety cap, not a per-run decision.

`EmailSyncJob` (`mod.rs:203-207`) gains a `max_connections_per_identity: usize` field,
populated from the flag in `wizard.rs::dispatch`/`dispatch_async` and passed straight
through `EmailSyncJob::run` (`mod.rs:233-248`) into `worker::run_email_sync_job` as a new
argument — no change to the generic `Job` trait's `run` signature (`core/job.rs:27`), the
same pattern already used for `remote`/`encryptor`.

`run_email_sync_job` builds one `Arc<Semaphore>` per identity from that argument and hands
`Arc<Vec<Arc<Semaphore>>>` to every spawned `run_worker`. `WorkerConnection`
(`worker.rs:111-115`) gains an `OwnedSemaphorePermit` field; `run_worker` acquires
`identity_semaphores[identity_index].clone().acquire_owned().await` *before* calling
`connect_with_retry` (the connect at `worker.rs:149`), storing the permit alongside the
session. The permit releases automatically whenever that `WorkerConnection` is dropped or
replaced (the existing logout paths at `worker.rs:146-147,177,197,202-203`) — no manual
bookkeeping. A worker whose next batch belongs to an identity already at its cap simply
blocks on `.acquire_owned()` instead of opening a connection the provider is about to
reject; once a provider's real cap is respected, no code path can exceed it.

Companion fix: `all_batches`'s construction (`worker.rs:560-564`) changes from
identity-by-identity concatenation to round-robin interleaving across identities, so workers
aren't all stalled waiting on identity 0's semaphore while other identities' independent
work sits unprocessed further back in the same queue.

### 2. Retry a batch once before permanently failing it

`fetch_uids` can't be retried in place — a `connection closed` failure means the session
itself is dead, not just the one command — so the retry happens at the queue level instead.
The shared queue's item type changes from `(usize, Batch)` to
`(usize, Batch, attempts_remaining: u8)`, seeded at a new `const BATCH_RETRIES: u8 = 2`. On
any of `run_worker`'s three failure paths (connect failure `worker.rs:157-162`, `EXAMINE`
failure `worker.rs:170-179`, `process_batch_on_session` error `worker.rs:193-198`): if
`attempts_remaining > 0`, sleep briefly and push `(identity_index, batch,
attempts_remaining - 1)` back onto the queue instead of immediately counting the batch as
failed; `outcome.failed`/`FailureBreakdown` are only incremented once retries are exhausted.

This finishes the thought CLI-0014 deferred, and reduces user-visible failures from exactly
this kind of transient-connection incident — a batch that fails once (e.g. while other
workers are still releasing connections under fix 1) gets one more chance before being
deferred to the job's next invocation, which was already the accepted fallback per CLI-0021
§6's addendum ("a transient throttle now degrades to 'N messages failed, re-run to pick
them up'").

### 3. Collapse the per-UID "missing file" warning into one line per batch

`FailureBreakdown` (`worker.rs:79-85`) gains a `missing_file: usize` field. In
`process_batch_on_session`'s transform loop (`worker.rs:242-283`), the code now checks
whether `eml_path` exists *before* calling `transformer.transform(...)`. A missing file is
tallied as `outcome.failed += 1; outcome.failure_breakdown.missing_file += 1;` and skipped
— no doomed read, no per-file `eprintln!` from `transform.rs:98-104`. After the loop, if
`missing_file > 0`, one collapsed line is printed via `multi_progress.println`:
`"Warning: N of M messages in '<mailbox>' were missing from staging (fetch likely failed);
they'll be retried next run"`.

No change to `transform.rs` or the generic `Transform` trait — this only intercepts the
specific "the fetch phase never wrote this file" case before it ever reaches `transform()`,
so `transform()`'s existing per-genuine-parse-failure `eprintln!`s (corrupt/unparseable
`.eml` content, a different and much rarer failure mode) are untouched.

## Consequences

- A `job run email-sync` invocation can no longer open more than
  `max_connections_per_identity` simultaneous connections to any one identity, regardless of
  `--concurrency` — a single-identity run's effective fetch/transform parallelism is now
  capped at that number even if `--concurrency` is set higher. This is an intentional
  trade-off: a run that used to (rarely) get lucky at high concurrency against a single
  large mailbox now reliably stays under common provider connection caps instead.
- A batch that fails transiently gets one extra chance (two attempts total) before its UIDs
  are deferred to the job's next invocation, at the cost of a short sleep on the failing
  worker before it's requeued.
- A degraded mailbox now produces one summarizing warning line per affected batch instead of
  one line per missing message — a real terminal-output-volume change for anyone currently
  grepping for the old per-UID line, but the aggregate `FailureBreakdown.missing_file` count
  in the final summary already gives the total.
- `EmailSyncJob`, `cli.rs`'s `JobType::EmailSync`, and the wizard's `dispatch`/`dispatch_async`
  signatures all gain a new parameter to thread the flag through.

## Out of scope

- `sink::fetch_uids`'s silent drop of any FETCH response missing `uid`/`body`
  (`sink.rs:182`) still doesn't record *which* UID was skipped — fix 3's collapsed warning
  already covers the user-visible symptom (a batch came up short) without needing this;
  attributing the exact UID would need a `fetch_uids` signature change of its own.
- A per-provider-tuned default for `--max-connections-per-identity` (mirroring
  `Provider::accepts_invalid_certs()`'s existing per-provider-method pattern in
  `provider.rs`) instead of one flat default of 6 regardless of provider — deferred until a
  single conservative default proves insufficient or too conservative for some provider; the
  new flag already makes it tunable per run without a code change in the meantime.

Implementation lands on the same branch as this ADR.
