# ADR-0030: attachment loss in email-sync dedup — root cause and fix

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-25.
- **Status**: Accepted.

## Context

A real test run reported:

> Synced 370165 message(s), 29723 failed, 77897 message(s) merged, 0 attachment(s) deduped, 291288 uploaded, 0 unchanged, 0 upload failed.

291288 uploaded is consistent with just the canonical (non-duplicate) message count (370165 - 77897 = 292268, within a small margin) -- there is no headroom in that number for any attachment files at all. Zero attachments reached the bucket, silently: no error, no warning, no failure counted anywhere. This ADR documents the investigation and the fix.

## Investigation

### How an attachment is staged (`transform.rs`)

`EmailTransform::transform` writes each attachment to `attachments_dir = staged_dir.join(uid.to_string()).join("attachments")`, where `staged_dir = staging_root.join("transformed").join(relative_dir)` -- i.e. the real file lives at:

```
<staging_dir>/transformed/<mailbox_relpath>/<uid>/attachments/<name>
```

The `StagedAttachment` it returns carries two different path fields for two different purposes:

- `staged_path`: the full, correct, absolute path above -- used immediately afterward by `verify_transformed` to confirm the file exists and is non-empty.
- `staged_relpath`: deliberately just `"attachments/<name>"` -- documented as "relative to the staged message's own directory", intended for reuse, unchanged, as the *final* frontmatter-relative reference once the attachment is placed at its canonical location under `identity_dir`.

Only `staged_relpath` survives into the checkpoint (`CheckpointEntry.attachments: Vec<(hash, staged_relpath)>`, via `worker.rs`'s `process_batch_on_session`) and round-trips faithfully through `.job-checkpoint` (`manifest.rs`'s `append_checkpoint`/`load_checkpoint` -- verified correct and covered by an existing test). `staged_path`, the only field that actually locates the file on disk during staging, is never persisted.

### Where it breaks (`dedup.rs`)

`run_dedup_pass`'s attachment-placement loop reconstructs the staged file's location as:

```rust
let staged_path = staging_dir.join(staged_relpath);
```

This treats `staged_relpath` as staging-root-relative -- correct for `md_staged_relpath` (which genuinely is, e.g. `transformed/inbox/123.md`), but **not** correct for an attachment's `staged_relpath`, which is missing the `transformed/<mailbox_relpath>/<uid>/` prefix entirely. `staging_dir.join("attachments/foo.pdf")` is never where the file actually is. `staged_path.exists()` is therefore always `false`, and the pass's own idempotency guard --

```rust
if !staged_path.exists() {
    continue; // "already handled by a prior run"
}
```

-- silently treats every single attachment as already-done. `attachment_index.check(hash)` is never reached, so `deduped_attachments` never increments (matching the reported `0`), and the attachment is never moved into `identity_dir/attachments/`, so it never becomes an upload task (matching zero attachments uploaded). No code path here produces an error, warning, or failure count -- this is a silent-loss bug, not a crashing or logged one.

Messages are entirely unaffected: `verify_transformed` checks the correct absolute `staged_path` field (not the relpath), and `md_staged_relpath` genuinely is staging-root-relative, so message placement and upload both work correctly.

`remove_staged_files` (deletes a *duplicate* message's redundant staged attachments once it's merged into a canonical message) does `staging_dir.join(relpath)` for the exact same reason and has the identical bug -- `fs::remove_file` fails silently (`let _ = ...`) since the path is wrong. Consequence: duplicate messages' staged attachments are never cleaned up, a secondary disk-space leak in `staging_dir` (not a correctness issue, since these copies are genuinely redundant by construction).

### Why no test caught this

`dedup.rs`'s own attachment-dedup test hand-constructs `CheckpointEntry`s and stages attachment fixture files directly at `staging_dir.join(relpath)` -- i.e., the test fixture itself encodes the *buggy* assumption rather than mirroring `EmailTransform`'s real nested staging layout. No existing test exercises the real `append_checkpoint`/`load_checkpoint` round-trip together with `EmailTransform`'s real on-disk staging layout and `run_dedup_pass` in one combined pipeline -- exactly the seam where this bug lives.

## Decision

Reconstruct an attachment's real staged path from fields the checkpoint entry already carries -- `entry.md_staged_relpath`'s parent directory (`transformed/<mailbox_relpath>`) plus `entry.uid` -- instead of naively joining `staged_relpath` onto `staging_dir`. One shared helper, used by both the placement loop and `remove_staged_files`:

```rust
fn staged_attachment_path(staging_dir: &Path, entry: &CheckpointEntry, relpath: &str) -> PathBuf {
    let md_parent = Path::new(&entry.md_staged_relpath)
        .parent()
        .unwrap_or_else(|| Path::new(""));
    let file_name = Path::new(relpath).file_name().unwrap_or_default();
    staging_dir
        .join(md_parent)
        .join(entry.uid.to_string())
        .join("attachments")
        .join(file_name)
}
```

No checkpoint file format change -- `md_staged_relpath` and `uid` are already persisted, so this is a pure logic fix in `dedup.rs`, backward-compatible with every existing `.job-checkpoint` file (including ones written during the affected run). It is also self-healing: the buggy code never touched the orphaned attachment files, so they're still sitting in their correct staged locations on disk. Re-running `job run email-sync` against the same `--local-output` after this fix lands will find them, place them at their canonical `identity_dir/attachments/` location, and upload them -- no re-fetch or manual recovery needed, since the dedup pass already re-processes an identity's full checkpoint history on every run.

An integration-level test is added alongside the fix: real `EmailTransform::transform` on a message with an attachment -> real `append_checkpoint`/`load_checkpoint` round-trip -> real `run_dedup_pass`, asserting the attachment actually lands under `identity_dir/attachments/` -- the exact seam the existing unit tests didn't cover.

## Consequences

- Every attachment in every `email-sync` run to date has been silently lost: never uploaded, and left orphaned (not cleaned up) in each run's staging directory.
- Anyone who has already run a sync needs to re-run `pigeon job run email-sync` after this fix lands to recover and upload their orphaned attachments -- no other manual step required.
- `0 attachment(s) deduped` alone is not, by itself, a reliable "healthy run" signal -- it reads as "no duplicates found" when it can also mean "attachment processing never ran at all." Worth a follow-up: a distinct counter (e.g. attachments placed vs. attachments deduped) so a future regression here is loud instead of blending into a stat that looks fine at a glance.
- The 29723 "failed" messages in the reported run are **not** evidenced to be related to this bug (failure is determined entirely during transform/verify, before checkpointing, on a code path this investigation found no fault in) -- worth its own separate investigation if it persists after this fix, but out of scope here.

## Out of scope

- Investigating the 29723-message failure count.
- Structured/aggregated failure-reason reporting (today's per-message `eprintln!` warnings have no aggregate summary) -- a related but distinct UX gap.
- Any change to the dedup/placement algorithm's actual logic beyond the path-reconstruction fix -- the two-pass structure, idempotency guard, and canonical-selection order are all correct as designed.

Implementation is a separate, later task.
