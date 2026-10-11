# ADR-0125: fix false-positive transcode failures from ffmpeg's implicit autorotate

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-11.
- **Status**: Accepted.

## Context

A real `job run transform` run produced hundreds of `pigeon.jsonl` entries like:

```
transcode failed with a non-retryable error; skipping remaining retries
error: transcoding <path>.mov changed pixel area from (1920, 1440) (2764800 px) to
       (1308, 1744) (2281152 px); refusing an output that isn't full-size
```

(and permutations -- `(1920,1440)→(1744,1308)`, `(1440,1920)→(1308,1744)`, etc. -- same
pattern, different device orientation).

`transcode_video` (`src/commands/job/transform/media.rs`) applies no `-vf` filter and
relies on that fact to justify comparing input/output pixel *area* via `probe_dimensions`,
which reads only `ffprobe`'s raw `stream=width,height`. That comparison assumes "no filter
on our command line" means "no reshape happens at all." It doesn't: ffmpeg has, since
roughly 4.3, applied an *implicit* "autorotate" step during decode whenever a container
carries a display transform (QuickTime/MP4's `tkhd` matrix) -- independent of any `-vf`
pigeon itself passes. For most phone-captured video this is a harmless ±90°/180° rotation
with no area change, which the ADR-0121 area-based guard already tolerates. But for some
real camera-captured `.mov` files, the embedded matrix encodes a rotation *plus* a
non-unity scale component, and ffmpeg faithfully reshapes the decoded frame to match:

```
1308 / 1440 = 1744 / 1920 = 109/120   (exact integer ratio)
2281152 / 2764800 = (109/120)²        (exact area ratio -- a uniform scale, not a crop)
```

This is a **correct** transcode of a real file, not corruption -- but the guard has no way
to tell the difference, so it hard-fails via `is_non_retryable_transcode_error`. Video has
no copy-through/keep-original fallback (ADR-0122 Decision §2: video is never copy-through),
so the affected file is marked `Outcome::Failed` and left on disk indefinitely, never
pushed, never garbage-collected (ADR-0124).

This directly breaks an assumption ADR-0121 established for a *different* metadata model
(HEIF's `irot` box, always a pure, area-preserving transpose) which ADR-0122 then reused
for video "the same way it does for an image's" without re-examining whether video's
`tkhd`-matrix semantics behave the same way. They don't -- QuickTime/MP4 display matrices
can bake in scale, not just rotation.

`pull_transform::media` (`src/commands/job/pull_transform/media.rs`) has the identical
narrow probe (no SAR/rotation fields ever read) and an even *stricter* exact-tuple
dimension comparison in `verify`, so it would misfire on this same class of file. But
`recode_and_verify` (`pull_transform/worker.rs`) falls back to keeping the original file
byte-for-byte on persistent verify failure (ADR-0074), a design ADR-0116 explicitly left
untouched -- so `pull_transform` degrades gracefully where `transform` hard-fails. That
asymmetry is pre-existing and deliberate; this ADR does not revisit it.

### Verifying the fix mechanism

Before committing to a fix, the mechanism was reproduced directly against a live ffmpeg
9.0.2 install (not just reasoned about):

1. A base test `.mp4` was remuxed with `ffmpeg -display_rotation 90 -i base.mp4 -c copy
   rotated.mp4`, attaching a real container-level display matrix (ffprobe's
   `side_data_list`, `rotation: 90`) without re-encoding -- the same structural shape as a
   camera `.mov`'s `tkhd` matrix.
2. Running pigeon's current, unmodified `transcode_video` command shape against it: input
   probed `64x48`, output probed `48x64` -- genuinely swapped, with **zero**
   `side_data_list` on the output (the rotation was "consumed" into the pixel grid).
   Reproduces the bug exactly.
3. Re-running with ffmpeg's `-noautorotate` global input flag added before `-i` (and with
   the real `libx265`/`hvc1`/`mp4` codec/muxer this function actually uses): output probed
   `64x48` -- identical to the input's raw coded size, not merely equal area -- **and** the
   output's `side_data_list` still carried the original `Display Matrix`/`rotation: 90`,
   even though this is a full re-encode, not a stream copy.

This settles the open question definitively: `-noautorotate` is a one-flag fix. It is a
content-agnostic gate ("don't insert a reshaping filter at all during decode"), not a
rotation-degree-specific transform, which is exactly why it also generalizes to the real
incident's rotation-plus-non-unity-scale matrix, not just a clean ±90°/180° case. No
explicit rotation-metadata-propagation code (`-metadata:s:v:0 rotate=N` or a bitstream
filter) is needed -- ffmpeg's own decode→encode→mux pipeline re-attaches the side data on
its own.

## Decision

### 1. `transcode_video` gains `-noautorotate` and explicit stream mapping

`src/commands/job/transform/media.rs`'s `transcode_video` ffmpeg invocation changes from:

```
ffmpeg -y -loglevel error -i <input> -c:v libx265 ...
```

to:

```
ffmpeg -y -loglevel error -noautorotate -i <input> -map 0:v:0 -map 0:a:0? -c:v libx265 ...
```

- `-noautorotate` must precede the `-i` it governs -- it's an input/decoder-level option;
  ffmpeg errors if it's placed after an output-side argument.
- `-map 0:v:0 -map 0:a:0?` (the `?` makes the audio map optional, so a video-only input
  doesn't error) pins exactly the stream `probe_dimensions`'s `-select_streams v:0` already
  reads on the input side -- closing a second, lower-likelihood but previously unverified
  candidate cause (ffmpeg's own default stream-selection disagreeing with
  `probe_dimensions`'s fixed `v:0` pick on a multi-video-track `.mov`). Verified safe
  against a video-only fixture (no error from the optional audio map).

`transcode_to_jpg` and `probe_dimensions` are untouched: HEIC's `irot` is a different,
already-correctly-handled metadata model (ADR-0121), and `probe_dimensions` never decodes a
frame, so it was never exposed to ffmpeg's autorotate step either way.

The existing pixel-area comparison (lines ~272-285) is left exactly as-is. ADR-0121's
area-based rationale was never wrong in principle -- it only misfired here because the
*input to the comparison* was already reshaped before the comparison ever ran. Once
`-noautorotate` removes the reshape, input and output are dimension-identical in the
normal case (a strict subset of "equal area"), so the existing check becomes correct again
with zero additional logic.

*Explicitly deferred, not bundled into this fix*: tightening `transcode_video`'s comparison
from area to an exact `(width, height)` tuple, since video (unlike HEIC) has no legitimate
transpose case left to tolerate once autorotate is disabled. Left for a future PR to keep
this change minimal and easy to review/rollback.

### 2. Mismatch error message gains diagnostic detail

A new `probe_video_diagnostics` helper in `media.rs`, called **only on the error path**
(when the area guard is about to fail), runs one extra `ffprobe` invocation for
`sample_aspect_ratio`, `display_aspect_ratio`, `stream_side_data` (rotation), and stream
tags/count -- exactly the fields `probe_dimensions` deliberately omits -- and appends the
raw JSON to the existing error string. The literal substring
`"refusing an output that isn't full-size"` that `is_non_retryable_transcode_error` matches
on is left untouched. This has no cost on the common (matching) path and gives a future
investigation of this class of failure the detail this incident's own log lacked.

### 3. Tests

- A new fixture helper, `generate_test_video_with_rotation`, generates a base test video
  and remuxes a copy with `ffmpeg -display_rotation <degrees> -i base.mp4 -c copy -f mp4
  <path>` to attach a real container-level display matrix -- mirroring a camera `.mov`'s
  `tkhd` matrix without a checked-in binary fixture. The rotation must be attached via a
  separate remux step on a real container; baking `-display_rotation` into the original
  `lavfi` test-source generation command bakes the rotation into the raw frames instead of
  attaching display-matrix side data, and wouldn't exercise the bug at all.
- A new test, `transcode_video_preserves_raw_dimensions_for_an_input_with_a_rotation_display_matrix`,
  runs `transcode_video` against this fixture and asserts `probe_dimensions(input) ==
  probe_dimensions(output)` exactly (not just equal area) -- pinning the no-reshape
  mechanism.
- Caveat, stated here and inline in the test: this fixture is a *pure* rotation
  (area-invariant), which would **not** have failed even under the pre-fix code --
  ffmpeg's CLI exposes no flag to inject the production incident's actual
  rotation-plus-non-unity-scale matrix. The automated test covers the mechanism, not the
  exact production fingerprint.
- Existing tests (`transcode_video_preserves_dimensions_and_duration_for_a_real_video`,
  the `transcode_video_*_scratch`/`fails_fast` tests, `is_non_retryable_transcode_error_*`)
  are expected to pass unmodified -- `-noautorotate -map` changes nothing for an ordinary,
  non-rotated single-video-track input.

### 4. Manual verification against the real fingerprint

Because the automated test can't reproduce the exact production matrix, this was
additionally verified by hand against a real affected `.mov` sample pulled from the
incident: `ffprobe -show_entries 'stream=width,height:stream_side_data=side_data_type,
rotation'` before and after running the fixed binary confirmed dimensions now match and any
rotation tag present on the source survives onto the transcoded output.

## Out of scope

- `pull_transform::media`'s equally narrow probe/verify -- it has the same latent gap but is
  shielded by its own pre-existing keep-original fallback (ADR-0074, deliberately left
  untouched by ADR-0116). A different blast radius; not touched here.
- Adding a copy-through/keep-original fallback for video in `transform` (ADR-0122 Decision
  §2's explicit no-fallback-for-video choice). The root-cause fix removes the need for one
  in this specific failure mode; revisit separately only if a different, still-unexplained
  video failure mode surfaces later.
- Tightening `transcode_video`'s area comparison to an exact-tuple match -- noted above as a
  valid, low-risk follow-up, deliberately not bundled into this fix.
- `transcode_to_jpg`/HEIC's `irot` path -- untouched, a different and already-correctly-handled
  metadata model.
- Pinning or upgrading the deployed ffmpeg version -- `-noautorotate` predates every ffmpeg
  version in play here; no CI/provisioning change needed.
- A CLI flag to re-enable autorotate -- no stated use case; `-noautorotate` is applied
  unconditionally, consistent with this job's existing "small vetted menu, nothing
  user-exposed and arbitrary" precedent (ADR-0077/0122).

## Consequences

- Real-world `.mov` files whose embedded display matrix encodes a non-trivial
  rotation-plus-scale transform (not just a clean ±90°/180° rotation) now transcode
  successfully instead of hard-failing as a false "non-retryable" corruption signal.
- Playback orientation is unaffected: ffmpeg's decode→encode→mux pipeline still attaches
  the original display matrix to the re-encoded output even with reshaping suppressed,
  confirmed empirically rather than assumed.
- A multi-video-track `.mov`'s stream selection is now pinned explicitly on both the probe
  and encode sides, removing a second, previously-unverified source of input/output
  mismatch.
- A future genuine bad-decode producing a different pixel area is still caught -- the
  guard's logic is unchanged; only the input it operates on is no longer pre-reshaped.

## Verification

- `mise run ci` (fmt-check + lint + test) clean, including the new rotation-fixture test.
- Manual real-sample verification per Decision §4 above.
