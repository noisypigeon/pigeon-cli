# ADR-0101: `pigeon job run import` (rclone-backed copy job)

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-04.
- **Status**: Accepted.

## Context

Every existing job type reaches storage through pigeon's own `minio`-backed S3 client (ADR-0009/0010) -- fine for the S3-compatible endpoints pigeon already understands, but there's no way to pull data from, or push data to, the dozens of other backends the external `rclone` tool already supports (arbitrary cloud providers, SFTP, etc.) without leaving pigeon's observability/CLI surface entirely and running `rclone` by hand.

This ADR adds `pigeon job run import`, a thin wrapper around a single `rclone copy <source> <destination>` invocation with a fixed set of performance/retry flags, so that copy shows up in `pigeon.jsonl`, pigeon's metrics, and the ADR-0100 report-bucket upload like every other job's work does -- without pigeon taking on any part of rclone's own job (listing, diffing, transferring, retrying).

## Decision

### 1. Source/destination are raw rclone strings, not pigeon bucket-configs

`--source`/`--destination` are passed straight through to `rclone copy`'s argv exactly as given (e.g. `source:media/`, `destination:`). Pigeon does not manage rclone credentials or config at all -- the backing `rclone.conf` is provisioned by an external, terraform-driven deployment process, entirely outside this crate's scope. This is deliberate, not a gap: routing `import` through pigeon's own `BucketConfig`/keyring (as every other job's source/destination does) would limit it to the same S3-compatible endpoints pigeon's other jobs already reach directly, defeating the reason to shell out to rclone in the first place.

Both are mandatory `WizardInput`s (`SourceInput`/`DestinationInput` in `src/commands/job/import/wizard.rs`), structurally identical to `shared_wizard::SourceBucketInput`'s flag/prompt/error shape but resolving a plain string rather than looking up a bucket-config alias. Kept local to `import`, not hoisted -- no second consumer exists yet.

### 2. The rclone performance/retry flags are fixed, not CLI flags

```
rclone copy <source> <destination> \
  --transfers 32 --checkers 64 --fast-list --buffer-size 32M \
  --multi-thread-streams 4 --multi-thread-cutoff 256M \
  --retries 5 --low-level-retries 20 --stats 30s \
  --use-json-log --log-level INFO --log-file <path>
```

Only `--source`/`--destination` vary per run. `ImportJob::run` ignores the `Job` trait's `concurrency`/`upload_concurrency` parameters entirely (same precedent as `DecryptFilesJob`, ADR-0090, for a job where those dials don't apply) -- rclone's own `--transfers`/`--checkers` own that concern.

### 3. Structured JSON logging replaces the plain-text log, and doubles as the report

Two changes from a typical hand-run rclone invocation:

- `-P`/`--stats-one-line` are dropped (human-terminal-oriented; irrelevant once output is captured to a file instead of a live terminal) and `--use-json-log` is added, so every line is a parseable JSON object instead of free text -- a direct fit for pigeon's own JSONL (`pigeon.jsonl`) observability design.
- `--log-file` no longer points at a fixed path (the user's original example used `~/rclone-media.log`); it's written to `<local_output>/rclone-<run_id>.jsonl`, reusing the same "per-run artifacts live under `--local-output`" convention every other job already has for its staging tree, with a run-id-suffixed name so repeated runs never ambiguously append to or clobber a shared file.

After the subprocess exits, `src/commands/job/import/rclone_log.rs::parse_and_report` reads this file and: re-emits every `"level":"error"` line as a `tracing::warn!(key = ..., step = "transfer", error = ..., ...)` (same `key`/`step`/`error` field vocabulary ADR-0073/0099 already established), reads the last `stats`-bearing line's cumulative `transfers`/`errors`/`bytes` as the run's final counts, and feeds those counts into `tracing::info!("import: rclone copy complete", ...)` (bracketing a matching `"import: rclone copy starting"` line) and `observability::metrics::record_phase_count("import", "transfer", ..., None)`. The `source_bucket`/`destination_bucket` metric-label parameters are passed `None` throughout -- there's no `BucketConfig` in scope for either side of this job, unlike every other job's metrics call sites.

The rclone JSON log file is not summarized into a second, separate report -- it *is* this job's report, passed directly as `report_upload::upload_run_artifacts`'s `report_path` argument (which is content-agnostic about what it's given). `import` therefore skips the generic `report_upload::write_summary_report()` step every other non-`deduplicate` job uses (ADR-0100 §4).

### 4. Conforms to ADR-0100 like every other job, from day one

`import` is the 7th job type and gains `--report-bucket` same as the other 6: `report_upload::resolve()`/`generate_run_id()`/`run_prefix()`/`new_transcript()`/`say()`/`upload_run_artifacts()` are all reused unmodified. `rclone` availability (`rclone version`) is checked once, up front in the wizard, before any prompts -- mirroring `pull_transform::wizard`'s existing `check_ffmpeg_available()` call for the same reason: fail immediately on a missing binary, not partway through a long-running subprocess.

## Consequences

- `pigeon` gains a 7th job type that can reach any backend `rclone` supports, at the cost of pigeon managing none of that backend's credentials -- `rclone.conf` is entirely an external dependency.
- `import`'s `--source`/`--destination` are opaque strings to pigeon; a typo or a misconfigured `rclone.conf` surfaces only via rclone's own error output (captured in the JSON log and, for a pre-logging-init failure, in the error message's folded-in stderr).
- Exactly one new runtime dependency exists outside this repo's control: `rclone` itself must be on `PATH`, matching the existing `ffmpeg`/`ffprobe` precedent `pull_transform` already set.
- rclone's own distinct non-zero exit codes (e.g. hitting `--max-transfer`, no files transferred) are not individually classified -- any nonzero exit is a pigeon job failure, same binary contract every other job already has.
- The rclone JSON log schema (`level`/`msg`/`object`/`stats.{bytes,transfers,errors}`) was confirmed against rclone's documentation during design but not against a live invocation; a schema mismatch would silently zero out the parsed counts (parsing failures are tolerated, not fatal) rather than fail the run outright.

## Out of scope

- Any translation of pigeon's own `BucketConfig`/keyring credentials into rclone-usable config -- `import` never touches pigeon's keyring for source/destination.
- Exposing rclone's performance/retry flags as pigeon CLI flags.
- Classifying rclone's distinct non-zero exit codes into different pigeon-level outcomes.
- Live-tailing the log file for a real-time progress bar while `rclone` runs (the log is only read once, after the subprocess exits).
- Managing or validating the `rclone.conf` that the external deployment process provisions.

## Verification

- `mise run ci` clean (fmt-check + lint + test).
- `rclone_log.rs` unit tests: synthetic JSON-line fixtures (a normal line, a `stats`-bearing line, an `object`-keyed error line, a non-object error line, a blank/garbage line) confirming `parse_and_report` returns the last stats line's totals and tolerates unparseable lines.
- `worker::run_import_job` test: skips at runtime (not `#[ignore]`) if `rclone` isn't on `PATH` (mirroring `pull_transform::media`'s ffmpeg-dependent tests); otherwise copies a file between two `tempdir()`s with no `rclone.conf` needed and asserts the transferred/error counts and destination contents.
- Manual run against a real `rclone.conf`-backed remote, confirming `pigeon.jsonl` carries the `"import: rclone copy starting"`/`"...complete"` lines, the rclone JSON log uploads correctly under the `--report-bucket` run prefix, and an induced failure (bad source path) surfaces a clear error.

## Amendment (2026-10-07): retuned concurrency/rate-limit flags (ADR-0106)

ADR-0106 retunes the fixed flag set in decision point 2 after a 33-run
review found sustained Backblaze B2 rate-limiting (over 99.9% of ~494K
sampled WARN/ERROR lines) and several outright job failures from exhausted
retries. `--transfers`/`--checkers` drop from `32`/`64` to `8`/`16`, and a
new `--tpslimit 10` caps the request rate explicitly. This stays within
decision point 2's "fixed, not CLI flags" stance -- the values changed, the
no-per-run-configurability design didn't.

## Amendment (2026-10-07): `--transfers`/`--checkers`/`--tpslimit` become CLI flags (ADR-0108)

ADR-0108 overturns decision point 2's "fixed, not CLI flags" stance for
exactly these three flags, after a different provider (Scaleway) than the
one ADR-0106 tuned for (Backblaze B2) showed the same fixed `8`/`16`/`10`
values too conservative, hanging for over an hour on a 16GB transfer.
`--transfers`/`--checkers` become optional CLI flags defaulting to
ADR-0106's values; `--tpslimit` becomes an optional CLI flag defaulting to
**unset/no cap**, reverting ADR-0106's hardcoded `10`. Every other fixed
flag (`--retries`, `--buffer-size`, `--multi-thread-*`, `--fast-list`)
stays fixed -- this is a narrow reopening, not a reversal of the broader
anti-flag-sprawl rationale.
