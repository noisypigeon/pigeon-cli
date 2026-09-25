# ADR-0034: the manifest ATTACHMENTS estimate is stuck at zero — root cause and fix

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-25.
- **Status**: Accepted.

## Context

A real `pigeon job run email-sync` wizard run reported:

```
jane-doe manifest ████████████████████████████████████████ 8/8
IDENTITY   MAILBOXES  PENDING  ATTACHMENTS  SIZE
jane-doe   4          72       0            20.4 MB
```

`ATTACHMENTS` is `0` despite 72 pending messages and 20.4 MB of real
data, for an identity known to have real attachments -- later pipeline
phases (`transform`/`dedup`) find and upload them without issue. This
happens on current `main`, which already includes ADR-0033's widened
`count_attachments` heuristic (also counting a `Content-Type` `name`
param, not just `Content-Disposition: attachment`) -- the exact fix
issue #42 asked for. The estimate is zero for every identity, not just
this one. This ADR investigates why, and proposes a fix.

## Investigation

### Ruled out: the IMAP crate's `BODYSTRUCTURE` parsing

`async-imap 0.11.3` / `imap-proto 0.16.7` (pinned in `Cargo.lock`) are
the crates behind `session.uid_fetch(&uid_set, "(UID RFC822.SIZE
BODYSTRUCTURE)")` (`manifest.rs`'s `pull_manifest`) and
`fetch.bodystructure()`. Reading `imap-proto`'s parser
(`src/parser/rfc3501/body_structure.rs`) confirms it fully parses
extension data -- disposition, `Content-Type` params, language,
location -- for both single-part and multipart bodies, at any nesting
depth; its own test suite (`body_structure.rs:458-490`) exercises
exactly this scenario (a PDF part with a `Content-Type` `NAME` param
*and* `Content-Disposition: attachment; FILENAME=...`) and passes at
the pinned version. A response this parser can't understand fails the
*entire* per-message `msg_att` parse, which `pull_manifest` surfaces
as a hard `Err` (a fetch failure the wizard would report), not a
silent `0`. This doesn't match the observed behavior, and there's no
open upstream issue describing disposition/params silently dropping at
this version.

### Ruled out (on current `main`): the heuristic itself

`count_attachments` (`manifest.rs:38-57`) already has ADR-0033's
widening -- it counts a leaf part if `common.disposition` is
`attachment` (case-insensitive) *or* `common.ty.params` has a `name`
key (case-insensitive). A single heuristic miss also wouldn't explain
"always zero, for every identity" as a deterministic, universal
pattern -- some fraction of real-world messages should trip at least
one of these two checks.

### Root cause: `gather_pending`'s persisted-manifest reuse never invalidates

`gather_pending` (`mod.rs:70-197`) loads the *previous* run's
persisted `.manifest` file and builds:

```rust
let mut persisted_sizes: HashMap<(String, u32), (u64, u32)> = HashMap::new();
for entry in &persisted_manifest {
    persisted_sizes.insert((entry.mailbox.clone(), entry.uid), (entry.size, entry.attachments));
}
```

Then, per mailbox, if every currently-pending UID is already a key in
that map (`all_known`, `mod.rs:151-153`), it reuses the persisted
`(size, attachments)` pair **verbatim** and skips `pull_manifest`
entirely (`mod.rs:154-168`) -- `count_attachments` is never called on
this path, regardless of which version of the heuristic is compiled
in. Nothing ever invalidates a cached `attachments` value once
written:

- A UID only leaves `persisted_sizes`' effective relevance once it's
  checkpointed (synced) -- until then, every run re-hits the reuse
  path with the same cached value.
- A `UIDVALIDITY`-triggered mailbox reset (`mod.rs:129-133`) clears
  that mailbox's `*.eml` files and `.job-checkpoint` entries, but
  **never touches `staging_dir/.manifest`** -- so even a full mailbox
  reset doesn't force a fresh `BODYSTRUCTURE` pull.
- The `.manifest` file format carries no schema/heuristic-version
  marker, so a value written by an older, narrower heuristic (or even
  the very first ADR-0032 version, before `count_attachments` existed
  in its current form) is syntactically indistinguishable from a
  freshly, correctly computed `0`. `load_manifest`'s existing
  self-healing (`load_manifest_drops_pre_adr_0032_three_column_lines`)
  only catches the pre-ADR-0032 *3-column* format -- a well-formed
  4-column line with a stale value passes through untouched.
- `save_manifest` (`mod.rs:194`) writes `fresh_manifest`, which on the
  reuse path is just the cached values copied straight back out -- the
  staleness is self-reinforcing: once `0` is written for a UID, every
  subsequent run reads `0`, reuses `0`, and re-persists `0`.

This mechanism is identical for every identity and independent of any
individual message's MIME shape, which matches "always zero, for every
identity" far better than a per-message heuristic gap would. It also
means upgrading `count_attachments` (as ADR-0033 already did) has no
effect on an identity whose `.manifest` already has cached entries for
all its pending UIDs from before the upgrade -- exactly the reported
symptom.

### Secondary, minor gap (not the primary cause)

`count_attachments`'s `name`-param check
(`key.eq_ignore_ascii_case("name")`) is an exact match and won't catch
an RFC 2231-encoded parameter name (e.g. `name*0*=`, `name*=`). Worth
noting, not worth conflating with the headline fix -- it's a narrow,
content-dependent gap, not a systemic one.

## Decision

Stop treating a persisted `attachments` count as valid input to
`gather_pending`'s own skip-the-refetch decision. Since
`pull_manifest`'s single `UID FETCH ... (UID RFC822.SIZE
BODYSTRUCTURE)` command already fetches `size` and `attachments`
together in one round trip, there is no way to cheaply keep reusing a
cached `size` while re-deriving `attachments` alone -- splitting the
cache by field buys nothing. The `persisted_sizes`/`all_known` reuse
fast-path is removed from `gather_pending` entirely; it always calls
`manifest::pull_manifest(&mut session, mailbox_name,
&pending_uids).await?` for every mailbox with pending UIDs.

`BODYSTRUCTURE`/`RFC822.SIZE` fetches never transfer body content
(ADR-0021 §3's original rationale for choosing this data-item family
over a full body fetch), so the cost of always re-pulling is one
lightweight metadata-only IMAP round trip per mailbox per
`gather_pending` call -- not a body fetch, and not a cost that scales
with attachment size. In exchange, every printed `ATTACHMENTS`
estimate is always freshly derived from whatever `count_attachments`
version is actually running, with no possibility of this staleness bug
recurring after a future heuristic change.

`.manifest`'s on-disk format, and `save_manifest`/`load_manifest`
themselves, are unchanged -- `.manifest` becomes a passive,
write-only-by-`gather_pending` snapshot (still useful for external
inspection/debugging) rather than an input `gather_pending` reads back
to make its own reuse decision. `persisted_manifest`/`persisted_sizes`
and the `all_known` branch (`mod.rs:97-104,151-169`) are deleted.

This incidentally also closes the separate "UIDVALIDITY reset doesn't
clear `.manifest`" gap noted above, since `.manifest` is no longer
read as a decision input at all -- there's nothing left to go stale.

## Consequences

- `ATTACHMENTS` becomes a live, correct-per-current-heuristic estimate
  on every run, for every identity, instead of a value that can get
  permanently stuck the moment it's first (possibly wrongly) cached.
- `gather_pending` loses its one network-round-trip optimization for
  identities with a large, mostly-unchanged pending set across
  repeated wizard invocations (e.g. someone previewing the summary
  several times before confirming) -- traded for correctness, and
  bounded in cost since no body content is ever transferred either
  way.
- `.manifest`'s persisted `size`/`attachments` columns are no longer
  read by `gather_pending` itself; they remain useful for a human or
  tool inspecting the file directly, but nothing in this codebase
  round-trips them anymore.
- Anyone who has already accumulated a `.manifest` with stale `0`
  attachment counts sees the correct estimate on their very next run
  after this fix lands -- no manual cleanup needed, since the fix
  changes what `gather_pending` reads, not the file itself.

## Out of scope

- The RFC 2231 encoded-parameter-name gap in `count_attachments` ([#49](https://github.com/noisypigeon/pigeon-cli/issues/49))
  (`name*0*=`/`name*=` not matching the current exact `"name"` check)
  -- a narrower, content-dependent heuristic gap, unrelated to why the
  estimate is *always* zero.
- Any other change to `count_attachments`'s heuristic, or to
  `pull_manifest`'s IMAP fetch item list.
- Adding a schema/heuristic-version marker to `.manifest` -- considered
  as an alternative fix and rejected in favor of removing the reuse
  path outright, since the two fields can't be cached independently
  anyway (see Decision).

Implementation is a separate, later task.
