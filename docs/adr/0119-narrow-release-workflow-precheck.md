# ADR-0119: narrow the release workflow's pre-publish check to `fmt-check` + `lint`

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-10.
- **Status**: Proposed.

## Context

After ADR-0118 fixed the missing `clippy`/`rustfmt` components, the
release workflow's `mise run ci` step still failed, this time at `cargo
test` — 11 failures, all environmental, none a real code regression:

- `job_run_pull_transform_*`/`job_run_transform_*` tests expect `ffmpeg`
  on `PATH`; it isn't installed on the runner.
- `job_run_rclone_*`/`job_run_transform_*` tests expect `rclone` on
  `PATH`; same.
- `keyring_delete_confirmed_deletes_it` fails with "No default store has
  been set" — no OS keyring backend is available/initialized in the
  runner's execution context.
- `log_file_flag_writes_valid_jsonl_with_the_command_name` fails on an
  empty-contents assertion — not obviously related to the other three,
  and not investigated further here (see Decision).

`mise run ci`'s full test suite has only ever run on a developer's own
machine, which already has `ffmpeg`, `rclone`, and a working OS keyring
session installed/configured. This is the first time it has ever run on
a bare CI box, and the gap between "what a dev machine has" and "what a
fresh `ubuntu-latest` runner has" turns out to be substantial — not just
missing Cargo toolchain components (ADR-0118), but missing system
binaries and a fundamentally different (headless, session-less) OS
keyring environment.

Every commit reaching `main` already passed the full `mise run ci` suite
locally before merge — that's the standing gate ADR-0029 established and
never relaxed. Re-running the identical suite again in the release
workflow doesn't verify anything new about *this* commit; it only
re-verifies work already gated once. Properly provisioning a CI runner to
match a dev machine (installing `ffmpeg`/`rclone`, and solving the OS
keyring's headless-session problem specifically for a non-interactive CI
process) is real, non-trivial work in its own right — exactly the kind of
broader CI-environment scope ADR-0117 already declined to take on
("a PR-triggered lint/test CI workflow... not introduced here").

## Decision

Narrow the release workflow's pre-publish check from `mise run ci` (which
chains `fmt-check` + `lint` + `test`) to just `mise run fmt-check` and
`mise run lint` — both fast and dependency-free, needing nothing beyond
the Rust toolchain + components ADR-0118 already installs. Drop `mise run
test` from this workflow entirely.

`cargo publish`'s own packaging step still does a full, fresh build of
the crate (visible in its own `Compiling pigeon-cli ...` output) before
uploading — that remains the real code-level safety net that catches
"doesn't compile," which is the one failure mode that would actually be
new information about this specific commit landing on `main`. `fmt-check`
and `lint` catch the (should-never-happen, but cheap to check) case of a
formatting/lint regression slipping past the local gate.

The `build-binaries` job is unaffected — it already only ran `mise run
build-release` (`cargo build --release`), never the test suite.

## Consequences

- The release workflow's pre-publish check no longer needs `ffmpeg`,
  `rclone`, or a working OS keyring session on the runner — it never will
  need them, since the full test suite doesn't run here at all.
- A real regression caught only by `cargo test` (not `fmt-check`/`lint`,
  not `cargo publish`'s own build) would not be caught by this workflow.
  Accepted: that regression would also need to have slipped past the
  mandatory local `mise run ci` gate ADR-0029 already requires before any
  PR can even be opened — this workflow was always a second check on
  already-gated work, not the primary one.
- The four specific environmental failures found above (`ffmpeg`,
  `rclone`, OS keyring, and the still-unexplained log-file test) are not
  fixed — they simply no longer block releases. They remain real gaps in
  "can `mise run ci`'s full suite run on a fresh machine," tracked below.

## Out of scope

- Provisioning `ffmpeg`/`rclone` and a working headless OS keyring
  session for a hypothetical future CI environment that does need to run
  the full test suite (e.g. a PR-triggered CI workflow, itself still out
  of scope per ADR-0117). ([#47](https://github.com/noisypigeon/pigeon-cli/issues/47))
- Root-causing `log_file_flag_writes_valid_jsonl_with_the_command_name`'s
  CI-only failure — not reproduced locally, not investigated beyond
  confirming it's one of several environmental failures on a fresh
  runner, not a release-blocking regression in already-merged code.
  ([#50](https://github.com/noisypigeon/pigeon-cli/issues/50))

## Verification

- `mise run fmt-check` and `mise run lint` both clean on the branch.
- Re-run (or wait for the next merge to trigger) the release workflow on
  `main` and confirm the `release` job's pre-publish step now passes.
