# CLI-0141: fail fast on an audio-only `transform` input, and fix its lost tracing spans

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-12.
- **Status**: Accepted.

## Context

A `pigeon job run transform --input-file-type mp4` run pulled 7289 files
cleanly (`rclone`: 0 errors) but failed 483 of 7230 dispatched transcodes.
Reading `transform-report.txt`, the rclone pull JSONL, and `pigeon.jsonl`
(following the `analyze-job-run` skill) found two independent, confirmed
defects.

**480 of 483 failures: an audio-only file routed into the video
pipeline.** `InputFileType`'s video variants (`Mov`/`M4v`/`Mp4`,
`src/commands/job/transform/format.rs`) are dispatched purely by file
extension; nothing checks whether a file actually contains a video stream.
`transcode_video` (`media.rs:313`) always runs `ffmpeg ... -map 0:v:0 -map
0:a:0? ...` -- `0:v:0` is mandatory. The failing files (`audioclip*.mp4` in
the source tree) are confirmed genuinely audio-only via `ffprobe`: one
stream, `codec_type=audio`, no video stream at all -- almost certainly
voice-note recordings saved with a video-looking extension. Reproducing
`transcode_video`'s exact command against one of them reproduces the exact
logged error verbatim:

```
Stream map '' matches no streams.
To ignore this, add a trailing '?' to the map.
Failed to set value '0:v:0' for option 'map': Invalid argument
```

This string is not in `is_non_retryable_transcode_error`'s
`NON_RETRYABLE_SIGNATURES` (`media.rs:396`), so every one of these 480
files was retried twice (1s/2s linear backoff) before being recorded
failed -- for a 100%-deterministic outcome, pure waste: ~960 unnecessary
`ffmpeg` invocations and roughly 480 seconds of pure sleep on a rerun
against the same source.

**The remaining 3 failures are a narrower, separate case**:
`IMG_4023.MP4`, `IMG_2213.MP4`, and `LG - 2253.mp4` each contain *multiple*
HEVC video streams of different lengths (Apple/Android "Live
Photo"-style bracketed clips) plus one audio stream. `-map 0:v:0` pins
stream index 0, but the container's own `format=duration` metadata doesn't
match that stream's real length, so `transcode_video`'s existing duration
guard (correctly) refuses the output as a suspected truncated encode. This
is a real edge case, but picking the "right" video stream among several
candidates of different lengths needs its own design; it is deferred (see
Out of scope).

**Separately, and independently of either failure mode: `transform`'s
per-file `tokio::spawn` never propagates the ambient tracing span.**
`run_transform_job`'s dispatch loop (`worker.rs:842`,
`in_flight.spawn(async move { process_one(...).await })`) spawns a bare
`async move` block with no `.instrument(...)` call. This is the exact bug
class CLI-0098 fixed for `deduplicate`/`pull_transform` (which captures
`tracing::Span::current()` once and calls `.instrument(command_span.clone())`
on each spawn -- `deduplicate/worker.rs:452,600`), but it was never applied
to `transform`'s own spawn site. Confirmed directly against this run's
`pigeon.jsonl`: every one of its 966 `"retrying after error"` WARN and 483
`"retries exhausted"` ERROR events (`retry_transcode_unless_fatal`,
`worker.rs:95`) carries **no `span` key at all** -- not even an empty one.
The practical consequence is that the `analyze-job-run` skill's documented
procedure for isolating a run (`jq 'select(.spans[]?.command ==
"job.transform")'`) returns **zero** hits for any of these 483 failures;
they were only found by grepping the raw file for literal strings and
manually reconstructing the run's time window from `elapsed_ms`. This made
an already-confusing failure (root cause above) much harder to diagnose
than it needed to be, and would do the same to any future `transform`
failure of any kind.

`process_one`'s own `#[instrument(..., level = "debug")]` span
(`worker.rs:295`, carries `relative_path`) doesn't help here by default --
and per CLI-0099's precedent (demoting per-file spans to `debug`
specifically to bound log volume on multi-thousand-file runs), it should
stay there. WARN/ERROR events are inherently low-volume (only emitted on
failure), so the right fix for *those* call sites specifically is a
structured field passed explicitly, not a span-level change.

## Decision

### 1. `transcode_video` fails fast and accurately on an audio-only input

Add `has_video_stream` to `media.rs`:

```rust
async fn has_video_stream(path: &Path) -> bool {
    // ffprobe -v error -select_streams v -show_entries stream=index -of csv=p=0 <path>
    // true iff the command exits 0 and stdout is non-empty
}
```

Called at the top of `transcode_video`, before the `ffmpeg` command is
built. On `false`, return immediately:

```
{input} has no video stream (likely an audio-only recording saved with a
video extension); see CLI-0142 for an audio-preserving input type
```

Add this message's signature (`"has no video stream"`) to
`NON_RETRYABLE_SIGNATURES` -- it is exactly as deterministic as the other
entries in that list, and classifying it as such is what removes the
wasted retries.

### 2. Fix span propagation, matching CLI-0098's precedent exactly

Capture `let command_span = tracing::Span::current();` once before
`run_transform_job`'s dispatch loop begins (the same placement
`deduplicate/worker.rs:452` uses relative to its own worker loop), and add
`.instrument(command_span.clone())` to the `in_flight.spawn(async move {
... })` call at `worker.rs:842`, mirroring `deduplicate/worker.rs:600`
exactly. This alone restores the outer `"command": "job.transform"` span
on every per-file event, so the skill's standard jq filter works for this
job again.

### 3. Structured `relative_path` on retry events, without touching span levels

Add a `context: &str` parameter to `retry_transcode_unless_fatal`
(`worker.rs:95`), threaded from both of `process_one`'s call sites
(`worker.rs:350` png/heic, `worker.rs:357` video), and include it as an
explicit field (`relative_path = %context`) on its existing
`tracing::warn!`/`tracing::error!` calls. `process_one`'s own
`#[instrument(level = "debug")]` span stays at `debug`, per CLI-0099 --
this is a narrower, lower-cost fix that only touches the already-rare
WARN/ERROR call sites.

## Out of scope

- **The 3 multi-video-stream duration-guard failures**
  (`IMG_4023.MP4`, `IMG_2213.MP4`, `LG - 2253.mp4`). `-map 0:v:0` picking an
  arbitrary track among several differently-lengthed real video streams
  needs its own design for how to pick the "right" one; a narrow, rare
  edge case next to the 480-file failure mode this ADR fixes. Filed as an
  `out-of-scope` issue per CLI-0031. ([#77](https://github.com/noisypigeon/pigeon-cli/issues/77))
- **Actually preserving audio-only content.** This ADR only makes the
  failure fast and accurately diagnosed; it does not transcode or push
  anything for an audio-only input. See CLI-0142.
- Any change to the circuit breaker (CLI-0121) or its sample size --
  unaffected; the new failure signature is handled identically to every
  other non-retryable one it already special-cases.

## Consequences

- An audio-only file misrouted into a video-typed `transform` run now
  fails on its first attempt, with a message naming the actual problem,
  instead of two retries ending in a confusing raw `ffmpeg` stream-map
  error. The file is still left unpushed (CLI-0124's existing "a file that
  fails is left in place" behavior is unchanged) -- this ADR only improves
  the diagnosis, not the outcome.
- `transform`'s per-file WARN/ERROR events become discoverable via the
  standard `analyze-job-run` jq filter for the first time. No existing
  dashboard or log consumer depends on the current span-less shape, so
  this is additive.
- `is_non_retryable_transcode_error`'s signature list grows by one entry;
  as with every existing entry, it is a `ffmpeg`-stderr substring match and
  may need reconfirming against a future `ffmpeg` version.

## Verification

- `mise run ci` clean (fmt-check + lint + test).
- `media.rs`: new test `transcode_video_fails_with_a_clear_error_on_an_audio_only_input`
  (an `anullsrc`-based synthetic fixture, same construction style as the
  existing `generate_test_video` helper); a test asserting
  `is_non_retryable_transcode_error` matches the new signature.
- `worker.rs`: existing `retry_transcode_unless_fatal` tests updated for
  the new `context` parameter.
- Manual: rerun `job run transform --input-file-type mp4` against a small
  local directory containing one real video and one known audio-only file.
  Confirm (a) the audio-only file fails immediately with the new message
  and no retry warnings, (b) `jq 'select(.spans[]?.command ==
  "job.transform")'` against `pigeon.jsonl` now finds that failure's
  `ERROR`/`WARN` lines, each carrying a `relative_path` field.

## Amendment (2026-10-12): the audio-preserving path now exists (CLI-0142)

This ADR's new failure message pointed at "CLI-0142 for an audio-preserving
input type" before that ADR existed. It now does: `--input-file-type
mp4-audio` transcodes an audio-only `.mp4` to Opus `.opus` rather than
failing it, so the 480 files this ADR could only diagnose are processable.
Nothing here changes -- a plain `mp4` run still fails such a file fast and
non-retryably, which is the correct outcome for a run that asked for video;
the two kinds' opposite treatment of the same file is the intended
distinction. CLI-0142 reuses this ADR's `has_video_stream` helper inverted,
as the guard that refuses to strip video off a file that has it.

## Amendment (2026-10-14): `has_video_stream` now distinguishes cover art (CLI-0153)

This ADR's `has_video_stream` counts any stream matching
`-select_streams v`, which was exactly right for the problem it solved --
telling an audio-only `.mp4` from a real video. It is wrong for a *music*
file, because ffmpeg models embedded cover art as a video stream too.

CLI-0153 found this before shipping the `m4a`/`mp3`/`aac` kinds, and
measured it: an `.m4a` with an attached picture reports stream index 1 under
this probe. Had it shipped unchanged, essentially every tagged file in a real
library would have hit CLI-0142's has-a-video-stream guard, been classified
wrong-input-kind so CLI-0143 deleted its local copy, and tripped CLI-0121's
circuit breaker into aborting the run.

The probe now also reads `stream_disposition=attached_pic` and ignores
streams where it is set. The field defaults to `0` when absent, so an
unexpected probe shape reads as real video and *preserves* this ADR's guard
rather than disarming it. Both of this ADR's own behaviours -- an audio-only
input rejected by `transcode_video`, a real video accepted -- are unchanged
and still pinned by its tests.
