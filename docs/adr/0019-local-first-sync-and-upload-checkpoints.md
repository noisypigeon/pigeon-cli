# ADR-0019: local-first sync pipeline with upload checkpoints

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-23.
- **Status**: Accepted.

## Context

Three related improvements to `pigeon email sync`'s concurrency/resumability, investigated directly against the current implementation.

**Download, transform, and dedupe locally, then upload — not interleaved.** Today (ADR-0007 + ADR-0011 + ADR-0014), each concurrent mailbox worker does fetch → transform → verify → upload → delete → mark-processed per UID, inline. Because upload happens before every mailbox's local dedup has necessarily settled, ADR-0012 had to add a whole "reupload after merge" mechanism (`should_reupload_after_merge`) for the case where a later-processed duplicate in another mailbox amends an already-uploaded canonical file's frontmatter. Deferring upload until *all* local fetch+transform+dedupe work is complete for the identity removes that whole class of problem: by the time upload runs, every canonical file is already in its final, fully-merged state.

**Savepoints so interrupted flows don't restart from the beginning.** Today's fetch and transform phases are *already* resumable at the per-UID/per-mailbox level — `.processed`, `on_disk_uids`, and the ADR-0012 dedup-index files (`.message-hashes`/`.attachment-hashes`) are all append-only and reloaded correctly on every re-run; nothing there needs fixing. But splitting upload into its own later phase introduces a genuinely *new* resumability need that doesn't exist today: once `.processed` no longer implies "uploaded" (see below), an interrupted upload phase needs its own checkpoint so a resumed run doesn't have to re-touch every already-uploaded file.

**Whether concurrency causes files to be touched/transformed more than once — investigated, confirmed not currently a bug.** Re-reading `sync_mailbox`'s current code precisely: `message_index.lock()` and `attachment_index.lock()` (both `Arc<Mutex<ContentIndex>>`, shared across every concurrently-spawned mailbox worker) are acquired *before* `transform::transform_one(...)` and held for its *entire* synchronous call. Since `transform_one` is where `unique_path()`'s check-then-write race against the shared, flat, identity-scoped `output_dir` tree (per ADR-0006) would otherwise be possible, this existing lock scope already fully serializes `transform_one` — including its file writes — across every worker, today. **No bug currently exists.** But this protection is accidental: the lock's stated purpose (in its own code comment) is ADR-0012 dedup-index consistency, not write-race prevention, and nothing documents that narrowing this lock's scope later (e.g. for a perceived performance win) would silently reintroduce a real concurrent-write race. This gets fixed by making the invariant explicit, not by adding new code.

This ADR explicitly amends ADR-0011 (inline per-message upload) and ADR-0012 (the reupload-after-merge mechanism it introduced) — both stay unedited as historical records of what was decided when written; this ADR supersedes them going forward, per this project's established convention.

## Decision

### Sync splits into three sequential phases for the whole identity

Fetch → transform+dedupe (both concurrent across mailboxes, per ADR-0014, exactly as today minus the inline upload step) → upload (a new phase, running only after *every* mailbox's fetch+transform+dedupe work is complete). The default `sync` (no `--debug`) runs all three in order automatically.

### `.processed`'s meaning changes

It now means "fetched, transformed, verified, locally deduped" — not "fully done including upload." Called out explicitly as a semantic change to an existing marker.

### New per-identity upload checkpoint: `.uploaded`

Lives at `staging_dir`'s root — mirroring ADR-0012's `.message-hashes`/`.attachment-hashes` precedent exactly (append-only, one relative path per line, loaded once at the start of the upload phase, committed as each file's upload is confirmed). Not required for *correctness* (`upload_if_changed`'s ETag comparison is already idempotent), but avoids redundant HEAD-request round-trips for already-uploaded files on a resumed/re-run upload phase — a resumability-speed improvement, stated as such rather than overclaimed as a correctness fix.

### ADR-0012's "reupload after merge" mechanism is removed

`should_reupload_after_merge` and its call site go away. No longer needed: since upload only ever runs after all local dedup/merging is finished, no canonical file can be amended after it's already been uploaded.

### Upload phase enumerates `output_dir`, not mailbox/UID state

Since after the transform+dedupe phase the identity's `output_dir` (per ADR-0006's flat structure) is the complete, settled source of truth, the upload phase walks it directly (reusing the existing recursive-directory-walk pattern already used in `dataops::commands`), computes each file's S3 key the same way `upload_key` already does, checks `.uploaded`, and calls `upload_if_changed` for anything not yet confirmed — decoupled from mailbox/UID entirely, which is appropriate since dedup already blurs a file's relationship to "one mailbox" (shared attachments, `also-in:` cross-references).

### The existing transform-serialization lock scope stays exactly as-is — documented, not changed

`sync_mailbox`'s `message_index`/`attachment_index` locks continue to wrap the whole `transform_one` call; the fix here is entirely to that code's doc comment, stating explicitly that this scope is relied on for two reasons — ADR-0012 dedup-index consistency *and* preventing a `unique_path` TOCTOU race against the shared `output_dir` tree — so a future change can't narrow it without realizing both are at stake.

### New `--debug upload` mode

A third `DebugPhase::Upload` variant: runs only the upload phase against an existing `--local-output`'s `result/` tree and `.uploaded` checkpoint. Needs no IMAP session/credentials at all. Unlike `sink`/`transform` (which *reject* `--remote-output`), `upload` *requires* it.

## Consequences

- Uploads only ever see fully-deduped, final content — removes a whole subsystem (reupload-after-merge) and its ADR-0012×ADR-0011 interaction complexity.
- `.processed` alone no longer answers "is this identity's remote backup current" — `.uploaded` is now the source of truth for that question.
- The default `sync` flow's wall-clock shape changes: no more overlapping "still fetching mailbox B while uploading mailbox A's messages." Local work fully finishes before any network upload begins — trading some potential wall-clock overlap for a simpler, stronger correctness guarantee and cleaner resumability boundaries.
- Three `DebugPhase` variants now exist (`Sink`, `Transform`, `Upload`), each independently resumable and re-runnable on its own.
- No code changes result from the concurrency investigation — it's a documentation/intent fix confirming an existing accidental protection is sound, not a behavior change.

## Out of scope

- Concurrent/parallel uploads within the upload phase — each file's upload is safely parallelizable (distinct S3 keys, idempotent `upload_if_changed`), but this ADR doesn't add a `--concurrency`-style flag for it; sequential stays the default.
- Any change to `bucket_exists`/content-hash dedup detection logic itself.
- Retroactively fixing already-uploaded archives from before this ADR whose canonical file was amended by the now-removed reupload mechanism in a prior run. ([#25](https://github.com/noisypigeon/pigeon/issues/25))
- Implementation itself — like every ADR before it, this is a decision record only.
