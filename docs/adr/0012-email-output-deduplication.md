# ADR-0012: deduplicate byte-identical content in `pigeon email sync`'s output

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-22.
- **Status**: Accepted.

## Context

Real archives produced by `pigeon email sync` (ADR-0007) show heavy content duplication in `--output-dir`: the same attachment bytes get written under many different per-message filenames, and the same physical email can be fetched more than once. The latter is architecturally possible today because `sync::run_async` iterates every mailbox independently (sync.rs's outer `for name in &names` loop) with no cross-mailbox awareness of message identity — a mail provider that exposes one physical message through more than one mailbox (a message present in both a regular mailbox and an "everything" mailbox, or under two overlapping labels) gets fetched, transformed, and written out once per mailbox it appears in, even though the bytes are identical every time. This ADR decides how `sync` (and `--debug transform`, which shares the same `transform_one` code path) detects byte-identical duplicates — both whole messages and attachments — and keeps one physical copy, referencing it from the rest, instead of writing repeated copies.

Hashing a real archive's `attachments/` folder as a starting point for this decision confirmed real, non-trivial byte-identical duplication *between different, genuinely distinct messages* (the same file re-attached across several separate messages) — this is the dominant case in practice. Hashing the `.md` files in the same archive found **zero** byte-identical whole-file duplicates, because ADR-0007's `uid:` frontmatter field is scalar and implicitly mailbox-scoped: two different (mailbox, uid) pairs never render identical `.md` output today even when they represent the same underlying message, since each carries its own `uid:` and `mailbox/...` tag. This means the two mechanisms below address genuinely different, non-overlapping cases, and both are needed:

- **Attachment dedup** — the same bytes attached across otherwise-different messages. Directly demonstrated, and the higher-value case.
- **Whole-message dedup** — the exact same physical message exposed at more than one (mailbox, uid) pair. Nothing defends against this today; explicitly worth closing even though it's the rarer case.

Implementation is a separate, later task — like every ADR before it, this is a decision record only.

## Decision

### Detection: hash content already fully in memory during `transform_one`

`transform::transform_one` already reads the whole raw `.eml` into memory (`bytes`) before parsing it, and already holds each attachment's raw bytes (`part.contents()`) before writing it to disk. Both dedup checks hash content already fully in hand at exactly the point a write would otherwise happen — no new I/O, no re-reading a file just to hash it:

- **Message-level**: hash the whole raw `.eml`'s bytes once, right after they're read.
- **Attachment-level**: hash each attachment's raw bytes right before the `unique_path`+write step that would otherwise create a new file for it.

Both reuse the `md5` crate, already a direct dependency as of ADR-0011 for the same kind of "is this content the same" comparison (there, against an S3 object's ETag). Reusing it here instead of adding `sha2` avoids a new dependency; accidental hash-collision risk at the scale of a personal mail archive is not a practical concern, the same judgment ADR-0011 already made for the same primitive.

### Durable index: two new per-identity files in `staging_dir`

Two new append-only files sit directly under `--staging-dir`'s root (not per-mailbox, since dedup must span every mailbox an identity has, not just one) — `.attachment-hashes` and `.message-hashes` — one file per concern, matching `.processed`/`.uidvalidity`'s existing precedent of one file per kind of bookkeeping rather than one combined file. Each line is `<hex-md5> <relative-output-path>`. Both are loaded once into memory at the start of a `sync`/`--debug transform` run, consulted on every hash check during that run, and appended to as new canonical content is committed. `staging_dir`'s root today holds only mailbox subdirectories, so these new dotfiles can't collide with a sanitized mailbox name.

Commit timing follows each caller's own existing durability contract rather than inventing a new one:
- Default `sync` flow: a new index entry is committed at the exact point `.processed` already is — after `verify_transformed` passes. An unverified message's content never becomes the canonical entry something else could be deduped against.
- `--debug transform`: committed immediately after each successful write, matching that mode's existing simpler loop (no verify-gate, no resume, per ADR-0007).

### Attachment dedup: reference an existing path, write nothing new

Before writing an attachment, hash its bytes and check `.attachment-hashes` (plus this run's not-yet-committed entries, so two identical attachments discovered within the same message, or across two messages processed earlier in the same run, also dedupe against each other). On a hash hit: skip the write entirely and record the **existing canonical** relative path as this message's attachment reference instead of computing a new one. On a miss: write it exactly as today (the existing same-name-different-content collision suffixing is untouched — it still runs, just only for genuinely new content that happens to compute the same target filename as something else), then record the new hash.

This needs **zero frontmatter schema change**. A message's `attachments:` list already just holds relative paths; nothing about the schema assumes each message's attachments are exclusively its own, and multiple messages' frontmatter already can point at the same path without any reader-side ambiguity. Structural verification (does every listed attachment path exist with nonzero size) needs no change either — it's trivially satisfied by a shared canonical path exactly as it would be by a fresh one.

### Whole-message dedup: merge into the canonical file

When a raw `.eml`'s hash hits `.message-hashes`, the duplicate occurrence gets **no new file at all** — it's merged into the already-canonical `.md`:

- The canonical file's frontmatter is amended: its `mailbox/<x>` tag set gains this occurrence's mailbox if not already present, and a new optional `also-in:` list field gains (or updates) an entry recording this occurrence — e.g. `"mailbox/archive#45"` (mailbox tag + UID). Entries are keyed by mailbox: a later UIDVALIDITY reset that re-fetches the same duplicate under a new UID **updates** its existing `also-in` entry in place rather than accumulating a stale second one.
- `uid:` is left exactly as ADR-0007 defined it — scalar, referring only to the canonical (first-seen) occurrence. `also-in:` is purely additive; nothing about the existing schema breaks.
- The duplicate occurrence's own `.eml` is deleted and its own (mailbox, uid) is marked `.processed`, exactly like any other successfully handled message — this doesn't need special-casing in `sync`'s per-UID loop (see below).

`transform_one` expresses "merged, nothing new written" the same way it expresses a normal success: it returns a result whose message-path points at the **existing canonical file** (after amending it) and whose attachment list is empty. Structural verification and the delete/mark-processed step that follow it then run completely unmodified — the canonical file obviously exists, and there are zero of its own attachments left to check. The only new logic this requires is the frontmatter amendment itself (a small, targeted textual edit of the existing `---`-delimited block to extend `tags:`/`also-in:`, in keeping with the project's existing hand-rolled, dependency-light frontmatter rendering rather than introducing a YAML parsing crate) — no new function signatures are needed in the structural-verification or per-UID delete/resume logic at all.

Canonical-occurrence selection is simply whichever mailbox the server happens to enumerate first in a given run — no mailbox is preferred over another. This is stated here explicitly as an accepted non-decision rather than left as an implicit accident of iteration order.

### Interaction with ADR-0011's `--output-remote`

A merge can mutate a canonical `.md` that this same run already uploaded earlier (if its mailbox was processed before the one containing the duplicate). To keep ADR-0011's "per-message, inline, no bulk final pass" principle intact rather than working around it, a merge that changes the canonical file triggers one small follow-up upload of just that `.md` (never its attachments, which didn't change) when `--output-remote` is set — the same content-comparison upload primitive ADR-0011 already introduced, called once more, for one more file. A later `sync --output-remote` run would also catch the change on its own via the normal same-file-or-different-file comparison regardless, so this is a same-run freshness improvement, not a correctness requirement.

Attachment dedup composes with ADR-0011 with **no changes needed there at all**: a deduped attachment's path is the same relative path a prior message's upload already used, so the existing upload comparison naturally reports it unchanged and skips the redundant transfer — deduplication on disk and deduplication on the remote fall out of the same mechanism for free.

### Summary visibility

`sync`'s (and `--debug transform`'s) printed summary gains new counts for messages merged into an existing canonical file and attachments referenced from an existing canonical file, alongside the existing message/attachment counts — so a user can see how much duplication was found and collapsed on each run, directly answering the concern that motivated this ADR.

## Consequences

- The two dedup mechanisms are independent and address different real cases: attachment dedup is the dominant, everyday saving; whole-message dedup closes an architectural gap (cross-mailbox exposure of one physical message) that nothing previously defended against.
- `email::transform`'s frontmatter schema gains one new optional field, `also-in:` — purely additive, no existing field's meaning changes, and `uid:`'s scalar, ADR-0007-defined meaning is preserved exactly.
- `staging_dir` gains two new per-identity bookkeeping files, following the exact convention `.processed`/`.uidvalidity` already established.
- The merge case's design deliberately avoids new code paths in structural verification or the per-UID delete/resume logic by expressing "merged" as an ordinary success whose message-path happens to be a pre-existing file — the smallest change that satisfies "keep one copy and reference it to the others."
- A `sync --output-remote` run does slightly more work than before when a same-run merge happens (one extra small upload), but never more than one per merge, and never for unaffected attachments.
- Deduplication is additive and forward-looking: nothing about existing, already-written output changes as a result of adopting this ADR until it's re-processed.

## Out of scope

- Retroactively reconciling an output directory that already has duplicate content from before this ADR's implementation — a one-time "reconcile the existing archive" pass, in the same spirit as ADR-0007's own deferred UID-based reconciliation idea, is a plausible future ADR, not this one. ([#17](https://github.com/noisypigeon/pigeon/issues/17))
- Cross-identity deduplication — this stays scoped to one identity's own output tree, matching ADR-0006's flat-per-identity folder structure. ([#18](https://github.com/noisypigeon/pigeon/issues/18))
- Any change to the hash algorithm choice, or to `remote`'s standalone commands beyond the composition already provided by ADR-0011's existing upload-comparison primitive.
- Implementation itself — like every ADR before its own separate implementation request, this is a decision record only.
