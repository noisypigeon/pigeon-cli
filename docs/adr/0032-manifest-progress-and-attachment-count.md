# ADR-0032: manifest-phase progress reporting and an ATTACHMENTS column

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-25.
- **Status**: Accepted.

## Context

`pigeon job run email-sync`'s wizard flow (ADR-0021 §5) starts by pulling a
per-identity manifest -- connecting to each identity's IMAP server, listing
its mailboxes, and figuring out what's pending -- before showing the user
anything at all. Confirmed directly in code: `EmailSyncJob::gather()`
(`src/commands/job/email_sync/mod.rs:214-226`) calls `gather_pending`
(`mod.rs:65-170`) once per selected identity, which connects
(`worker::connect_with_retry`, itself capable of up to 3 silent retries with
5/10/15s linear backoff -- `worker.rs:24-25,59-70`), runs `LIST *`, then for
every non-`\NoSelect` mailbox runs `EXAMINE` + `UID SEARCH ALL`, and -- for
any pending UID whose size isn't already cached in a persisted `.manifest`
-- `UID FETCH ... RFC822.SIZE`. None of this prints anything: grepping the
whole phase (`mod.rs`, `manifest.rs`, the `connect_with_retry` path in
`worker.rs`) for `println!`/`eprintln!`/any `indicatif` type turns up
nothing. Every other phase of this pipeline already has a progress bar --
fetch/transform and upload (ADR-0013/0014/0015/0024) via
`sink::new_progress_bar` on a shared `indicatif::MultiProgress` -- so the
manifest phase's silence reads as the CLI having hung, especially for an
account with many mailboxes or a provider that's throttling connections.

Separately, the wizard's post-manifest summary table
(`print_manifest_summary`, `wizard.rs:237-250`) --

```
IDENTITY          MAILBOXES  PENDING  SIZE
finch-container   7          568      209.0 MB
willow-graysen    14         31911    2.2 GB
```

-- was asked to gain an `ATTACHMENTS` column. `SIZE` already reflects
`RFC822.SIZE` (whole-message bytes, attachments included), so this is a new
*count*, not a second byte total.

## Decision

### 1. A per-identity progress bar during manifest gathering

`EmailSyncJob::gather()` creates its own `indicatif::MultiProgress` --
separate from the instance `run_email_sync_job` creates later for
fetch/transform/upload (ADR-0024 §4). Manifest gathering always fully
completes, bars and all, before `run()` starts, so there's nothing to share
across the two, and no need to change the `Job` trait's signature (`gather`
stays `&self` -- `DecryptFilesJob`, `Job`'s only other implementor per
ADR-0028, is unaffected). The instance is passed by reference into
`gather_pending(ctx, &multi_progress)`.

Inside `gather_pending`, per identity:

- Before connecting: `multi_progress.println(format!("Connecting to {}...", ctx.identity.alias))` -- so a slow or retrying connect isn't silent either, matching ADR-0015's established "anything printed while bars might be live goes through `MultiProgress::println`, never a raw `println!`/`eprintln!`" rule.
- Once `LIST` returns and the mailbox count is known (after the existing `\NoSelect` filter): `sink::new_progress_bar(format!("{} manifest", ctx.identity.alias), mailboxes.len() as u64, &multi_progress)` -- the one existing bar-style helper (`sink.rs:130-141`, template `"{prefix} {bar:40} {pos}/{len}"`), reused rather than inventing a second visual style.
- `bar.inc(1)` once per mailbox, after that mailbox's `EXAMINE`/`UID SEARCH`/(`FETCH`) all complete -- matching the existing "increment on completion regardless of outcome" convention (ADR-0024 §4).
- `bar.finish()` once the mailbox loop ends, before moving to the next identity.

Identities are gathered strictly sequentially today (`EmailSyncJob::gather`'s
own `for ctx in &self.contexts` loop, no concurrency) -- each identity's bar
completes and stays in scrollback before the next one starts, the same
sequential-is-fine reasoning ADR-0013 originally used before ADR-0014
introduced concurrent mailbox workers.

### 2. `ATTACHMENTS`, sourced from IMAP `BODYSTRUCTURE`

Confirmed against this repo's pinned versions (`async-imap 0.11.3` /
`imap-proto 0.16.7`): `Fetch::bodystructure()` returns the MIME structure
tree (`imap_proto::types::BodyStructure`) without transferring any body
content -- a metadata fetch item exactly like `RFC822.SIZE`, not
`BODY.PEEK[]`, so it stays inside ADR-0021 §3's existing "message content is
never transferred" constraint for this phase. Each leaf part carries an
optional `ContentDisposition { ty, .. }`; a `Multipart` variant carries
`bodies: Vec<BodyStructure>`.

`pull_manifest` (`manifest.rs:32-69`) changes its fetch item list from
`"(UID RFC822.SIZE)"` to `"(UID RFC822.SIZE BODYSTRUCTURE)"` -- one combined
round trip, no new IMAP call. A small recursive helper counts attachment
leaf parts:

```rust
fn count_attachments(structure: &BodyStructure) -> u32 {
    match structure {
        BodyStructure::Multipart { bodies, .. } => {
            bodies.iter().map(count_attachments).sum()
        }
        BodyStructure::Basic { common, .. }
        | BodyStructure::Text { common, .. }
        | BodyStructure::Message { common, .. } => u32::from(
            common
                .disposition
                .as_ref()
                .is_some_and(|d| d.ty.eq_ignore_ascii_case("attachment")),
        ),
    }
}
```

This is explicitly an **estimate**, not authoritative -- it walks IMAP's own
declared MIME structure, a different code path from `transform_one`'s real
`mail_parser`-based `message.attachments()` (which ADR-0030 already showed
has its own edge cases, e.g. a phantom zero-byte trailing part on malformed
source mail). The two numbers are not guaranteed to agree, and this ADR
doesn't try to reconcile them -- same "illustrative, not calibrated" framing
this codebase already uses for the concurrency-time estimate (ADR-0021 §9).

### 3. `.manifest` gains a 4th column; a pre-upgrade file self-heals

`ManifestEntry` (`manifest.rs:21-25`) gains `pub attachments: u32`.
`save_manifest`/`load_manifest` (`manifest.rs:75-108`) add a 4th
tab-separated field. `load_manifest`'s existing line parser is a
`filter_map` chain of sequential `.next()?` calls -- a pre-upgrade
3-column line simply fails the new 4th `.next()?` and is dropped, exactly
matching this loader's already-documented "malformed lines are skipped
leniently" behavior. No migration step is needed: the next `gather_pending`
call re-pulls `RFC822.SIZE`+`BODYSTRUCTURE` once for those UIDs (the same
self-healing framing ADR-0030 used for its own on-disk fix), then caches
normally from then on.

Persisting the count (rather than always re-fetching `BODYSTRUCTURE` live on
every wizard invocation) matters because it preserves the existing
`all_sizes_known` cache-skip behavior (`mod.rs:134-148`), which exists
specifically to avoid a redundant IMAP round trip on a repeat or resumed
wizard run (ADR-0021 §3/§4) -- re-fetching unconditionally would quietly
reintroduce exactly the round trip that cache was built to avoid.
`gather_pending`'s `persisted_sizes` map (`mod.rs:91-94`,
`(mailbox, uid) -> size`) becomes `(mailbox, uid) -> (size, attachments)`;
the `all_sizes_known` gate naturally requires both fields cached together
for a UID, since they now come from the same map entry.

### 4. `IdentityManifestSummary` and the printed table

`IdentityManifestSummary` (`mod.rs:38-43`) gains
`pub pending_attachments: usize`, accumulated the same way
`pending_messages`/`pending_bytes` already are (`mod.rs:150-152`).
`print_manifest_summary` (`wizard.rs:237-250`) adds an `ATTACHMENTS` column,
positioned after `PENDING` and before `SIZE`:

```
IDENTITY          MAILBOXES  PENDING  ATTACHMENTS  SIZE
finch-container   7          568      42           209.0 MB
```

## Consequences

- The manifest phase is no longer silent -- a user sees a per-identity
  connect line and a mailbox-scoped progress bar the moment `job run
  email-sync` starts, consistent with every other phase of the pipeline.
- `.manifest`'s on-disk format changes (3 columns -> 4); every file written
  before this change is silently invalidated on next read and rebuilt --
  one extra `BODYSTRUCTURE` round trip per affected identity, once.
- The summary table gains a column whose number is a best-effort IMAP-side
  estimate, not the same figure `run`'s final `deduped_attachments` count
  reports -- worth remembering if the two are ever compared side by side.
- No change to `Job`'s trait signature, and no change to `DecryptFilesJob`.

## Out of scope

- Fixing `transform.rs`'s/`dedup.rs`'s `eprintln!` warning sites, which
  never adopted ADR-0015's `MultiProgress::suspend` wrapping after the
  ADR-0021 restructure moved them into `src/commands/job/email_sync/` --
  confirmed still raw `eprintln!` today, reachable while other workers'
  bars are live. A real, pre-existing gap, but unrelated to the silent
  manifest phase this ADR addresses. ([#40](https://github.com/noisypigeon/pigeon-cli/issues/40))
- Any progress indicator for the dedup phase (`run_dedup_pass`), which is
  also currently silent -- a separate, adjacent gap from the one raised
  here. ([#41](https://github.com/noisypigeon/pigeon-cli/issues/41))
- Reconciling BODYSTRUCTURE-derived attachment estimates against the real
  `deduped_attachments` count reported once a run finishes. ([#42](https://github.com/noisypigeon/pigeon-cli/issues/42))

The three items above are filed as GitHub issues via `mise run adr-issue`
while landing this ADR, per ADR-0031 §4. Implementation of the Decision
itself is a separate, later task.
