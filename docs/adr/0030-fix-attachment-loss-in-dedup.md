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

- Investigating the 29723-message failure count. ([#38](https://github.com/noisypigeon/pigeon-cli/issues/38))
- Structured/aggregated failure-reason reporting (today's per-message `eprintln!` warnings have no aggregate summary) -- a related but distinct UX gap. ([#37](https://github.com/noisypigeon/pigeon-cli/issues/37))
- Any change to the dedup/placement algorithm's actual logic beyond the path-reconstruction fix -- the two-pass structure, idempotency guard, and canonical-selection order are all correct as designed.

Implementation is a separate, later task.

## Amendment (2026-09-25): attachments still lost after the fix

### Context

The fix above (the `staged_attachment_path` helper) merged and was rebuilt. Two real `pigeon job run email-sync` runs against real mailboxes afterward still reported zero attachments:

- `Synced 30711 message(s), 11 failed, 26505 message(s) merged, 0 attachment(s) deduped, 4157 uploaded, ...`
- `Synced 49 message(s), 11 failed, 40 message(s) merged, 0 attachment(s) deduped, 9 uploaded, ...` (a resumed run against the same `--local-output`)

The exact same 11 UIDs (by mailbox+UID) failed "verification failed" in both runs, unchanged. Rather than speculate further, this amendment's investigation inspected the real on-disk staging state directly: the `.eml` files a failed verification deliberately keeps (at the exact paths the warning prints, for debugging), and the `.job-checkpoint`/output-tree state for an identity with zero pending messages in both runs (meaning its checkpoint predates both, so it exercises the fix's "re-run against already-canonical messages" path).

This surfaced two distinct, independently-confirmed bugs. The original fix above is correct as far as it goes, but does not fix the reported symptom -- a second, deeper bug does almost all of the damage.

### Finding 1: the persistently-failing 11 messages -- a real, pre-existing, unrelated bug

All 11 kept `.eml` files share the same shape (confirmed by grepping headers and boundary lines across all 11): `Content-Type: multipart/mixed; boundary=SKPSMTPMessage--Separator--Delimiter` -- the boundary signature of the SKPSMTPMessage iOS SMTP-sending library, here wrapping Revel Systems point-of-sale receipt emails. Each contains exactly one real `text/html` part, followed by a *second* opening boundary line (`--SKPSMTPMessage--Separator--Delimiter`) with no headers, no content, and no closing `--...--` terminator. The raw message is genuinely truncated/malformed at the source -- confirmed via `tail`, each file ends immediately after that second boundary line.

`mail_parser`'s `message.attachments()` picks up this trailing, header-less, content-less part as a phantom "attachment" (no filename, empty body). `EmailTransform::transform` writes it to disk as a real, 0-byte file, exactly as it would any other attachment. `verify_transformed`'s all-or-nothing check --

```rust
outcome
    .attachments
    .iter()
    .all(|attachment| fs::metadata(&attachment.staged_path).is_ok_and(|meta| meta.len() > 0))
```

-- correctly rejects the phantom 0-byte part, but that rejects the *entire* message, not just the phantom part. The message (real HTML body and all) never gets checkpointed, is counted as "failed," and -- since its `.eml` is kept, not deleted, on a failed verification -- is re-fetched and re-fails identically on every subsequent run. Deterministic, not flaky: same malformed source bytes every time.

### Finding 2 (the dominant cause of the reported symptom): attachment placement only runs for messages canonicalized *in the same call*

The identity with zero pending messages in both runs had, in its `.job-checkpoint`, several legitimately-verified messages with real attachments (photos, several hundred KB each -- no malformed-MIME issue at all). Their canonical `.md` files were correctly placed and merged in the output tree. But the output tree's `attachments/` directory didn't exist at all -- the real staged image files were still sitting, untouched, exactly where `transform.rs` originally wrote them.

The cause is in `run_dedup_pass` (`dedup.rs`). Its message-placement pass (loop 1) records each canonical message's final path only in `placed_md_paths[index]`, a `Vec` local to that one call. Its idempotency guard --

```rust
if !staging_dir.join(&entry.md_staged_relpath).exists() {
    continue;
}
```

-- correctly recognizes "this message was already canonicalized by a prior run" and skips re-placing it, but leaves `placed_md_paths[index]` as `None` -- nothing re-derives that this entry *is* canonical from an earlier run. The attachment-placement pass (loop 2) then does:

```rust
for (index, entry) in entries.iter().enumerate() {
    let Some(md_path) = &placed_md_paths[index] else {
        continue;
    };
    ...
```

So any entry whose message was placed in an *earlier* run is silently skipped here too, even though its attachments may still be sitting, unplaced, in staging. This directly contradicts this ADR's original "self-healing" claim: re-running `job run email-sync` does **not** recover orphaned attachments for a message that was already canonical before the run started -- which, in ordinary incremental usage (the entire point of this pipeline's UID-based resumability, spanning many runs over time as new mail arrives), is the common case, not the edge case. Once a message is canonicalized, its attachments become permanently stuck unless that exact same call also happens to place the message.

This also explains why the fix's own new integration test passed without exposing the gap: it transforms, checkpoints, and dedups all in one call, so `placed_md_paths` is always populated -- exactly the one case that isn't broken. The break only shows up across *separate* invocations (real-world incremental usage), which no existing test exercised.

### Decision

Finding 2's fix decouples "does this entry's message have a canonical final path" from "was that placement decided in this exact call." Loop 2 should resolve each entry's target `.md` path via `message_index.check(&entry.message_hash)` instead of trusting `placed_md_paths`. `message_index` is reliably populated for every hash by the time loop 2 runs, whether committed just now (loop 1, this call) or in a prior run (`ContentIndex::load` reads the full persisted index file at the top of every call). The existing per-attachment `staged_path.exists()` check already safely no-ops for a merged duplicate's attachments (deleted by `remove_staged_files` whenever the merge happened, this run or an earlier one), so switching the source of the target path doesn't risk double-processing a duplicate's attachments. `placed_md_paths` becomes unnecessary once loop 2 no longer depends on it.

Finding 1 is a separate, real, confirmed bug with its own fix direction: `verify_transformed` (or `transform()` itself) should not let a single phantom (nameless, zero-byte) attachment part reject an otherwise-valid message wholesale -- e.g. skip zero-byte, nameless attachment parts during `transform()` rather than staging them at all, since a truncated trailing MIME part with no headers and no content is not a real attachment to begin with.

### Consequences

- The original fix's "self-healing" claim was wrong for the common case: any message canonicalized before this fix existed (or before any given future fix lands) needs more than a re-run to recover its attachments, under today's code -- it needs the Finding 2 fix specifically.
- Both findings are confirmed with direct evidence from real on-disk state, not inference from summary counters alone -- the kept `.eml` files and the multi-run checkpoint/output-tree state were essential to distinguishing them from each other.
- Finding 1's malformed messages are likely to recur for any mail sent through the same buggy client; today's behavior (permanent per-run failure, `.eml` preserved for inspection) is at least safe, if noisy.

### Out of scope (this amendment)

- Implementing either fix -- both are a separate, later task, pending direction on whether to land them together or separately.
- Any other change to the dedup/placement algorithm beyond what Finding 2 requires.
- Structured/aggregated failure-reason reporting (still a distinct, existing gap, unchanged since the original ADR). ([#37](https://github.com/noisypigeon/pigeon-cli/issues/37))

Implementation is a separate, later task.
