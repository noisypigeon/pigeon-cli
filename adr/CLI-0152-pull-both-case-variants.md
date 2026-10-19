# CLI-0152: pull both case variants of a colliding source key

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-14.
- **Status**: Accepted.

## Context

A real `job run transform --input-file-type xlsx` run lost a file, and then
lost it again on a rerun. From the rerun's own artifacts (run_id
`18de4d1042f804d0-pigeon-141ad`):

- Three keys exist in one family at the source. `Bookkeeping.xlsx` became
  `Bookkeeping-d09d24e1eb67da86.csv` and `bookkeeping-2.xlsx` became two
  CSVs; both landed. `bookkeeping.xlsx` did not.
- rclone named the casualty itself, in the pull log:
  `level=notice object=bookkeeping.xlsx msg=Duplicate object found in
  source - ignoring`.
- All three were in the intended pull set
  (`.staging/pull-files-from.txt` lines 14, 254, 255).

S3 keys are case-sensitive; the local pull tree may not be. On the
case-insensitive APFS volume this run used, one directory cannot hold
`Bookkeeping.xlsx` and `bookkeeping.xlsx` at once, so rclone resolved the
clash itself by keeping one -- **its own ordering decision, which this job
cannot predict** -- and CLI-0130's reconciliation correctly reported the
other as an undelivered pull failure, exiting 1.

CLI-0144 diagnosed this mechanism precisely and deferred the fix to
[#83](https://github.com/noisypigeon/pigeon-cli/issues/83), on the grounds
that it needed *"the local-path layout changes CLI-0124's GC and the initial
`source/` scan both depend on."* CLI-0151 then improved the *message* and
explicitly left the data gap alone. So the second loss was not a regression:
it was the absence of this ADR.

### Why the deferral no longer holds

`PendingFile` already separates the two roles the fix needs separated:

- `relative_path` was used **only** as identity -- `group_key`'s hash input,
  the tracing span field, the retry context, and the reconciliation key.
- `absolute_path` was used **only** as the byte location -- the converter
  input and CLI-0124's GC target.

Nothing in `process_one` derived one from the other. The coupling lived in
exactly two lines: `enqueue`'s `source_dir.join(&relative_path)` and
`gather_pending`'s `relative_path_string(source_dir, &absolute_path)`. With
that known, the change is small.

## Decision

### 1. `PendingFile.relative_path` becomes `source_key`

A rename, but not a cosmetic one. The field has never been a local path, and
once a colliding key is pulled somewhere its key does not describe, calling
it `relative_path` is actively misleading. The new name is what stops a
future reader re-coupling the two, and the doc comment says which role each
field plays.

### 2. Colliding keys leave the bulk pull and are fetched individually

`files_needing_pull` already computed `case_collisions`. A new pure
`split_pull_set` divides the pull set into the keys the bulk `rclone copy`
takes and the keys fetched one at a time, each with its own `CopyTo` into
`.staging/case-variants/{nnnn}/{basename}`.

**Every member of a colliding group, not just the ones rclone would have
dropped.** Which one it keeps is its own ordering decision, so excluding the
whole group is what makes the outcome deterministic -- and it leaves the
bulk pull with no collision to resolve at all, so its `duplicate_notices`
(CLI-0151 §6) should be **0**. That is a crisp signal the split is working,
not merely a tidier log.

**A directory per key, not a suffixed filename.** A suffixed name would have
to be *invertible* -- or need an on-disk key-to-path side map -- for anything
to recover the source key for hashing. A directory sidesteps inversion
entirely, and the basename is kept verbatim so the local file still looks
like what it is.

`CopyTo` already serves the download direction (`emit_upload_metrics:
false`, exactly as `deduplicate`'s pull leg sets it), so this is reuse. The
pass is sequential on purpose: a colliding group is inherently tiny -- two
keys in the run that prompted this -- so there is nothing to parallelise and
no reason to add a second concurrency knob.

### 3. `gather_pending` is deliberately untouched

It scans `source/`, which never holds a variant, so CLI-0127's crash resume
does not cover them: after a crash they are simply re-pulled, one small
`copyto` each. That is correct, cheap, and is precisely *why* no side map is
needed. Named here as a deliberate limitation rather than left to be
discovered.

### 4. Destination naming already handled this

`compute_destination_name` hashes the source key **case-sensitively**, so
`Bookkeeping.xlsx` and `bookkeeping.xlsx` yield different digests and
distinct destination objects. CLI-0144 verified that property but never got
to use it; the real run confirms it, with
`Bookkeeping-d09d24e1eb67da86.csv` already at the destination. The same
reasoning covers the local `result/` tree and `push-logs/` names. No change.

### 5. CLI-0144's warning said something that is now false

*"rclone will deliver at most one of them"* stops being true the moment §2
lands. The per-group line and its named keys survive, but it becomes an
**INFO** describing what was done -- each key pulled individually to its own
path -- rather than a warning about data about to be lost.

`undelivered_detail`'s collision arm is replaced. A colliding key can now
only go undelivered if its own `copyto` failed, and `case_variant_failures`
carries rclone's actual error for it, which is strictly more useful than any
prose this function could invent. A second arm covers a colliding key that
vanished *without* its own pull reporting an error -- a bug rather than the
ordinary case the old arm described, and it says so.

`.staging/case-variants/` is cleared at startup alongside `scratch/`, reusing
CLI-0127's existing `remove_dir_all`; CLI-0124's GC already deletes each
variant file through `absolute_path`.

## Consequences

- **No source key is silently dropped**, on any filesystem. The reported
  case goes from 266 pulled / 1 failed / exit 1 to 267 pulled / 0 failed /
  exit 0, and `bookkeeping.xlsx` gets its own CSVs at the destination.
- CLI-0151 §6's "fully accounted for by case-colliding source keys" path
  should now go quiet. Kept as a fallback, since a variant pull can still
  fail for ordinary reasons.
- A colliding key is pulled outside the bulk transfer, so it does not benefit
  from `--download-transfers` parallelism. Irrelevant at the scale this
  occurs (two keys), and stated so nobody is surprised by a serial leg.
- A crash loses the variant files and re-pulls them next run (§3). Bounded
  and cheap.
- **This never fired on a case-sensitive host.** ext4 delivers both keys
  through the bulk pull, so a Linux deployment was never affected; the
  split now runs there too, harmlessly, and makes behaviour identical
  across platforms rather than filesystem-dependent.

## Out of scope

- **Changing how the destination is named for a colliding pair.** §4 already
  distinguishes them; nothing is needed.
- **Detecting a collision between a *source* key and an unrelated
  *destination* object.** Different problem, not observed.
- **Parallelising the variant pull** -- see Consequences.
- **Resuming a variant across a crash** (§3), which would need the on-disk
  side map this design exists to avoid.
- `sanitize_filename`/`strict_stem` case-folding. Both deliberately preserve
  case, which is what keeps the two variants' names distinct.

## Amends

CLI-0124 (its GC now covers a second local tree), CLI-0127 (its crash resume
deliberately does not extend to variants), CLI-0130 (a case collision no
longer produces an undelivered file, so its cross-check should go quiet),
CLI-0144 (reverses its deferral and replaces the message and the
`undelivered_detail` arm it introduced), and CLI-0151 (§6's explanation
path becomes a fallback). Each gets a short forward-pointing "Amendment".

Closes [#83](https://github.com/noisypigeon/pigeon-cli/issues/83).

## Verification

- `mise run ci` clean (fmt-check + lint + test).
- `worker.rs`: `case_variant_path` is deterministic, puts two
  case-differing keys in two **different directories**, keeps only the
  basename for a nested key, and lands outside `source/` -- the property
  §3 depends on.
- `worker.rs`: `split_pull_set` removes every member of a colliding group
  and nothing else, loses and duplicates nothing across the split, leaves a
  collision-free set entirely to the bulk pull, and is order-independent so
  `case_variant_path`'s index is stable.
- `worker.rs`: `undelivered_detail` surfaces rclone's real error for a
  failed variant pull, and flags a collision that vanished without one as
  the bug it would be.
- **Recorded as not end-to-end testable, rather than faked.** An
  integration test of the collision is unreproducible on *either*
  filesystem: on a case-insensitive host the fixture cannot even be created
  (writing `ledger.xlsx` overwrites `Ledger.xlsx` in the local source
  directory -- observed while attempting exactly this test), and on a
  case-sensitive host the bulk pull delivers both anyway, so the new path
  never runs and the test would pass vacuously -- the CLI-0139 mistake. The
  collision exists only because the real source is S3. Hence the pure tests
  above, plus the manual check below.
- Manual, the reported case: rerun the xlsx job and confirm **267 pulled,
  267 transcoded, 0 failed, exit 0**, `rclone_duplicate_notices: 0`, a
  `"case-variant pull complete"` line with `failed: 0`, and
  `bookkeeping.xlsx`'s own CSVs at the destination beside
  `Bookkeeping-d09d24e1eb67da86.csv`. Then a second rerun as a clean no-op.

## Amendment (2026-10-15): slot-batched variant pulls (CLI-0156)

The "inherently tiny" premise behind §2's sequential per-key `copyto` pass did
not hold at scale: a real `md` run had 428 colliding groups (870 keys) and
spent 19m45s in that pass with the whole pipeline stalled behind it. CLI-0156
replaces per-key directories with per-*slot* directories
(`.staging/case-variants/slot{k}/<key>`), each holding at most one spelling of
any key or directory prefix, and pulls each slot with one bulk `rclone copy`,
all slots concurrently. The per-group INFO notice becomes one aggregate line;
the full group list moves to `transform-report.txt`.
