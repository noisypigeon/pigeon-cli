# ADR-0105: capture failure narration in the run transcript, not just stderr

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-07.
- **Status**: Proposed.

## Context

Reviewing the same 33 archived job-run bundles (ADR-0104's context), every
*failed* `job run import` run has a 0-byte `transcript.txt` -- confirmed
directly on `.../consolidate-setsye-segment-1-4957/transcript.txt`,
`.../consolidate-a5tl5v-segment-2-1235/transcript.txt`,
`.../consolidate-aut1w2-segment-1-check-1232/transcript.txt`, and
`.../consolidate-e2qpr0-segment-2-1241/transcript.txt` (the 4 failed runs in
the sample). An operator skimming the human-readable artifact ADR-0100
specifically introduced for "what happened in this run" sees nothing -- not a
failure message, not a hint, just an empty file. That's the worst version of
"unclear outcome": it doesn't even look like something went wrong.

Root cause, confirmed in the code:

- `commands::fail()` (`src/commands/mod.rs:19-23`) is the shared helper every
  job wizard's error path calls: it `tracing::error!`s (goes to
  `pigeon.jsonl`) and `eprintln!`s (visible on the live console only), then
  returns the exit code. It never touches a `Transcript`.
- `report_upload::say()` (`src/commands/job/report_upload.rs:95-99`) is the
  *only* function that writes to both stdout and the transcript, and it's
  only ever called on the success path -- e.g.
  `src/commands/job/deduplicate/wizard.rs:273-274` and
  `src/commands/job/import/wizard.rs:222` both call it only inside the
  `Ok(summary)` branch of their `job.run(...).await` match.
- On the `Err(err)` branch, `deduplicate/wizard.rs:281-286` writes a generic
  `Debug`-formatted report (if one doesn't already exist) and calls `fail(err)`
  -- no `report_upload::say` call, so no transcript line. `import/wizard.rs:230`
  does the same: `Err(err) => (fail(err), log_path_for_err)`.

The archived `transcript.txt` is still uploaded on failure (via
`upload_run_artifacts`, called unconditionally after the match in both
wizards) -- it's just empty, because nothing ever wrote to it.

## Decision

Route the failure message through the transcript wherever one already
exists at the call site, mirroring what `report_upload::say` does on the
success path:

1. In each job wizard's `Err(err)` branch (after the `Transcript` has been
   created via `report_upload::new_transcript`), call
   `report_upload::say(&transcript, format!("Error: {err}"))` before (or
   instead of a bare) `fail(err)`, so the same message that goes to stderr
   also lands in the archived transcript.
2. For the handful of earlier `return fail(err)` call sites in each wizard
   that happen *before* a `Transcript` exists yet (e.g. argument/keyring
   resolution failures ahead of `report_upload::new_transcript`), no change
   is possible or needed -- there's nothing to upload yet in that case, so
   this is specifically about failures that occur after local setup has
   produced a transcript to write into (which covers every failure that
   currently results in an uploaded-but-empty `transcript.txt`, i.e. exactly
   the 4 failed runs observed in this sample).
3. Apply the same pattern to every other job wizard using this structure
   (`pull-transform`, `email-sync`, `email-pull`, `decrypt-files`), not just
   `deduplicate` and `import`.

## Consequences

- An operator skimming `transcript.txt` for a quick status check sees a
  failure message on a failed run, instead of an empty file indistinguishable
  from "nothing happened" or "misconfigured upload."
- No behavior change to exit codes, retries, or checkpointing -- purely what
  gets recorded.
- `fail()` itself stays as-is (still used plenty of places with no
  transcript in scope); this only adds a transcript write at the specific
  call sites that already have one.

## Out of scope

- Changing `fail()`'s own signature to thread a transcript through
  everywhere -- not every call site has one in scope, and forcing it would
  push transcript-creation earlier than it needs to be for failures that
  happen before any local output directory exists.
- `pigeon.jsonl`'s own completeness for a failed run -- that's ADR-0104.
- `import/wizard.rs::dispatch_async`'s `job.gather().await` failure site is a
  `return fail(err)` that bypasses `upload_run_artifacts` entirely -- unlike
  every other wizard, `import` creates its `Transcript` *before* calling
  `gather()`, so a transcript is technically in scope there, but writing to
  it wouldn't help: the early `return` means nothing (not report, not
  transcript, not `pigeon.jsonl`) gets uploaded today. Fixing this properly
  means restructuring `dispatch_async` so a gather failure also flows
  through the function's tail-end upload call -- a separate, larger change
  than this ADR's one-line-per-site fix, deferred to its own follow-up. ([#32](https://github.com/noisypigeon/pigeon-cli/issues/32))

## Verification

- Unit test per job wizard: simulate a failure after transcript creation,
  assert `transcript.txt`'s contents are non-empty and contain the error
  message.
- Manual: force a failure in `job run import` (e.g. an invalid `--source`)
  and confirm the uploaded `transcript.txt` in the report bucket contains the
  error, not nothing.
- `mise run ci` clean.
