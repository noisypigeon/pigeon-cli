# ADR-0153: `m4a`, `mp3`, `aac`, `wav`, and `opus` audio input kinds

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-14.
- **Status**: Accepted.

## Context

ADR-0142 added one audio kind, `mp4-audio`, to reach 480 voice notes saved
with an `.mp4` extension: audio-only input, Opus output, one fixed 48 kbps
bitrate, and an explicit deferral --

> An `--audio-quality` menu mirroring ADR-0122's `--video-quality`. One
> fixed, generous bitrate is enough: these files are small, so the
> quality/size tradeoff that justified a per-run video menu barely registers
> here. A menu can be added if a real need surfaces.

This ADR adds `m4a`, `mp3`, `aac`, `wav`, and `opus`, all outputting
`.opus`, taking the audio family to six kinds. It is the need ADR-0142
named. These are not voice notes: `.m4a`/`.mp3`/`.aac` are a music library
and `.wav` is uncompressed, so 48 kbps would be a severe, irreversible loss
in an archival pipeline whose source may not survive to redo it from.

Two things found while grounding this shaped it more than the new variants
did, and both were measured rather than assumed.

### Cover art would have rejected almost every music file

ffmpeg models embedded cover art as a **video stream**, and
`has_video_stream` counted any stream matching `-select_streams v`.
Confirmed against a real fixture -- an `.m4a` with an attached picture:

```text
ffprobe -select_streams v -show_entries stream=index -of csv=p=0 tagged.m4a  ->  1
ffprobe -select_streams v -show_entries stream=index -of csv=p=0 tone.m4a    ->  (empty)
```

So a tagged `.mp3`, `.m4a`, or `.aac` -- i.e. the overwhelming majority of a
real library -- would have hit ADR-0142's *"has a video stream, so it is not
an audio-only recording"* guard, been classified non-retryable **and**
wrong-input-kind (so ADR-0143 deletes its local copy), and after 18 of the
first 20 completions ADR-0121's circuit breaker would have aborted the run.
The feature would have failed on contact with real data.

### The truncated-encode guard fails every real-length `.aac`

`transcode_audio` inherits ADR-0142's guard: fail when input and output
duration differ by more than `DURATION_TOLERANCE_SECONDS` (0.5 s, absolute,
symmetric). Every other audio kind carries a reliable container duration. A
bare `.aac` is an ADTS *stream* with none, so ffprobe derives one from
bitrate and size -- and the error **scales with length**. Measured on
generated fixtures:

| source | reported in | out | delta | old guard |
|---|---|---|---|---|
| 3 s | 3.000 s | 3.048 s | 0.048 s | passes |
| 30 s | 29.627 s | 30.030 s | 0.403 s | passes, barely |
| 180 s | 177.868 s | 180.031 s | **2.162 s** | **fails** |

A ~1.2% under-estimate, so every track of real length would have failed.
The plan for this ADR was to find out rather than widen a guard on
speculation; this is what finding out returned, so §2 fixes it.

Note the failure would have been quiet-but-wasteful rather than loud: the
guard's message is **not** in `NON_RETRYABLE_SIGNATURES` -- it is close to,
but does not match, the *"refusing an output that isn't full-size"* entry --
so each file would have been retried twice for a perfectly deterministic
outcome, which is ADR-0141's exact pathology.

## Decision

### 1. `has_video_stream` ignores attached pictures

The probe gains a disposition entry and filters on it:

```text
ffprobe -v error -select_streams v \
  -show_entries 'stream=index:stream_disposition=attached_pic' -of json
```

A stream with `attached_pic=1` is cover art, not video. Parsed with `serde`,
matching `probe_dimensions`'s existing JSON precedent in the same module
rather than adding a second parsing style.

The disposition field **defaults to `0`** when absent, which is the
conservative direction on purpose: an unexpected probe shape reads as real
video and so *preserves* ADR-0141's and ADR-0142's guards rather than
silently disarming them.

This is a prerequisite, not a refinement. Without it `m4a`, `mp3`, and `aac`
are unusable.

### 2. The truncated-encode guard becomes one-sided and scales

Two corrections to the same root error -- treating an estimated duration as
exact, and treating the comparison as symmetric -- extracted into a pure
`is_truncated_encode(before, after)`:

- **One-sided.** A partial write always produces a *shorter* output. An
  output *longer* than the input is never evidence of truncation, and for an
  estimated input duration it is the normal case -- every measurement above
  exceeded its reported input. The old symmetric comparison treated that as
  a failure.
- **Scaled.** A new `DURATION_TOLERANCE_FRACTION` of 2% is taken as the
  allowance when it exceeds the 0.5 s floor. 2% covers the measured ~1.2%
  with headroom while still catching a partial write, which is a far larger
  fraction of a stream.

Pure so the thresholds are testable without running `ffmpeg`, the same
reason ADR-0143 extracted `circuit_breaker_should_trip`.

**This changes behaviour for video kinds too**, since the guard is shared.
Stated rather than hidden: a video whose output is *longer* than its
container duration now passes where it previously failed, which may absorb
some of the three multi-video-stream files
[#77](https://github.com/noisypigeon/pigeon-cli/issues/77) tracks. A
genuinely truncated encode still fails at every scale, which is what the
guard is for.

**Deliberately not done**: adding the guard's message to
`NON_RETRYABLE_SIGNATURES`. It would stop the wasted retries, and the
failure *is* deterministic -- but it would also make such failures
circuit-breaker-eligible, aborting runs that today merely report a few
files. That is a separate decision, now that §2 means the guard should
rarely fire at all.

### 3. Five new `InputFileType` variants, and an `is_audio()`

`M4a`, `Mp3`, `Aac`, `Wav`, `Opus`. One extension each -- no speculative
`adts`/`wave`/`oga`/`m4b` aliases, per ADR-0139's add-on-evidence rule --
`output_extension()` → `"opus"`, `is_video()`/`is_spreadsheet()` false,
`name()` falling through to `canonical_extension()`.

A new `is_audio()` covers all six audio kinds, mirroring
`is_video()`/`is_spreadsheet()`: it gates §5's prompt and the converter
dispatch by asking what a kind *is*, rather than making each call site widen
a `match` arm list -- the shape that let ADR-0141's audio-only dispatch bug
hide in one.

Prefix inference needs no new code: `--input-file-type wav` resolves
`source:wav/` → `destination:opus/`.

### 4. `AudioQuality`, with a default that varies by kind

A small vetted menu beside `VideoQuality`, same shape so there is one
convention for a quality menu rather than two:

| Tier | Bitrate |
|---|---|
| `low` | 32k -- speech, the conventional Opus voice rate |
| `medium` | 48k -- ADR-0142's choice, archival voice of unknown type |
| `high` | 96k -- good general audio |
| `transparent` | 128k -- music; the widely-cited Opus stereo threshold |

Deliberately **not** `lossless`, unlike `VideoQuality`'s tier of that name:
Opus has no lossless mode, so the name would be a lie. `transparent` claims
only what it delivers. `OPUS_BITRATE` is retired and its reasoning moves
onto `AudioQuality::Medium`, where it still applies, rather than being
deleted.

`InputFileType::default_audio_quality()` is the load-bearing part, more than
the menu is -- a menu defaulting to 48k everywhere would still quietly
destroy a music library:

- `Mp4Audio` → `Medium`, so **ADR-0142's output is byte-identical when the
  flag is omitted**. Pinned by a test.
- `M4a`, `Mp3`, `Aac`, `Opus` → `Transparent`. Lossy sources, so the encode
  is a *second* generation and the top tier limits the compounding.
- `Wav` → `Transparent`, and this is the strongest case of the five: WAV is
  uncompressed, so this is the *first* lossy generation the file has ever
  had, with nothing already discarded to hide behind. A lower tier would
  make this job the sole cause of whatever is lost.

### 5. `--audio-quality`, gated on `is_audio()`

An `AudioQualityInput` mirroring `VideoQualityInput`, prompted only for an
audio kind and defaulting per §4 rather than to a single constant — its
interactive `Select` highlights that kind's own default, and its
non-interactive fallback returns it. An `Audio quality:` echo line joins the
existing `Video quality:` one.

### 6. `.opus` is re-encoded, with the sharp edge stated

`Opus` joins the `transcode_audio` arm rather than `copy_through`, so a mixed
library normalizes to one bitrate. This is a deliberate departure from
ADR-0112's jpeg and ADR-0149's csv copy-through, and it carries two costs
recorded here rather than discovered later:

- A **second lossy generation** on a file already in the target codec -- the
  exact thing copy-through exists to avoid.
- Re-encoding a 48k source at the `transparent` default **cannot recover
  quality and only inflates size** (~2.6×). An operator normalizing a
  library that came from `mp4-audio` runs should pass
  `--audio-quality medium`.

`outcome_for` therefore reports `Transcoded` for `Opus`, not
`CopiedThrough`; `Jpeg` and `Csv` remain the only copy-through kinds.

**Explicitly rejected**: silently capping the target at the source's own
measured bitrate. It would prevent that inflation, but "I asked for 128k and
got 48k" is a worse surprise than a documented sharp edge, and the menu
exists precisely so the operator chooses.

## Consequences

- Five new input kinds, all reaching `.opus`, with no new external
  dependency -- `ffmpeg`'s `libopus` was already required by ADR-0142.
- **A tagged music file now transcodes instead of being refused**, and a
  genuine video is still refused by the audio path. Both pinned by tests
  that construct a real attached-picture fixture.
- **Album art is dropped from the output.** `-map 0:a:0` selects only audio,
  so a music archive loses its artwork. Textual tags are unaffected --
  ffmpeg copies global metadata by default, so title/artist/album survive.
  Named prominently because it is the one silent loss in this change.
- The duration guard changes for **every** kind, including video (§2).
- WAV is by far the largest per-file input this job has taken (~10 MB/minute
  at 44.1 kHz stereo 16-bit), but the *output* is smaller, so the disk
  high-water mark is the pulled source -- which ADR-0124's GC already bounds
  to the in-flight window, and ADR-0130's per-file `check_disk_space`
  already guards. Measured: a 30 s WAV fixture went 2,646,078 → 551,434
  bytes.
- `opus` was an output extension only until now. A test that asserted
  `parse("opus")` *fails* is reversed, with a comment saying so.

## Out of scope

- **Preserving embedded cover art in the output.** Opus-in-Ogg can carry
  `METADATA_BLOCK_PICTURE`, but ffmpeg's support for writing it is uneven,
  so this is its own decision rather than a line in this one.
- **Reclassifying the duration-guard message as non-retryable** (§2).
- **Sample-rate and bit-depth preservation for `.wav`.** Opus always
  operates at 48 kHz internally, so a 96 kHz or 24-bit master is resampled
  and requantized by construction -- inherent to the output format, not a
  tunable. Named because a WAV archive is the one input class where an
  operator might reasonably have expected otherwise.
- **Multichannel audio.** A 5.1 source at 128 kbps is under-provisioned,
  since the tiers are chosen for stereo. Not guarded -- no such file is
  known to be in scope -- but recorded so a future surprise has a reference.
- `flac`, `aiff`, `ogg`, `wma` kinds -- added on evidence of a real need.
- Capping the target bitrate at the source's (§6, rejected with reasoning).
- Lossless archival (FLAC output) -- this job has one output format per
  input kind, and Opus is the chosen one.

## Amends

ADR-0122 (a second quality menu, copying `VideoQuality`'s shape, and whose
`Lossless` tier deliberately has no audio counterpart), ADR-0141 (its
`has_video_stream` now distinguishes cover art from video), and ADR-0142
(its `--audio-quality` deferral is taken up, its `OPUS_BITRATE` const
retired into `AudioQuality::Medium` with its reasoning intact, its 48k
default preserved for the one kind it was chosen for, and its
truncated-encode guard corrected). Each gets a short forward-pointing
"Amendment".

## Verification

- `mise run ci` clean (fmt-check + lint + test). 444 tests, from 431.
- `media.rs`, the load-bearing pair, both confirmed to actually run rather
  than skip: cover art does **not** read as a video stream while a real
  video does, and a tagged `.m4a` transcodes successfully rather than being
  refused. Fixture built by muxing a generated PNG in as an attached
  picture, so it runs wherever `ffmpeg` does.
- `media.rs`: `is_truncated_encode` accepts every measured ADTS case
  (177.868 → 180.031 among them), still catches a partial write at 3 s, 10 s
  and 180 s, honours the 0.5 s floor on short inputs, and the 2% allowance
  on long ones.
- `format.rs`: the five kinds parse case-insensitively, reject eleven
  neighbouring spellings, have one extension each, output `opus`, are
  `is_audio()` and neither video nor spreadsheet; `AudioQuality` parses
  every tier, rejects `lossless` specifically, and round-trips through
  `as_str`; **`default_audio_quality()` is `Medium` for `Mp4Audio`** and
  `Transparent` for the five new kinds.
- `worker.rs`: a `wav` run end-to-end transcodes and pushes a `.opus`,
  reports `Transcoded`, and leaves `source/` empty; `outcome_for` treats
  `Opus` as transcoded while `Jpeg`/`Csv` stay copy-through.
- `tests/cli.rs`: `transform --help` lists all five kinds, `--audio-quality`
  and its `transparent` tier; an invalid `--audio-quality` fails fast and
  the error names the valid tiers.
- Manual: a real run per kind against a prefix of the actual library,
  confirming `.opus` objects at the destination, that a tagged file with
  album art succeeds, and spot-checking one output's bitrate and that its
  title/artist tags survived.
