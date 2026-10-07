# ADR-0106: tune `import`'s rclone concurrency/retry flags for destination rate limits

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-07.
- **Status**: Proposed.

## Context

The same 33-run review (ADR-0104's context) aggregated every WARN/ERROR line
across all 33 archived `pigeon.jsonl` files: roughly 494,000 lines total, of
which **over 99.9%** are a single root cause -- Backblaze B2 responding `429
Too Many Requests` to an rclone operation. Broken down by the underlying
rclone error message across all runs:

| Error | Count |
|---|---|
| `Failed to set modification time: TooManyRequests` | 464,526 |
| `Failed to copy: failed to open source object: Too Many Requests` | 24,756 |
| `Failed to copy: s3 upload: 429 Too Many Requests` | 4,496 |
| `Failed to calculate src hash: TooManyRequests` | 557 |
| `Failed to calculate dst hash: TooManyRequests`/`RequestCanceled` | 103 |
| `error reading source directory: Too Many Requests` | 10 |
| (everything else, incl. the unrelated `directory not found` case) | ~20 |

4 of the 33 runs hit `job failed` (rclone exit 1 or 3) outright after
exhausting retries -- `consolidate-setsye-segment-1-4957` (`directory not
found` on a `mega:` source, unrelated to rate-limiting),
`consolidate-a5tl5v-segment-2-1235`, `consolidate-aut1w2-segment-1-check-1232`,
and `consolidate-e2qpr0-segment-2-1241` (all three B2 429 exhaustion). One run
(`consolidate-a5tl5v-segment-2-1235`) logged 205,306 of its own 342,921 total
rclone log lines (59%) as 429 errors before finally reporting "136011
transferred, 1 error" -- the published summary radically understates how
close to collapse that run actually was; only rclone's internal
`--retries 5` budget kept 205,305 of those 205,306 failures from becoming
visible at all.

Root cause in this codebase: `run_import_job`
(`src/commands/job/import/worker.rs:103-146`) invokes `rclone copy` with a
fixed, non-configurable (per ADR-0101's explicit design) set of flags:

```
--transfers 32 --checkers 64 --fast-list --buffer-size 32M
--multi-thread-streams 4 --multi-thread-cutoff 256M
--retries 5 --low-level-retries 20
```

Up to 96 concurrent HTTP operations (32 transfers + 64 checkers) against one
B2 account, with no `--tpslimit` or other explicit request-rate ceiling.
Against sources with huge counts of small objects (a Google Photos Takeout
export's per-media `.json` sidecar files, in particular -- the dominant
pattern in the sampled 429s), this concurrency produces a sustained request
rate well past whatever B2 is willing to sustain for the account, and the
fixed 5/20 retry budget is not always enough to absorb it, especially once
several segment jobs are hitting the same B2 account at once (see the
companion ADR in `noisypigeon/noisypigeon` about concurrent job scheduling).

ADR-0099 explicitly worked to cut `pigeon.jsonl` log volume (143k lines down
to "a handful" by default); this 429 flood reintroduces hundreds of thousands
of lines through a different path (every rclone-level object failure is
logged as a `tracing::warn!` by `rclone_log.rs`), undoing that improvement in
practice for any run that hits sustained rate-limiting.

## Decision

1. **Lower the default concurrency** for `import`'s rclone invocation --
   reduce `--transfers`/`--checkers` from 32/64 to values informed by what
   B2 actually sustains without 429ing under this workload (start conservative,
   e.g. 8/16, and tune up from measured headroom rather than down from
   failure).
2. **Add an explicit rate ceiling.** rclone supports `--tpslimit` (transactions
   per second) independent of `--transfers`/`--checkers` concurrency; add a
   conservative default so the client self-limits before the server 429s,
   rather than discovering the limit via mass failures and retries.
3. **Revisit the fixed-flags stance from ADR-0101** for this specific
   dimension: either keep concurrency/rate flags fixed but tuned to the
   measured B2 behavior above, or carve out a narrow, justified exception
   (e.g. a `--rate-limit-profile` or per-destination-provider default) if one
   fixed value can't serve every provider `import` is used against. ADR-0101's
   broader "not configurable per run" rationale (simplicity, no per-run
   flag sprawl) is not being reopened wholesale -- only the specific
   evidence that the current fixed values are mismatched to at least one
   real destination.
4. **Collapse repeated-429 log volume.** Independent of the concurrency fix,
   `rclone_log.rs`'s per-object-failure `tracing::warn!` should rate-limit or
   summarize consecutive identical-cause failures (e.g. "N more 429s in the
   last Ms" rather than N individual lines), consistent with ADR-0099's
   stated log-volume goal, so a sustained rate-limit episode doesn't reinflate
   the log even when the run ultimately succeeds.

## Consequences

- Fewer outright `job failed` exits caused by exhausting rclone's retry
  budget under sustained rate-limiting.
- Dramatically lower `pigeon.jsonl` volume on runs that hit rate-limiting but
  still succeed (the common case: 29 of the 33 sampled runs eventually
  transferred everything despite heavy 429 volume).
- Lower destination request rate may increase wall-clock time for large
  transfers; this is the direct trade-off against this ADR's reliability
  goal, and should be measured against real transfer-time data once
  retuned.

## Out of scope

- The one `directory not found` failure (`consolidate-setsye-segment-1-4957`,
  a `mega:` source) is unrelated to rate-limiting and not addressed here --
  it looks like a source-mount-readiness race at the deployment layer; see
  the companion ADR in `noisypigeon/noisypigeon`.
- Per-provider-specific rclone backends (e.g. B2-native `--b2-*` flags vs.
  generic S3-compatible flags) -- out of scope for this pass; the fix here is
  concurrency/rate-ceiling only, not a backend migration.

## Verification

- Manual: rerun one of the previously-429-heavy buckets
  (`backblaze-google-consolidation`) with the retuned flags and confirm the
  WARN count and wall-clock time, comparing against this ADR's baseline
  numbers.
- `mise run ci` clean.
