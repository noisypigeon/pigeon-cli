# ADR-0111: `deduplicate` creates `--local-output` if it doesn't exist

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-09.
- **Status**: Proposed.

## Context

`pigeon job run deduplicate --local-output <path> ...` fails with a raw,
confusing OS-level error if `<path>` doesn't already exist:

```
failed to create <path>/transcript.txt: No such file or directory (os error 2)
```

`LocalOutputInput` (`src/commands/job/deduplicate/wizard.rs:107-134`) never
validates or creates the path itself, and `job.gather()` -- called earlier,
before the confirm prompt -- only loads a checkpoint file
(`manifest::load_checkpoint`) that tolerates a missing directory as "empty."
Nothing in the main dispatch flow creates the directory before
`report_upload::new_transcript` needs it, right after the confirm prompt.

`pigeon job run rclone copy`/`rclone delete` (ADR-0110) already handle this
correctly: `dispatch_copy_async`/`dispatch_delete_async`
(`src/commands/job/rclone/wizard.rs`) call
`std::fs::create_dir_all(&local_output)` explicitly, in exactly this spot --
right after the confirm prompt resolves, right before `new_transcript`. This
ADR applies that same pattern to `deduplicate`'s main dispatch flow.

## Decision

Insert an explicit directory-creation step in `dispatch_async`
(`src/commands/job/deduplicate/wizard.rs`), immediately after the confirm
prompt resolves `Ok(true)` and before `generate_run_id()`/`new_transcript`,
via a small extracted helper (kept directly unit-testable, since a full CLI
run can't reach this point without a real, listable bucket -- every fake
`--source-bucket` alias fails earlier, during bucket resolution, before
`--local-output` is ever touched):

```rust
fn ensure_local_output_dir(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path).map_err(|err| format!("failed to create {}: {err}", path.display()))
}
```

called as `if let Err(err) = ensure_local_output_dir(&job.local_output) {
return fail(err); }`. `LocalOutputInput` itself stays untouched -- this is a
dispatch-flow fix, not a change to how the flag is parsed or defaulted.
`dispatch_upload_only`
(ADR-0089) is deliberately not touched: it legitimately requires
`--local-output` to already hold a completed prior run (checked via
`upload_only_preflight_ok`'s `.staging/.processed` marker), so auto-creating
a missing directory there would mask "there's nothing to resume" with a
different, equally wrong, empty-directory state instead.

## Consequences

- A `--local-output` path that doesn't exist yet now succeeds (the directory
  is created) instead of failing with a confusing OS-level error about a
  transcript file the operator never asked about directly.
- `--upload-only` behavior is unchanged -- still correctly requires a prior
  completed run, still fails the same way on a missing/incomplete one.

## Out of scope

- `email_sync`/`email_pull` -- already create the directory today, as an
  incidental side effect of their manifest-gathering step's own
  `create_dir_all` call on a subdirectory beneath it. Not touched here.
- `pull_transform` -- has the identical gap this ADR fixes for
  `deduplicate`, but fixing it is a separate, not-yet-requested task.
- `decrypt_files`'s `--output-dir` (same failure mode, different flag name)
  and `--input-dir` (a different bug entirely -- a missing directory is
  silently treated as "no files found" rather than erroring). Not touched
  here.
- Hoisting the now-five-times-duplicated `LocalOutputInput` struct into
  `shared_wizard.rs` -- a separate, broader refactor this fix doesn't need.

## Verification

- `mise run ci` clean (fmt-check + lint + test).
- Unit tests directly against `ensure_local_output_dir`: a missing nested
  path is created; an already-existing directory is a no-op; a genuine OS
  failure (a path component that's a plain file, not a directory) reports a
  clear error naming the offending path.
