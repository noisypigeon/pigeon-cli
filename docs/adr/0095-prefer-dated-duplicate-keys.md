# ADR-0095: prefer dated keys when choosing dedupe's kept copy

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-03.
- **Status**: Accepted.

## Context

A post-run analysis of a real `job run dedupe` (`import-bgt6tk-media` → `deduplication-xl9ux9-media`, 2h42m, 44,618 uploads, 0 failures) found one real data-quality bug: `place_and_report` (`src/commands/job/dedupe/dedup.rs:69`) sorts candidate files by `original_key` alone, and whichever key sorts lexicographically first becomes the "kept" copy for a given content hash. The source bucket uses a `0000-00-00-*` filename sentinel for files with no known date, and `"0000..."` sorts before `"2018..."`, so 797 of 855 duplicate pairs in this run kept the undated copy and discarded a dated one — e.g. `2018/jpg/2018-01-02-image-12.jpg` was dropped in favor of `jpg/0000-00-00-image-370.jpg`. The destination bucket now carries less date information than the source had.

This ADR fixes the selection logic so future runs don't repeat the mistake. The already-written `deduplication-xl9ux9-media` bucket is **not** remediated — out of scope here by decision, not oversight; see Consequences. Three smaller observations from the same analysis (wasted duplicate downloads, a silent stderr-only job failure invisible to the JSONL log, and download terminal noise) are real but unrelated to this bug; they're listed under Out of scope below.

## Decision

Add a pure helper to `src/commands/job/dedupe/dedup.rs`:

```rust
/// Whether `key`'s file name carries the source data's "no known date"
/// sentinel (`0000-00-00-...`) rather than a real date prefix. Dedupe's
/// keep-selection uses this to avoid preferring an undated copy over a
/// dated one just because "0000" sorts before a real year.
fn is_undated_key(key: &str) -> bool {
    let name = Path::new(key)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(key);
    name.starts_with("0000-00-00-")
}
```

Change `place_and_report`'s sort (`dedup.rs:69`) from a plain `original_key` comparison to:

```rust
files.sort_by(|a, b| {
    is_undated_key(&a.original_key)
        .cmp(&is_undated_key(&b.original_key))
        .then_with(|| a.original_key.cmp(&b.original_key))
});
```

Dated keys now sort ahead of undated ones; `original_key` stays the tie-break within each group, so placement order is still fully reproducible across re-runs (the property the existing doc comment calls out). No other part of `place_and_report`, `place_one`, `write_report`, or the `worker.rs` call site changes — the fix is entirely in the comparator.

## Consequences

- Future `dedupe` runs keep the more informative (dated) copy whenever a duplicate pair has one dated and one undated key.
- The already-written `deduplication-xl9ux9-media` bucket keeps its 797 undated-kept duplicates as-is; this ADR does not remediate it, and re-running `dedupe` against the same source would not retroactively fix it either, since the upload phase only adds or overwrites keys — it never deletes one (so last run's kept undated copy would stay alongside any newly-uploaded dated one, not replace it).
- No schema, flag, or CLI surface changes — this is a pure bugfix to an internal comparator.

## Out of scope

- 44.2 GB of duplicate content downloaded and hashed before being discarded in the analyzed run (e.g. two 16.5 GB byte-identical `.mov` copies). A size/ETag pre-pass over the source listing — skip hashing when size is unique, treat same-size-same-ETag as already-proven-identical — could avoid this, but it's a separate performance change to the download/hash phase, not the keep-selection bug fixed here. ([#17](https://github.com/noisypigeon/pigeon-cli/issues/17))
- A failed `job run dedupe` invocation in the same analysis left no cause in the log (exit code 1 after 21s, zero WARN/ERROR events — the error went only to stderr). `run_instrumented` (`src/observability/mod.rs:111-130`) only ever records the exit code of the whole command closure, so a stderr-only early failure is invisible to the JSONL log. Worth a dedicated observability ADR. ([#18](https://github.com/noisypigeon/pigeon-cli/issues/18))
- Terminal noise from small-file download announcements: `ANNOUNCE_DOWNLOAD_THRESHOLD_BYTES = 50 MiB` (`src/commands/job/download.rs:26`) printed roughly 2,000 `Downloading ...` lines for routine 50-90 MB phone videos in the analyzed run, burying the handful of genuinely large (8-17 GB) call-outs the threshold was meant to highlight. ([#19](https://github.com/noisypigeon/pigeon-cli/issues/19))
- Possible upload-concurrency headroom (observed ~17 MB/s per stream across 16 streams in the analyzed run) is unverified without a NIC/link check first, and isn't a code change — not filed as an issue, just noted for whoever next tunes `--upload-concurrency`.

## Verification

Unit tests: `is_undated_key` on a `0000-00-00-*` name, a real dated name, and a no-date-prefix name; an integration test, `place_and_report_prefers_a_dated_key_over_an_undated_duplicate`, feeding `place_and_report` an undated and a dated key with identical content hashes (in an order where the undated key sorts first under plain lexicographic order, to actually exercise the fix) and asserting the dated key is kept. `mise run ci` clean. Existing `place_and_report_dedupes_a_cross_key_duplicate_and_records_it` and sibling tests continue to pass unchanged, confirming the tie-break preserves current behavior when neither key is undated.
