# ADR-0122: video input support for `pigeon job run transform`

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-10.
- **Status**: Proposed.

## Context

`pigeon job run transform` (ADR-0112, reworked by ADR-0116, observability/resume
fixed by ADR-0120, hardened by ADR-0121) recognizes exactly three
`--input-file-type` values -- `png`, `jpeg`, `heic` -- and always transcodes
them to full-size, maximum-quality `.jpg`. `InputFileType`
(`src/commands/job/transform/format.rs`) was deliberately named (and the job
deliberately named `transform`, not `image-transform`) so "a later iteration
can add a non-image input kind to this same enum/command without a rename" --
this is that iteration, driven by a real need: a large personal `.mov`/
`.m4v`/`.mp4` video collection that should be shrunk through the same
pull-&gt;transcode-&gt;push pipeline, with some quality loss acceptable in exchange
for materially smaller files.

This needs two things the codebase does not have today for this job:

1. Recognition of video extensions as `--input-file-type` values at all --
   today every input is an image, and output is unconditionally `.jpg`
   (`placement::compute_destination_name` hardcodes the suffix).
2. A **quality level** axis for the resulting compression. No such concept
   exists anywhere in this codebase: every existing ffmpeg quality/CRF/
   bitrate setting (`pull_transform::media::recode`'s per-`VideoFormat`
   constants, `transform::media::transcode_to_jpg`'s fixed `-q:v 1`) is a
   single hardcoded number per format, chosen once and never user-selectable
   (ADR-0074's explicit "fixed, conservative preset per category... not a
   dynamic per-file quality search" design).

### Codec/container choice

H.265/HEVC in an MP4 container, not H.264 (which is what `pull_transform`'s
existing `VideoFormat::Mp4` menu option uses for a different purpose --
general-purpose recoding, not space-optimized archival of a large personal
library). At equivalent perceptual quality, H.265 output runs roughly 30-50%
smaller than H.264 -- the right tradeoff specifically because the goal here
is shrinking a large volume of video, not just normalizing format. HEVC's
CRF quality knob runs lower-is-better (every ~6 CRF points roughly doubles or
halves file size); conventional tiers are archival-grade around CRF 18-22,
a web/streaming "sweet spot" around CRF 23-28 (26 is a typical default
"medium"), and small/batch files around CRF 28-32.

## Decision

### 1. Extend `InputFileType` with three video variants

`InputFileType` (`transform/format.rs`) gains `Mov`, `M4v`, `Mp4` alongside
the existing `Png`/`Jpeg`/`Heic`. `--input-file-type` keeps its existing
single-flag, one-extension-per-run semantics unchanged -- it still names
exactly one extension to pull per run, not a video/not-video switch.
`parse()` accepts `mov`/`m4v`/`mp4` case-insensitively (same leniency
precedent as the existing `jpg` alias for `jpeg`); `extension()`/`Display`
extend accordingly. `InputFileTypeInput::prompt`'s (`transform/wizard.rs`)
interactive `Select` menu grows from 3 to 6 items.

Two new helpers support everything downstream:

```rust
impl InputFileType {
    pub(crate) fn is_video(self) -> bool {
        matches!(self, InputFileType::Mov | InputFileType::M4v | InputFileType::Mp4)
    }
    pub(crate) fn output_extension(self) -> &'static str {
        if self.is_video() { "mp4" } else { "jpg" }
    }
}
```

### 2. Single canonical video output -- never copy-through

All three video input kinds always transcode -- unlike `jpeg`, there is no
copy-through path for an input that's already `.mp4`, because the point of
this feature is compression, not format normalization. Output is always
`.mp4` via `libx265`, `-tag:v hvc1` (required for QuickTime/Apple-device
playback of HEVC-in-MP4 -- ffmpeg's default `hev1` tag is not recognized by
Apple's own players), `-preset medium`. There is no user-selectable output
*container* menu here (unlike `pull_transform`'s `VideoFormat`) -- `transform`
stays single-canonical-output per input category, exactly matching its
existing jpg-for-every-image-kind precedent.

### 3. New `--video-quality low|medium|high|lossless` flag

A small, vetted CRF menu -- mirroring ADR-0077's per-category-menu precedent
(a short hardcoded list of pre-chosen ffmpeg args, never raw CRF/bitrate
exposed to the user) -- rather than a dynamic per-file search (ADR-0074):

| Tier | ffmpeg args | Notes |
|---|---|---|
| `low` | `-crf 30` | Small files, visible quality loss; fine for less-valued footage. |
| `medium` (default) | `-crf 24` | Deliberately tuned a bit better than the conventional ~26-28 "medium" -- this project's own stated preference for personal archival video. |
| `high` | `-crf 20` | Near-visually-lossless; noticeably larger files. |
| `lossless` | `-x265-params lossless=1` | True mathematically lossless; large files, use sparingly. |

Audio is always `-c:a aac -b:a 256k` regardless of the chosen video tier --
kept simple rather than scaled per tier, an explicit simplification stated
here, not a silent omission.

### 4. Conditional, defaulted prompt

`--video-quality` is only resolved/prompted when `--input-file-type` resolves
to a video kind (`InputFileType::is_video()`); it is never asked for png/
jpeg/heic runs. Unlike `--input-file-type` itself, an absent value on a
non-interactive video run **defaults to `medium`** rather than hard-erroring
-- a safe default exists here, and this flag is not as foundational to the
run as the source/destination/type flags are (mirrors `TransfersInput`'s/
`CheckersInput`'s existing default-not-error precedent, not
`InputFileTypeInput`'s hard-error one).

### 5. Output extension generalized in `placement.rs`

`compute_destination_name` gains an explicit `output_extension: &str`
parameter (`"jpg"` for image kinds, `"mp4"` for video, via
`InputFileType::output_extension()`), replacing the previously-hardcoded
`.jpg` suffix. Scratch-path naming (`<destination-name>.scratch`,
`worker::process_one`) is unaffected in shape -- it already derives
whatever extension `compute_destination_name` returns.

### 6. New `media::transcode_video`

Mirrors `transcode_to_jpg`'s shape and discipline:

- Explicit `-f mp4` muxer flag -- ADR-0115's exact lesson applies again
  here: the scratch path ends in `.scratch`, not `.mp4`, so ffmpeg can never
  be allowed to infer the muxer from the on-disk extension.
- No `-vf scale=...` -- original resolution is always preserved, by
  construction, same as the image path.
- A defensive post-encode `ffprobe` guard reusing the existing pixel-area
  dimension comparison (works for a video stream's `width`/`height` the same
  way it does for an image's) *and* a new duration comparison (small
  tolerance, e.g. 0.5s, to absorb container/muxing overhead) -- video has a
  truncated/partial-encode failure mode images don't (a process that exits 0
  having only written part of the stream), which only a duration check can
  catch.

### 7. Retry/circuit-breaker machinery reused unchanged

A video transcode failure is retried and classified exactly like a png/heic
one, via the existing `retry_transcode_unless_fatal` and ADR-0121's sampled
circuit breaker -- no new machinery. `is_non_retryable_transcode_error`'s
signature list gains one new entry, `"Unknown encoder"`, so a deployed
ffmpeg built without `libx265` support is classified as the same kind of
systemic, non-retryable capability gap as a missing HEIC decoder already is
-- and, consistent with that existing HEIC precedent, there is still no
separate up-front video-capability preflight probe; the first real failure
and the circuit breaker surface it.

### 8. Plumbing

`TransformPlan`/`TransformJob` gain a `video_quality: VideoQuality` field --
concrete, not `Option`, so `worker::process_one`'s match never has to unwrap
one; it's simply unused on an image run (default `Medium`, harmless).
`worker::outcome_for`/`process_one`'s match on `InputFileType` grows a
`Mov | M4v | Mp4` arm calling `transcode_video`; both still report
`Outcome::Transcoded`, lumped with png/heic (there is no new `Outcome`
variant). The CLI surface gains `--video-quality` on `JobType::Transform`
(`job/cli.rs`), threaded through `dispatch`/`dispatch_async`
(`transform/wizard.rs`) and a new `VideoQualityInput` `WizardInput`
implementation resolved immediately after `input_file_type`, conditionally.

## Consequences

- A personal video library in `.mov`/`.m4v`/`.mp4` can now be run through the
  same resumable, observable, retry-hardened pipeline images already use,
  landing as space-optimized HEVC `.mp4` at a chosen quality/size tradeoff.
- `--video-quality medium`'s CRF 24 is deliberately not the "generic"
  H.265 medium -- it is tuned to this project's own stated preference.
  Revisiting that number later only requires changing one constant.
- `transform`'s "always `.jpg`" assumption, true since ADR-0112, is gone;
  any future input kind added to this job must specify its own
  `output_extension()` rather than relying on the previous implicit default.
- Reusing the existing retry/circuit-breaker machinery means a systemic
  video-capability gap (e.g. a `libx265`-less ffmpeg build) fails the whole
  run fast, exactly like a systemic HEIC-capability gap already does --
  no new failure-handling concept to reason about.

## Out of scope

- Per-tier audio bitrate/codec scaling -- always AAC 256k regardless of
  video quality tier.
- Any resize/scale filter for video, mirroring the image path's existing
  no-resize guarantee.
- Preserving multiple audio tracks or subtitle streams beyond ffmpeg's own
  default stream selection.
- A user-selectable output *container* (mkv/webm) for this job -- that
  remains `pull_transform`'s `VideoFormat` menu's responsibility, not
  `transform`'s, which stays single-canonical-output per category.
- Persisting a chosen `--video-quality` across runs.

## Verification

- `mise run ci` (fmt-check + lint + test) clean, including new/updated unit
  tests for `InputFileType::parse`'s new variants, `VideoQuality::parse`,
  `compute_destination_name` ending in `.mp4` when passed that extension,
  `is_non_retryable_transcode_error` matching `"Unknown encoder"`, and
  `outcome_for` mapping every video kind to `Transcoded`.
- A new ffmpeg-available-gated integration test in `worker.rs`, mirroring
  the existing png/heic ones, pushes a tiny synthetic `lavfi`-generated
  video through the full pipeline and confirms it lands as `.mp4`.
- Manual smoke test: `mise run pigeon -- job run transform
  --input-file-type mp4 --video-quality medium --source-path <local dir with
  a short test .mp4> --destination-path <local dir> --non-interactive`, then
  `ffprobe` the output to confirm codec `hevc`, tag `hvc1`, and matching
  dimensions/duration against the source.
