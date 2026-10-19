# CLI-0142: an `mp4-audio` input type, transcoding audio-only recordings to Opus

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-12.
- **Status**: Accepted.

## Context

CLI-0141 found that 480 files in a real `job run transform
--input-file-type mp4` run were audio-only recordings -- voice notes saved
with an `.mp4` extension, confirmed via `ffprobe` as having no video stream
at all -- and made `transcode_video` reject them fast, with an accurate
message, instead of retrying ffmpeg's cryptic `-map 0:v:0` failure twice
apiece. That ADR deliberately stopped there: the diagnosis improved, but
the content is still never transcoded and never pushed. Those 480 files
remain unreachable by any `pigeon` job.

They are worth keeping. The gap is that `transform`'s only two output
shapes are "full-size maximum-quality `.jpg`" (CLI-0112) and "H.265/HEVC
`.mp4`" (CLI-0122), and audio-only input fits neither.

A further, stated goal shapes the format choice: transcription and
downstream analysis of this audio is intended future work. The
implementation of that is firmly out of scope here, but it means the stored
format should not be one that forecloses it.

## Decision

### 1. A new `InputFileType::Mp4Audio` (`--input-file-type mp4-audio`)

A new variant alongside CLI-0122's video kinds, selected per run exactly
like every other input kind, with:

- `extensions()` → `&["mp4"]` -- the same on-disk spelling as `Mp4`. This
  is deliberately not a new source filter: there is no distinct file
  extension that identifies an audio-only `.mp4`, so this is an
  operator-chosen run over a subset they have reason to believe is
  audio-only. §3's guard is what makes that belief safe to act on.
- `canonical_extension()` → `"mp4"`, inherited from `extensions()[0]` like
  every other single-spelling kind (CLI-0139).
- `output_extension()` → `"opus"`.
- `is_video()` → `false`. This is correct on its own terms (there is no
  video) and has the right side effects for free: `--video-quality` is
  never prompted (`dispatch_async` gates that on `is_video()`), and the
  copy-through path stays `Jpeg`-only.

`output_extension()`'s `if self.is_video() { "mp4" } else { "jpg" }` stops
being a two-way split and becomes a real `match`, so a future kind cannot
silently inherit `jpg` by falling through an `else`.

### 2. Opus in an Ogg container, at 48 kbps

`media::transcode_audio` runs:

```text
ffmpeg -y -loglevel error -i <input> -map 0:a:0 \
  -c:a libopus -b:a 48k -application audio -f opus <output>
```

**Why Opus.** It is the best quality-per-bit codec available for this
content class at low-to-moderate bitrates, and `libopus` is present in
every mainstream modern `ffmpeg` build (verified on the deployed one).
Crucially for the stated transcription goal, it costs nothing there: every
transcription pipeline decodes to PCM first, so the source codec is
irrelevant to it beyond whether the audio survived intact.

**Why `.opus`, not `.ogg`.** Both ffmpeg's `opus` and `ogg` muxers produce
byte-identical output for this command (verified), but `.opus` is the
canonical extension for Ogg-encapsulated Opus, whereas `.ogg` is ambiguous
(it is also the Vorbis extension). `-f opus` forces the muxer explicitly,
the same CLI-0115 lesson `transcode_to_jpg`/`transcode_video` already
encode: `worker.rs` writes to a scratch path ending in `.scratch`, so
ffmpeg must never be allowed to infer the container from the on-disk
extension.

**Why 48 kbps, not 32.** 32k is the speech-tuned figure and would be ~3x
smaller than a typical 128k AAC source; 48k is ~2.2x smaller. The content
here is personal archival audio of *unknown* type -- some of these clips
may be music rather than speech -- and 48k is the lowest rate that stays
comfortably transparent for speech while remaining acceptable for general
audio. This mirrors CLI-0122's own reasoning for `VideoQuality::Medium`'s
CRF 24, deliberately a notch better than the conventional default because
these are archives, not streams.

**Why `-application audio`, not `voip`.** `voip` applies a more aggressive
speech bandpass tuned for telephony, discarding content a future
transcription model might still use. `audio` is libopus's general-purpose,
higher-fidelity mode, and is already its default -- stated explicitly so a
future reader does not "optimize" it to `voip` on the assumption that
these are all voice notes.

`-map 0:a:0` pins the first audio stream, exactly as `transcode_video`
pins `0:v:0`, rather than relying on ffmpeg's default stream selection.

### 3. An input that still has a video stream is refused, not silently stripped

`transcode_audio` calls CLI-0141's `has_video_stream` and hard-fails when
it returns `true`, with a non-retryable message.

This is the load-bearing safety decision, not a formality. Nothing filters
a `mp4-audio` run's source down to audio-only files -- §1 reuses the `mp4`
extension precisely because no extension distinguishes them -- and
CLI-0138/0139's prefix inference means an operator who passes
`--source source` with no explicit prefix resolves to `source:mp4/`: *the
same default address a plain `--input-file-type mp4` video run uses*.
Without this guard, that single omitted prefix would quietly transcode a
bucket of real video into audio-only files and push them to the
destination. With it, the same mistake is a loud per-file failure that
names the cause, and no video is ever destroyed.

The new signature joins `NON_RETRYABLE_SIGNATURES` for the same reason
CLI-0141's did: it is perfectly deterministic, so retrying buys nothing.

### 4. Guards: duration, but not dimensions

Reuse `probe_duration_seconds` and `DURATION_TOLERANCE_SECONDS` for the
same truncated/partial-encode guard `transcode_video` has -- audio shares
exactly that failure mode (a process exiting 0 having written part of the
stream). Verified empirically that an ordinary transcode lands well inside
the existing 0.5s tolerance (a 3.000s source produced a 3.0065s output).

The pixel-area dimension guard is skipped: there are no dimensions.

## Out of scope

- **Transcription and any downstream analysis of the resulting audio.**
  This ADR chooses a storage format with that future work in mind and
  stops there.
- **An `--audio-quality` menu** mirroring CLI-0122's `--video-quality`.
  One fixed, generous bitrate is enough: these files are small, so the
  quality/size tradeoff that justified a per-run video menu barely
  registers here. A menu can be added if a real need surfaces.
- **Auto-detecting audio-only files during a `mp4` video run** and routing
  them to this path automatically. Deliberately rejected: a run transcodes
  exactly one input kind (CLI-0122 §51, as narrowed by CLI-0139 to
  one-input-*kind*-per-run), and the two kinds produce different output
  extensions, so a mixed run would silently write two formats into one
  destination prefix.
- **Extracting the audio track from a file that has both.** §3 refuses
  that input outright rather than making this job a de-muxer.
- Any new source-side extension filter for audio-only files -- no
  extension distinguishes them (see §1).

## Consequences

- The 480 audio-only files CLI-0141 can only diagnose become processable:
  `--input-file-type mp4-audio` transcodes and pushes them.
- **`transform` gains a third output extension.** A destination prefix
  receiving a `mp4-audio` run holds `.opus` files, so an operator pointing
  this at the same `destination:mp4/` prefix a video run uses would mix
  formats in one directory. Nothing prevents it (prefixes are
  operator-chosen, CLI-0138); the sensible convention is a separate
  prefix, and `--destination destination` with no prefix already resolves
  to `destination:opus/` by inference, which is the correct default.
- `pigeon` now depends on `libopus` being compiled into the deployed
  `ffmpeg`. Like the existing `libx265` dependency, a build without it
  fails with ffmpeg's own `"Unknown encoder"` message, which is already in
  `NON_RETRYABLE_SIGNATURES` and already trips CLI-0121's circuit breaker
  into aborting the run with a message naming an environment-capability
  gap -- the correct behavior, requiring no new code.
- `InputFileType::output_extension()` becomes an exhaustive `match`, so
  adding a future kind without choosing its output extension no longer
  compiles.
- An audio-only input is now reachable by two kinds with opposite
  outcomes: `mp4` fails it (CLI-0141), `mp4-audio` transcodes it. That is
  the intended distinction, and CLI-0141's own failure message points at
  this ADR by name.

## Amends

CLI-0122 (adds a non-video, non-image input kind to the `InputFileType`
enum it extended to video, and a third output extension alongside its
`.mp4` and CLI-0112's `.jpg`) and CLI-0141 (whose failure message named
this ADR as the audio-preserving path before it existed; that path now
exists). Each gets a short forward-pointing "Amendment" section, per this
repo's convention for amending a merged ADR without rewriting it.

## Verification

- `mise run ci` clean (fmt-check + lint + test).
- `format.rs`: `Mp4Audio` parses from `mp4-audio` case-insensitively,
  rejects neighbouring spellings, has exactly one extension (`mp4`),
  `canonical_extension()` is `mp4`, `output_extension()` is `opus`, and
  `is_video()` is `false`.
- `media.rs`: `transcode_audio` produces a playable `.opus` from a
  synthetic `sine`-based audio-only fixture whose duration is preserved
  within tolerance; refuses an input that has a video stream, with a
  message `is_non_retryable_transcode_error` classifies as non-retryable;
  fails on an unreadable input; succeeds when the output path ends in
  `.scratch` (the CLI-0115 muxer-inference regression, per output format).
- `worker.rs`: `run_transform_job` end-to-end over an audio-only source
  pushes a `.opus` result and reports `Outcome::Transcoded`.
- `tests/cli.rs`: `transform --help` lists `mp4-audio` among the input
  file types.
- Manual: a real `job run transform --input-file-type mp4-audio` run
  against a prefix holding known audio-only clips, confirming a non-zero
  transcoded count and `.opus` objects at the destination; plus the
  safety case -- the same command pointed at a prefix of real video fails
  every file with the "has a video stream" message and pushes nothing.

## Amendment (2026-10-12): the guard's blast radius is bounded (CLI-0143)

§3's has-a-video-stream guard is correct -- no video is ever converted to
audio-only, and nothing wrong is pushed -- but CLI-0143 found it was
correct without being *bounded*. Tracing what happens to each rejected file
showed that a misdirected run degrades messily rather than failing cleanly:
every rejected file's local source copy was retained (CLI-0124's
retain-on-failure rule), and the circuit breaker that should have aborted
the run required unanimity across its first 20 completions, so a single
early success among a mixed source disarmed it permanently. A
`mp4-audio` run over the real 7289-file bucket would then fail ~6809 files
while retaining all of them, ~80 GiB.

CLI-0143 fixes both halves: a wrong-input-kind failure now deletes its
local source copy (non-destructive -- the file is still at `--source`), and
the breaker trips on 18 of 20 rather than 20 of 20. The guard itself, its
message, and its non-retryable classification are unchanged.

## Amendment (2026-10-14): the deferred quality menu, and four more audio kinds (CLI-0153)

This ADR's Out of scope deferred an `--audio-quality` menu "if a real need
surfaces". It surfaced: CLI-0153 adds `m4a`, `mp3`, `aac`, `wav`, and
`opus`, which are a music library and (for `wav`) uncompressed, not the
voice notes this ADR's 48 kbps was chosen for.

Four changes to what this ADR established:

- **`OPUS_BITRATE` is retired** into `AudioQuality::Medium`, carrying its
  doc comment and its reasoning intact -- that reasoning is still correct
  for the kind it was written about.
- **`mp4-audio`'s default is still `medium` (48k)**, so this ADR's output is
  byte-identical when the flag is omitted. Pinned by a test. The new kinds
  default to `transparent` (128k) instead; the per-kind default is the
  load-bearing part, since a menu defaulting to 48k everywhere would still
  quietly destroy a music library.
- **The has-a-video-stream guard no longer fires on cover art** (see
  CLI-0141's amendment). Without that fix this ADR's guard would have
  rejected essentially every tagged music file.
- **The truncated-encode guard is corrected.** This ADR's symmetric 0.5 s
  comparison fails every real-length `.aac`: ADTS carries no container
  duration, ffprobe estimates it ~1.2% short, and a 180 s source measured
  177.868 s in against 180.031 s out. The guard becomes one-sided -- only a
  *shorter* output means truncation -- and its allowance scales with
  duration.

`transcode_audio`'s `-map 0:a:0`, its Opus-in-Ogg output, `-application
audio`, and the `.opus` extension reasoning are all unchanged. One
asymmetry this ADR noted does get resolved: `opus` was an input value
`parse` rejected, and is now a real input kind -- re-encoded rather than
copied through, so a mixed library normalizes to one bitrate.
