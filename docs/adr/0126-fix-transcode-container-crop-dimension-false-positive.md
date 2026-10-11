# ADR-0126: fix false-positive transcode failures from ffmpeg's implicit container-crop application

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-11.
- **Status**: Accepted.

## Context

ADR-0125 (merged to main) fixed one false-positive mechanism in `job run transform`'s
video transcode dimension guard: ffmpeg's implicit "autorotate" reshape. A fresh
`pigeon.jsonl` log, captured **after** that fix was deployed (confirmed because the
failure messages now include ADR-0125's own `(diagnostics: ...)` suffix), shows the same
class of failure still happening -- via a second, independent mechanism:

```
transcoding <path>.mov changed pixel area from (1920, 1440) (2764800 px) to
(1744, 1308) (2281152 px); refusing an output that isn't full-size (diagnostics: {
  ... "side_data_list": [{"side_data_type": "Frame Cropping"},
                          {"side_data_type": "Display Matrix", "rotation": -90}], ...})
```

Key evidence this is new, not a regression in ADR-0125's fix: `(1744, 1308)` is **not**
transposed relative to `(1920, 1440)` -- it's uniformly shrunk by exactly 109/120 on both
axes (the same ratio as the original incident, but this time with no swap), proving
`-noautorotate` *is* successfully suppressing the rotation reshape now. A second sample in
the same log has `"Frame Cropping"` with **no** `"Display Matrix"` at all, and the same
area mismatch still occurs -- proving Frame Cropping alone, independent of rotation, is
sufficient to trigger this. ADR-0125 was correctly scoped to the rotation/display-matrix
mechanism only; this is a second, independent root cause surfacing through the same guard.

FFmpeg 7.1 added support for QuickTime's Clean Aperture (`clap` atom) *container*-level
crop metadata, surfaced as a new side-data type `AV_PKT_DATA_FRAME_CROPPING` (ffprobe's
`side_data_type` string: literally `"Frame Cropping"`), carrying `crop_top`/`crop_bottom`/
`crop_left`/`crop_right`. The same release added a new, input-side
`-apply_cropping[:<stream_spec>]` CLI option (values `none`/`all` [default]/`codec`/
`container`) -- distinct from an older, unrelated decoder-level `apply_cropping` AVOption
that only ever governs the H.264/HEVC SPS "conformance window" crop, which `ffprobe`'s bare
`codecpar.width/height` has **always** already reflected, pre-decode. At ffmpeg's default
(`all`), decode silently auto-applies the *new* container-level crop too, shrinking the
decoded frame below what `probe_dimensions` reports for the input -- structurally the same
"comparing two different definitions of dimensions" bug ADR-0125 found for rotation, just
via a different ffmpeg feature that didn't exist before ffmpeg 7.1.

### Verifying the fix mechanism

Empirically confirmed against the locally-installed ffmpeg 9.0.2:

- `-apply_cropping` is an input-side option and must precede `-i`, same placement rule as
  `-noautorotate`.
- `-noapply_cropping` does not exist -- unlike `-noautorotate`, there is no bare-boolean
  negation form; the enum value must be spelled out.
- `-noautorotate -apply_cropping:v none -i ... -map 0:v:0 ...` runs cleanly -- no ordering
  conflict combining both options.
- **Blanket `-apply_cropping none` is unsafe**: against a synthetic SPS-cropped
  (codec-level) fixture built with the `h264_metadata` bitstream filter (128x96 coded,
  124x96 after SPS crop), the default/`codec` value correctly produces 124x96 (matching
  `probe_dimensions`'s baseline), but `none` produces 128x96 -- a brand-new false positive
  against virtually any ordinary non-16-pixel-aligned H.264/HEVC video, trading today's
  container-crop bug for a far more common codec-crop regression.
- **`-apply_cropping:v codec` is the surgical value**: it continues applying exactly the
  codec/SPS-level crop `probe_dimensions`'s baseline already assumes, while suppressing only
  the new container-level (`clap`-atom) crop `probe_dimensions` never accounts for.

This settles the open question: `-apply_cropping:v codec` is a one-flag fix, content-agnostic
in the same way `-noautorotate` is ("don't let ffmpeg auto-apply a crop mechanism our own
dimension check doesn't know about"), not a crop-value-specific transform.

## Decision

### 1. `transcode_video` gains `-apply_cropping:v codec`

`src/commands/job/transform/media.rs`'s `transcode_video` ffmpeg invocation changes from:

```
ffmpeg -y -loglevel error -noautorotate -i <input> -map 0:v:0 -map 0:a:0? -c:v libx265 ...
```

to:

```
ffmpeg -y -loglevel error -noautorotate -apply_cropping:v codec -i <input> \
  -map 0:v:0 -map 0:a:0? -c:v libx265 ...
```

- `-apply_cropping:v codec` must precede `-i`, same input-side rule as `-noautorotate`; the
  two combine with no ordering conflict (verified empirically).
- **`codec`, not `none`**: `none` would also suppress the long-standing SPS
  conformance-window crop `probe_dimensions` has always already reflected, reintroducing a
  *more common* false positive on ordinary non-16-pixel-aligned video. The doc comment calls
  this out explicitly so a future reader doesn't "simplify" it to `none`.

The existing pixel-area comparison and `probe_dimensions` are left exactly as-is -- same
rationale as ADR-0125 Decision §1: once the implicit auto-crop is suppressed, input and
output are dimension-identical again in the normal case, so the existing check is correct
with zero additional logic.

### 2. Mismatch error message gains the numeric crop extents

`probe_video_diagnostics`'s existing `-show_entries` query:
```
stream_side_data=side_data_type,rotation
```
is extended to:
```
stream_side_data=side_data_type,rotation,crop_top,crop_bottom,crop_left,crop_right
```
-- confirmed these are ffprobe's own field names for `AV_PKT_DATA_FRAME_CROPPING` (from
FFmpeg's `fftools/ffprobe.c` print handler). Appended to the existing `stream_side_data=`
section, not a new top-level key. Same error-path-only cost posture as before.

### 3. Tests

- A new fixture helper (mirroring ADR-0125's `generate_test_video_with_rotation` pattern --
  remux-based side-data injection on a real container, not baked into raw `lavfi` frames)
  attaches a codec-level (SPS conformance-window) crop via the `h264_metadata` bitstream
  filter -- the same technique used above to empirically validate the "codec vs none"
  distinction. A new test runs `transcode_video` against it and asserts success plus
  `probe_dimensions(output) == probe_dimensions(input)` (both already reflecting the SPS
  crop). This is the regression guard: if a future reader changes `-apply_cropping:v codec`
  to `none`, this test starts failing because the output would revert to the pre-crop coded
  size.
- Caveat, stated here and inline: ffmpeg's CLI has no `-display_rotation`-equivalent for
  injecting container-level (`clap`-atom) Clean Aperture metadata, so the exact production
  mechanism can't be synthesized in an automated fixture -- identical gap to ADR-0125's own
  rotation-plus-scale limitation, handled the same way (manual verification against a real
  sample, stated as a caveat, not claimed as full coverage).
- Existing tests (including ADR-0125's own rotation regression test) are expected to pass
  unmodified -- `-apply_cropping:v codec` is ffmpeg's own pre-7.1 default behavior for any
  input without container-crop metadata.

### 4. Manual verification against the real fingerprint

Verified by hand against a real affected `.mov` sample from the incident:
`ffprobe -show_entries 'stream=width,height:stream_side_data=side_data_type,crop_top,
crop_bottom,crop_left,crop_right'` before and after running the fixed binary confirmed
dimensions now match.

## Out of scope

- **`-apply_cropping none`** -- considered and rejected: suppresses the unrelated,
  long-standing SPS/codec-level crop too, trading today's bug for a more common false
  positive on ordinary non-16-pixel-aligned video (evidence above).
- **`pull_transform::media`** -- same asymmetric-blast-radius argument as ADR-0125: it has
  the identical narrow probe but is shielded by its own pre-existing keep-original fallback
  (ADR-0074, left untouched by ADR-0116), degrading gracefully where `transform` hard-fails.
  Not touched here, consistent with ADR-0125's own scoping.
- **Teaching `probe_dimensions` to compute expected post-crop dimensions** from the four
  numeric crop fields, as an alternative to suppressing ffmpeg's auto-crop -- considered,
  rejected in favor of the suppression approach: it's simpler, symmetric (both
  `probe_dimensions` calls on input and output stay the same decode-free bare-`codecpar`
  read), adds no new parsing/struct code, carries no risk from a future ffmpeg version
  changing crop-field units/semantics underneath a hand-rolled subtraction, and keeps this
  fix architecturally identical in shape to ADR-0125 -- valuable precedent for whoever hits
  the *next* ffmpeg-side implicit-reshape mechanism through this same guard.
- Tightening `transcode_video`'s area comparison to an exact-tuple match -- already noted as
  a deferred follow-up in ADR-0125, unaffected by this ADR.
- Pinning or upgrading the deployed ffmpeg version -- `-apply_cropping` predates every
  ffmpeg version in play here (introduced 7.1; installed is 9.0.2).
- A CLI flag to re-enable the container-crop auto-apply -- no stated use case, consistent
  with ADR-0125's "small vetted menu, nothing user-exposed and arbitrary" precedent.

## Consequences

- Real-world `.mov` files carrying QuickTime Clean Aperture (`clap`-atom) container-level
  crop metadata now transcode successfully instead of hard-failing as a false
  "non-retryable" corruption signal.
- The long-standing SPS/codec-level crop behavior `probe_dimensions` has always assumed is
  unchanged -- `codec` continues applying it exactly as ffmpeg's old (pre-7.1) default did.
- A future genuine bad-decode producing a different pixel area is still caught -- the
  guard's logic is unchanged; only the input it operates on is no longer pre-shrunk by an
  auto-applied container crop.
- A future, third independent ffmpeg implicit-reshape mechanism would be handled the same
  way again: find the suppression flag, verify it doesn't regress an already-relied-upon
  default, add it alongside the other two, extend diagnostics if new fields are relevant.

## Verification

- `mise run ci` (fmt-check + lint + test) clean, including the new codec-crop regression
  test.
- Manual real-sample verification per Decision §4 above.
