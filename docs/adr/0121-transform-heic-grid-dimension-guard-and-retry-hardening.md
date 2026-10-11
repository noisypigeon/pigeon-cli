# ADR-0121: `transform`'s dimension guard learns about HEIF tile grids and rotation, plus retry-skip/circuit-breaker hardening

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-10.
- **Status**: Proposed.

## Context

A real `pigeon job run transform --input-file-type=heic ...` run (`pigeon.jsonl`, `transform-report.txt`, `transcript.txt` pulled from the run's report bucket) failed almost completely: **13,497 of 13,505 HEIC files failed (99.94%)**, every one with the identical ffmpeg error:

```
ffmpeg failed to transcode <path>: [mov,mp4,m4a,3gp,3g2,mj2 @ 0x...] moov atom not found
<path>: Invalid data found when processing input
```

**Root cause, independently confirmed** by downloading one of the real failing files (`LG - 504.HEIC`) and reproducing locally: it is a valid, non-corrupt HEIF image using HEIF's "grid" derived-image feature -- 48 tiled 512x512 HEVC sub-streams reassembled into one 3024x4032 photo, confirmed via `ffprobe` and macOS's native `sips`/`mdls` (independent of ffmpeg). The job's deployment VM runs ffmpeg 4.4.8; infrastructure is separately upgrading it to 9.0.2 (outside this repo's scope -- that work lives in `noisypigeon/noisypigeon`'s terraform/provisioning).

That upgrade was directly verified here: ffmpeg 9.0.2 was installed locally (Homebrew) and pigeon's exact `transcode_to_jpg` command was re-run against the real failing file.

- ffmpeg 9.0.2 (still **not** compiled with `--enable-libheif`) now correctly reconstructs the grid image on actual transcode -- output is exactly 3024x4032, matching the true photo byte-for-byte in dimensions. **The version bump alone fixes the "moov atom not found" failure.**
- **But it creates a new, more subtle failure.** `transcode_to_jpg`'s own post-encode safety check (`probe_dimensions`, `ffprobe -select_streams v:0 -show_entries stream=width,height`) still only ever sees one raw 512x512 tile stream on the *input* side -- completely unaffected by the ffmpeg upgrade, because a grid-tiled HEIC exposes its 48 tiles as plain per-stream entries with no stream marked `default`/primary. ffmpeg 9.0.2 *does* expose the true reconstructed size, but through a different, newer ffprobe surface: `-show_stream_groups`, `type: "Tile Grid"`, with dimensions nested under `stream_groups[0].components[0].width/height` -- confirmed directly, reporting `4032x3024` for this file. That is *also* not a plain tuple match against the transcoded output's `3024x4032`: the file carries a HEIF-native rotation transform (`irot`, distinct from EXIF orientation, which reads as "normal" on this file) that ffmpeg's real decode pipeline correctly applies during a full transcode but which the raw stream-group canvas metadata does not reflect.

Net effect: once ffmpeg is upgraded, every grid-tiled HEIC transcode will flip from failing with `"moov atom not found"` to failing with pigeon's own `"refusing an output that isn't full-size"` -- a false rejection of a now-correctly-transcoded file -- unless the guard itself is fixed. A fix was verified directly: comparing **pixel area** (`width * height`) instead of exact `(width, height)` ordering sidesteps the rotation-transpose issue entirely (`4032x3024` and `3024x4032` have identical area, 12,192,768 px), while still catching a genuinely wrong or truncated decode.

Separately, and independent of the above, `pigeon.jsonl` showed every one of the 13,497 failures (under the *old* ffmpeg) was retried once before giving up (`TRANSCODE_RETRIES = 2`, 1s linear backoff) -- 26,994 "retrying after error" lines, exactly 2x the failure count -- for zero benefit, since a container-parse failure is deterministic and identical on every attempt. This retry-waste problem is independent of the dimension-guard fix above and will recur for any future systemic incompatibility (grid-related or not), so it is fixed alongside the guard.

## Decision

### 1. Fix the dimension guard to understand HEIF tile grids and rotation

`src/commands/job/transform/media.rs`'s `probe_dimensions` is rewritten to issue one combined JSON `ffprobe` call instead of the old CSV single-stream call:

```
ffprobe -v error -print_format json -select_streams v:0 \
  -show_entries stream=width,height -show_stream_groups <path>
```

Parsed via `serde` structs (mirroring `pull_transform/media.rs`'s existing `FfprobeOutput`/`FfprobeStream` JSON-parsing precedent -- `transform/media.rs` previously had no `serde` dependency at all) covering `streams: Vec<{width, height}>` and `stream_groups: Vec<{components: Vec<{width, height}>}>`. `stream_groups[0].components[0]`'s dimensions (the true reconstructed canvas) are preferred when present; the plain `streams[0]` entry (today's unchanged behavior) is the fallback for every non-grid input -- PNG, a simple single-image HEIC, or the output `.jpg` itself (which never has a stream group).

`transcode_to_jpg`'s before/after check switches from an exact `(width, height)` tuple comparison to a **pixel-area** comparison (`width as u64 * height as u64`), so a HEIF `irot` rotation transpose between the raw input canvas reading and the rotation-corrected transcoded output no longer false-trips the guard. The error message keeps its exact existing substring, `"refusing an output that isn't full-size"` -- this remains a real guard against a truncated or wrong-sized decode; a genuinely bad decode's pixel area will differ, not merely transpose.

### 2. Classify deterministic ffmpeg failures

New `src/commands/job/transform/media.rs::is_non_retryable_transcode_error(err: &str) -> bool`, placed immediately after `transcode_to_jpg`, matching known-deterministic substrings:

- `"moov atom not found"`
- `"Invalid data found when processing input"`
- `"Decoder (codec"` (ADR-0112's own quoted HEIC-decoder-missing message shape)
- `"could not find codec parameters"`
- `"refusing an output that isn't full-size"` (the dimension guard's own message, now area-based per Decision §1 -- still deterministic when it genuinely fires)

Same caution as ADR-0120's `destination.rs` rclone-stderr matching: pattern-matching subprocess output is inherently version-fragile; reconfirm these substrings against whatever `ffmpeg` version is actually deployed.

### 3. Skip retries when classified

`process_one`'s transcode call site (`src/commands/job/transform/worker.rs`) replaces the shared `core::retry::retry_with_backoff` with a new transform-local `retry_transcode_unless_fatal`, mirroring `retry_with_backoff`'s shape and logging exactly but returning immediately -- no sleep, no further attempt -- when `is_non_retryable_transcode_error` matches the failure. Kept local rather than added as a parameter to the shared `retry_with_backoff`, which has 6+ unrelated consumers (`push.rs`, `deduplicate`, `pull_transform`, etc.) -- this codebase's "duplicate until a third consumer" precedent argues against a speculative generic parameter serving one consumer.

### 4. Fail-fast circuit breaker for systemic (not per-file) failure

In `run_transform_job`'s dispatch loop, the first `CIRCUIT_BREAKER_SAMPLE_SIZE = 20` per-file completions are tracked **in completion order** (via the existing `in_flight.join_next()` arm -- concurrency means completion order differs from dispatch order). If **all 20** are transcode failures matching `is_non_retryable_transcode_error`, the job stops dispatching new work and stops enqueueing further live-tail arrivals, lets whatever is already in-flight drain naturally (the in-flight pull subprocess is not killed -- confirmed unnecessary, since the whole pull completed well inside the original incident's 17-minute window per `pigeon.jsonl`'s single "rclone: copy complete" line), and once the loop's existing exit condition is reached, returns `Err` with a clear, actionable message (sample size, match count, one example error, a general -- not HEIC-specific -- suggestion to check the host's ffmpeg capability) instead of `Ok(TransformSummary)`.

With Decision §1 in place, this breaker should rarely if ever trip for the grid-HEIC case specifically -- it remains general protection against other, unrelated systemic failures (a genuinely bad batch, a different future ffmpeg capability gap).

**This must not regress ADR-0116.** ADR-0116 deliberately removed whole-run-abort-on-first-failure after a real incident where one corrupt file among thousands caused 1,605 already-successful files to be silently discarded. This breaker is categorically different: it requires **100% of a real 20-file sample** to fail identically, never "one bad file," and fires only on a strong, specific systemic signal. With the true 8/13,505 success rate under the old ffmpeg, the probability of a random 20-completion sample containing zero successes was `(13497/13505)^20 ~= 99.1%` -- strict 100%-of-20 would almost certainly have caught that incident. No tolerance (e.g. 18/20) is used: it would add an unmotivated magic number without incident evidence requiring it, while strict-100% already covers the real case and minimizes false-positive risk on a healthy or mostly-healthy run. A batch under 20 total files can never trip the breaker -- acceptable, since at that scale the wasted-time harm this ADR targets is already small, and ADR-0116's own per-file protection still covers correctness there.

`run_transform_job`'s own doc comment already reserves `Err` for "infrastructure-level failures" (as distinct from per-file failures captured in `TransformSummary`) -- a systemic environment-capability gap fits that contract. `wizard.rs`'s existing `Err(err) => { report_upload::say_error(...); report_upload::write_summary_report(...); (fail(err), report_path) }` branch already handles this correctly with no modification -- confirmed by direct reading. `push.rs`'s only error message shape cannot overlap with any of the five classifier substrings, so the classifier naturally never misfires on a push-stage (as opposed to transcode-stage) failure, with no new structured stage-tracking needed.

## Consequences

- Grid-tiled real-world HEIC photos (the common case for modern phone cameras) now transcode correctly end-to-end once ffmpeg is upgraded, instead of either failing outright (old ffmpeg) or being falsely rejected by pigeon's own guard (new ffmpeg, unfixed guard).
- A deterministic ffmpeg failure now costs one attempt instead of two, and no longer pays a guaranteed 1s backoff sleep for zero benefit.
- A systemic, near-total transcode failure (of any cause, not just HEIC/grid-related) now fails the whole run within seconds instead of after a long, silent grind through thousands of guaranteed-identical failures.
- Accepted, explicitly stated limitation: a short batch (under 20 files) can never trip the circuit breaker regardless of failure rate; this is acceptable since the wasted-time harm this ADR targets is already small at that scale.
- The dimension guard's area-based comparison is a strict relaxation relative to the old exact-tuple comparison for every already-passing case (identical dimensions have identical area) -- no existing correct transcode becomes newly rejected.

## Out of scope

- Any change to `pull_transform`'s own media/dimension-verification logic (separate module, separate design, untouched).
- A structured failure-stage enum beyond `FileOutcome.detail`'s free text -- still no consumer justifies it (ADR-0116 precedent, reaffirmed).
- A CLI flag for the circuit breaker's sample size -- kept a plain constant, matching `TRANSCODE_RETRIES`'s existing precedent.
- Killing the in-flight pull subprocess when the circuit breaker trips -- confirmed unnecessary for the incident this ADR traces.
- Upgrading ffmpeg itself, or any bundled alternative HEIC decoder (`libheif`/`heif-convert`) -- infrastructure/provisioning work outside this repo.

## Verification

- `mise run ci` clean (fmt-check + lint + test), including new/updated tests for `is_non_retryable_transcode_error`, the area-based dimension comparison, `retry_transcode_unless_fatal`, and the circuit breaker's trip/no-trip behavior.
- Manually confirmed during this ADR's own investigation: `probe_dimensions`'s new stream-group-aware logic and the area-based comparison, run against the real `LG - 504.HEIC` file with real ffmpeg 9.0.2, correctly accept what is now a correct transcode.
- Manual (post-implementation): run `pigeon job run transform --input-file-type=heic ...` against a local directory of ≥20 fixture-style corrupt files and confirm a fast (seconds, not minutes) `Err` exit with a clear diagnostic message.
