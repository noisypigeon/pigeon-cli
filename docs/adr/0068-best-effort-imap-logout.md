# ADR-0068: treat IMAP LOGOUT failures as best-effort, not fatal

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-27.
- **Status**: Accepted.

## Context

`mise run pigeon job run email-sync` crashed again, this time *after* the
`karafinch` mailbox's manifest gathering fully succeeded (progress bar
reached `37/37`):

```
Error: logout failed: connection lost
[pigeon] ERROR task failed
```

**Root cause, confirmed by reading every `.logout()` call site**
(`grep -rn "logout failed\|\.logout(" src/`): there are
5 call sites across 3 files. Two of them
(`src/commands/job/email_sync/worker.rs:147,203`, the
concurrent fetch/transform/verify phase, ADR-0014/ADR-0024) already treat
logout as best-effort cleanup: `let _ = conn.session.logout().await;` — a
failure there is silently ignored, since the connection is being torn down
anyway and every real unit of work already completed or failed
independently.

The other three **propagate the error with `?`**, turning a pure "say
goodbye to the server" courtesy call into a hard failure of the whole
function that already did its real work:

- `src/commands/job/email_sync/mod.rs:168-170` — the
  exact crash site. This is the manifest-gathering phase's `gather_pending`:
  it loops over every mailbox, calls `manifest::pull_manifest` for each
  (ADR-0065's bisection now makes this loop *more* resilient to bad
  messages, which is what let it reach `37/37` instead of crashing
  earlier), then calls `session.logout().await?` — **after** the loop, but
  **before** `manifest::save_manifest(&ctx.staging_dir, &fresh_manifest)?`.
  A logout failure here doesn't just abort unnecessarily: it early-returns
  *before* `save_manifest` ever runs, silently discarding every mailbox's
  successfully-gathered manifest data for that identity, not just failing
  loudly. This compounds a merely-annoying crash into real data loss.
- `src/commands/job/email_sync/sink.rs:118-120` — the
  legacy `--debug sink` path's equivalent: every mailbox's `fetch_uids`
  already succeeded and `summary` is fully built, then logout can throw the
  whole `Ok(summary)` away via early `?`.
- `src/commands/keyring/email/imap_client.rs:92-94` —
  `verify_login` (used by `pigeon keyring add email` to check a credential
  before persisting it). Its whole point is "did `LOGIN` succeed" — a
  subsequent `LOGOUT` failure says nothing about whether the credential is
  valid, but today it would make a *good* credential look rejected.

All three are the same class of bug: conflating "the real operation
succeeded" with "we also managed to hang up the phone politely afterward."
`worker.rs`'s already-correct, already-proven pattern
(`let _ = ...logout().await;`) is the fix — just apply it consistently.

## Decision

- `mod.rs`'s `gather_pending`: replace the `?`-propagating logout call with
  `let _ = session.logout().await;`. This also fixes the ordering hazard:
  `manifest::save_manifest(...)` keeps running unconditionally right after,
  exactly as already coded — it naturally stops being skippable once
  logout can no longer early-return.
- `sink.rs`'s debug-sink path: same replacement, so a logout blip after a
  fully-successful per-mailbox fetch loop still returns `Ok(summary)`.
- `imap_client.rs`'s `verify_login`: same replacement — success is
  determined by `connect_and_login` succeeding; logout is cleanup only.

No behavior change on the happy path (logout still runs, in the same
place, every time); the only change is that its *result* no longer gates
anything downstream.

## Consequences

- A transient disconnect right at logout time no longer discards an
  identity's already-gathered manifest data or aborts the whole
  multi-identity job — matching `worker.rs`'s existing, already-relied-upon
  behavior.
- `verify_login` now correctly reports success based on `LOGIN`, not
  `LOGOUT` — a real (if narrow) correctness fix: a good credential could
  previously be reported as invalid due to a logout-time network blip.
- The IMAP server may occasionally see a connection drop without a clean
  `LOGOUT` (already true today for every path through `worker.rs`) —
  harmless; servers time out/reap idle or abruptly-closed connections
  regardless.

## Out of scope

- Any change to `worker.rs` — already correct, used as the precedent.
- Retrying `LOGOUT` itself, or reconnecting to retry it — there's nothing
  meaningful to retry; the operation it was cleaning up after already
  finished.
- Any other IMAP command's error handling — scoped strictly to `LOGOUT`.
