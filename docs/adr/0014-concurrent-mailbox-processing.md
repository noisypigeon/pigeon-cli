# ADR-0014: concurrent mailbox processing in `pigeon email sync`

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-23.
- **Status**: Accepted.

## Context

`pigeon email sync` processes one mailbox at a time end to end, and this ADR decides how to make that concurrent for more parallelism. Several existing pieces of the codebase make this more than a UI change:

- **A single shared `ImapSession` today.** Every IMAP call — `examine`, `uid_search`, `uid_fetch`, `list`, `logout` — takes `&mut ImapSession` (`service/pigeon-cli/src/email/imap_client.rs`; `service/pigeon-cli/src/email/sink.rs:133-134`; `service/pigeon-cli/src/email/sync.rs:66`), so one session can only have one command in flight. Concurrent mailboxes need one `ImapSession`/connection per concurrent worker, not a session shared across them.
- **No multi-threaded runtime exists yet.** All four runtime-construction sites (`sync.rs:45`, `sink.rs:37`, `imap_client.rs:55`, `remote/commands.rs:14`) build `tokio::runtime::Builder::new_current_thread()`, and `Cargo.toml`'s `tokio` dependency enables only `["rt", "net", "macros", "time"]` — not `rt-multi-thread`. True parallel execution via `tokio::spawn` needs that feature.
- **No provider connection-limit data exists.** `service/pigeon-cli/src/email/provider.rs` records per-provider host/port/TLS quirks but nothing about concurrent-connection caps. Real providers do cap this (Gmail and Fastmail both limit simultaneous IMAP connections per account; Proton Bridge's local proxy is typically more restrictive still) — unbounded concurrency risks provider-side throttling or rejected logins.
- **ADR-0012's dedup state is shared, mutable, and not thread-safe.** `ContentIndex` (`service/pigeon-cli/src/email/dedup.rs:11-14`) is a plain `HashMap`-backed struct with an `&mut self` `commit()`. `sync::run_async` loads one `message_index`/`attachment_index` pair up front and mutates both across every mailbox sequentially (`sync.rs:86-88, 141-157`). Concurrent mailboxes need these behind a shared lock to stay correct — and ADR-0012's canonical-occurrence rule, *"whichever mailbox the server happens to enumerate first"* (`docs/adr/0012-email-output-deduplication.md`, "Whole-message dedup"), stops being meaningful once mailboxes race rather than run in listing order.
- **`SyncSummary`** (`sync.rs:20-31`) is a plain counters struct with no read-during-run dependency — safe to keep as per-worker-local state merged once at the end, unlike the dedup indexes, which must stay consistent *during* the run because `check()` needs to see prior commits.
- **`.processed`/`.uidvalidity`/staged `.eml` files need no new synchronization.** Each mailbox already owns its own subdirectory via `sink::sanitize_mailbox_path` — distinct mailboxes never share a directory, so concurrent *different*-mailbox workers can't collide on these files.
- **ADR-0013 explicitly forecloses this.** Its Decision section states *"No `indicatif::MultiProgress` is needed: mailboxes ... are processed strictly sequentially (ADR-0007)"*, and its Out of Scope section lists, verbatim, *"Adopting `indicatif::MultiProgress` for concurrent or interleaved mailbox processing — `sync` stays sequential per ADR-0007."* Per this project's governance convention of flagging divergence from a prior ADR explicitly rather than silently drifting (the same way ADR-0007 itself explicitly reversed ADR-0001), this ADR supersedes that line and ADR-0007's original sequential framing.

Implementation is a separate, later task — like every ADR before it, this is a decision record only.

## Decision

### Runtime: enable `rt-multi-thread`, scoped to the default `sync` flow only

`Cargo.toml`'s `tokio` dependency gains the `rt-multi-thread` feature, and `sync::run` switches its runtime from `new_current_thread()` to `new_multi_thread()`. `--debug sink` and `--debug transform` keep their existing single-session, `current_thread` behavior unchanged — concurrency is opt-in to the default flow only, matching ADR-0011's precedent of a default-flow-only feature that `--debug` modes don't participate in.

### One `ImapSession` per concurrent mailbox worker

`session.list(...)` mailbox enumeration stays a single call up front, on one initial connection, exactly as today. Each mailbox is then dispatched to a worker that independently calls `imap_client::connect_and_login` for its own session, and runs ADR-0007's existing per-mailbox pipeline (examine → search → fetch → transform → verify → upload → delete) unchanged, logging out its own session when it finishes.

### Bounded concurrency via a new `--concurrency <N>` flag

Mailboxes are dispatched with `futures::stream::iter(names).map(...).buffer_unordered(concurrency)` — reusing the `futures` crate already a dependency (via `TryStreamExt`, used elsewhere in `sink.rs`/`sync.rs`), so no new dependency is needed beyond the `rt-multi-thread` feature flag. `--concurrency` defaults to a conservative fixed value (4) rather than a per-provider table: `provider.rs` has no connection-limit data today, and inventing precise per-provider numbers without real usage data is out of scope (an explicit accepted non-decision, in the same spirit as ADR-0012's canonical-mailbox non-decision). A user who knows their provider tolerates more can raise it.

### Dedup indexes become `Arc<Mutex<ContentIndex>>`

`message_index` and `attachment_index` move behind a shared lock across workers. `check()` and `commit()` stay logically atomic under it, preserving ADR-0012's "dedupes against anything committed earlier this run" guarantee. This amends ADR-0012's canonical-occurrence wording: *"whichever mailbox the server happens to enumerate first"* becomes *"whichever mailbox's commit for that hash wins the shared lock first"* — still a no-preference, accepted non-determinism, just redefined for concurrent workers instead of sequential listing order.

### `SyncSummary` becomes per-worker-local, merged at the end

Each worker accumulates its own `SyncSummary`; the orchestrator sums every field once all workers finish. No lock is needed on this hot path, since — unlike the dedup indexes — nothing reads a summary mid-run.

### Progress reporting moves to `indicatif::MultiProgress`

This explicitly supersedes ADR-0013's sequential-bar rationale and its "Out of scope" line. `sink::new_progress_bar` (added in ADR-0013) gains a `&MultiProgress` parameter and calls `.add(bar)` on it, so each active worker's fetch-phase and sync-phase bars render as their own simultaneous lines — appearing when a worker starts a mailbox, finishing/detaching when it completes.

### No new synchronization for per-mailbox files

Called out explicitly so a future reader doesn't wonder why only the dedup indexes got a `Mutex`: `.processed`, `.uidvalidity`, and staged `.eml` files are already scoped one-per-mailbox-subdirectory, so they need nothing new under concurrency.

## Consequences

- N× concurrent IMAP connections and logins per run instead of 1 — real provider-throttling risk if `--concurrency` is set too high, mitigated by a conservative default that's user-adjustable.
- First multi-threaded tokio runtime and first in-memory `Mutex`-guarded shared state in this codebase — previously all cross-run coordination was file-based (`.processed`, `.uidvalidity`, the dedup dotfiles).
- Interleaved multi-line progress output instead of one bar at a time — this is the visible payoff of the parallelism this ADR adds.
- `--debug sink`/`--debug transform` behavior is unchanged: still sequential, still a single session.

## Out of scope

- A per-provider concurrency-cap table in `provider.rs` — accepted non-decision, deferred pending real usage data. ([#20](https://github.com/noisypigeon/pigeon-cli/issues/20))
- Concurrency for `--debug sink`/`--debug transform`.
- Retry/backoff on provider rate-limit rejections surfaced by higher concurrency — today's existing hard-error-per-mailbox behavior is unchanged; backoff is a separate future ADR.
- Any change to `--output-remote`'s per-message upload mechanics beyond what naturally results from more mailboxes' messages uploading in parallel — ADR-0011's one-call-per-file model is untouched.
- Implementation itself.
