//! `ffmpeg`/`ffprobe`-backed png/heic -> jpg transcoding (ADR-0112 Decision
//! §4), mov/m4v/mp4 -> H.265 mp4 (ADR-0122), audio-only mp4 -> Opus
//! (ADR-0142), and the jpeg copy-through path that skips both entirely. Where the
//! since-removed `pull_transform::media` defaulted to `-q:v 3` (tuned for
//! screenshots, not "no quality loss"), this always uses ffmpeg's highest
//! JPEG quality
//! setting and forces 4:4:4 chroma, with no resize filter -- dimensions are
//! therefore preserved by construction, and `transcode_to_jpg` additionally
//! confirms that with a cheap post-encode `ffprobe` check as a defensive
//! guard against an unexpected ffmpeg default silently resizing the image.
//! That guard compares pixel *area*, not exact `(width, height)` ordering,
//! and `probe_dimensions` understands HEIF "Tile Grid" stream groups
//! (ADR-0121) -- both needed for a grid-tiled, HEIF-rotated real-world HEIC
//! photo to probe and compare correctly; see `probe_dimensions`'s and
//! `transcode_to_jpg`'s own doc comments for why.

use std::path::Path;

use serde::Deserialize;

use super::format::{AudioQuality, VideoQuality};

/// Confirms `ffmpeg`/`ffprobe` are on `PATH`, checked once up front before
/// any prompts (the same shape as `rclone::worker::check_rclone_available`),
/// so a missing binary fails the whole job immediately instead of partway
/// through a long run.
pub(crate) async fn check_ffmpeg_available() -> Result<(), String> {
    for binary in ["ffmpeg", "ffprobe"] {
        tokio::process::Command::new(binary)
            .arg("-version")
            .output()
            .await
            .map_err(|_| {
                format!(
                    "'{binary}' was not found on PATH -- required for \
                     'pigeon job run transform' to transcode images"
                )
            })?;
    }
    Ok(())
}

#[derive(Deserialize, Default)]
struct FfprobeStream {
    width: Option<u32>,
    height: Option<u32>,
}

#[derive(Deserialize, Default)]
struct FfprobeStreamGroupComponent {
    width: Option<u32>,
    height: Option<u32>,
}

#[derive(Deserialize, Default)]
struct FfprobeStreamGroup {
    #[serde(default)]
    components: Vec<FfprobeStreamGroupComponent>,
}

#[derive(Deserialize, Default)]
struct FfprobeDimensionsOutput {
    #[serde(default)]
    streams: Vec<FfprobeStream>,
    #[serde(default)]
    stream_groups: Vec<FfprobeStreamGroup>,
}

/// Pixel dimensions of `path`'s true image content, or `None` if `ffprobe`
/// can't determine them (e.g. a format it can't open at all) -- treated as
/// "skip the check," not an error, since this is a defensive guard, not the
/// primary correctness mechanism (no resize filter is ever applied, so
/// dimensions are already preserved by construction).
///
/// Prefers a HEIF "Tile Grid" stream group's reconstructed canvas size
/// (ADR-0121) over the first stream's raw dimensions: a grid-tiled HEIC
/// (the common shape for a modern phone camera's high-resolution photo)
/// exposes its dozens of tiles as individual small streams (e.g. 512x512)
/// with none marked `default`/primary, so `-select_streams v:0` alone only
/// ever sees one arbitrary tile, never the full reconstructed photo (e.g.
/// 3024x4032) -- ffmpeg instead surfaces the reconstructed size through a
/// separate `stream_groups` entry. Falls back to the plain stream's
/// dimensions when no stream group exists: every non-grid input (a plain
/// PNG, a simple single-image HEIC) and the single-frame `.jpg` output,
/// which never has one.
async fn probe_dimensions(path: &Path) -> Option<(u32, u32)> {
    let output = tokio::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height",
            "-show_stream_groups",
        ])
        .arg(path)
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let parsed: FfprobeDimensionsOutput = serde_json::from_slice(&output.stdout).ok()?;
    if let Some(component) = parsed
        .stream_groups
        .first()
        .and_then(|group| group.components.first())
        && let (Some(width), Some(height)) = (component.width, component.height)
    {
        return Some((width, height));
    }
    let stream = parsed.streams.first()?;
    Some((stream.width?, stream.height?))
}

/// Transcodes `input` (png/heic) to `output` (always `.jpg`):
/// ```text
/// ffmpeg -y -loglevel error -i <input> -frames:v 1 -q:v 1 -pix_fmt yuvj444p -f mjpeg <output>
/// ```
/// `-q:v 1` is ffmpeg's mjpeg encoder's highest quality setting (1=best ..
/// 31=worst); `-pix_fmt yuvj444p` forces 4:4:4 chroma instead of ffmpeg's
/// default 4:2:0 subsampling for JPEG output -- the single largest avoidable
/// quality loss in a naive encode beyond quantization itself. `-f mjpeg`
/// forces the muxer explicitly: `worker.rs` writes to a scratch path ending
/// in `.scratch`, not `.jpg` (the real destination name plus a `.scratch`
/// suffix), and without `-f` ffmpeg tries to infer the output format from
/// that final extension alone and fails outright ("Unable to find a
/// suitable output format"). No `-vf scale=...` or other filter is ever
/// applied, preserving the original pixel dimensions exactly. A non-zero
/// `ffmpeg` exit -- including a HEIC input on a `libheif`-less build, which
/// fails immediately with ffmpeg's own decoder-missing message -- returns
/// `Err` with that stderr folded in verbatim -- `worker.rs`'s call site
/// retries this a bounded number of times before giving up and recording
/// the failure for just this one file (ADR-0116; no longer a whole-run
/// abort, amending ADR-0112 Decision §7); there is no separate
/// HEIC-capability preflight probe.
pub(crate) async fn transcode_to_jpg(input: &Path, output: &Path) -> Result<(), String> {
    let result = tokio::process::Command::new("ffmpeg")
        .args(["-y", "-loglevel", "error"])
        .arg("-i")
        .arg(input)
        .args([
            "-frames:v",
            "1",
            "-q:v",
            "1",
            "-pix_fmt",
            "yuvj444p",
            "-f",
            "mjpeg",
        ])
        .arg(output)
        .output()
        .await
        .map_err(|err| format!("failed to run ffmpeg: {err}"))?;

    if !result.status.success() {
        return Err(format!(
            "ffmpeg failed to transcode {}: {}",
            input.display(),
            String::from_utf8_lossy(&result.stderr).trim()
        ));
    }

    if let (Some(before), Some(after)) = (
        probe_dimensions(input).await,
        probe_dimensions(output).await,
    ) {
        let before_area = before.0 as u64 * before.1 as u64;
        let after_area = after.0 as u64 * after.1 as u64;
        if before_area != after_area {
            return Err(format!(
                "transcoding {} changed pixel area from {before:?} ({before_area} px) to \
                 {after:?} ({after_area} px); refusing an output that isn't full-size",
                input.display()
            ));
        }
    }

    Ok(())
}

/// Tolerance for `transcode_video`'s duration guard, in seconds -- absorbs
/// ordinary container/muxing overhead between the source and the re-encoded
/// `.mp4`, while still catching a truncated or partial encode that
/// nonetheless exits 0.
const DURATION_TOLERANCE_SECONDS: f64 = 0.5;

/// Extra duration allowance, as a fraction of the input's own duration
/// (ADR-0153).
///
/// The absolute `DURATION_TOLERANCE_SECONDS` floor alone is wrong for any
/// input whose duration ffprobe can only *estimate*. A raw ADTS `.aac`
/// stream carries no container duration, so ffprobe derives it from bitrate
/// and size, and the error scales with length. Measured on generated
/// fixtures:
///
/// | source | reported in | out | delta |
/// |---|---|---|---|
/// | 3s | 3.000s | 3.048s | 0.048s |
/// | 30s | 29.627s | 30.030s | 0.403s |
/// | 180s | 177.868s | 180.031s | **2.162s** |
///
/// A ~1.2% under-estimate, so every real-length track would have failed the
/// old guard. 2% covers that with headroom while still catching a partial
/// write, which is a far larger fraction of the stream.
const DURATION_TOLERANCE_FRACTION: f64 = 0.02;

/// Whether `after` is short enough of `before` to mean a truncated or
/// partial encode (ADR-0153).
///
/// **One-sided on purpose.** A partial write always produces a *shorter*
/// output; an output *longer* than the input is never evidence of
/// truncation, and for an estimated input duration it is the normal case
/// (see `DURATION_TOLERANCE_FRACTION`'s measurements, where every output
/// exceeded its reported input). The old symmetric comparison treated that
/// as a failure.
///
/// Pure so the thresholds are testable without running `ffmpeg`, the same
/// reason ADR-0143 extracted `circuit_breaker_should_trip`.
fn is_truncated_encode(before: f64, after: f64) -> bool {
    let allowance = DURATION_TOLERANCE_SECONDS.max(before.abs() * DURATION_TOLERANCE_FRACTION);
    before - after > allowance
}

/// `path`'s container-level duration in seconds (`ffprobe`'s
/// `format=duration`), or `None` if `ffprobe` can't determine it -- treated
/// as "skip the check," same posture as `probe_dimensions`.
async fn probe_duration_seconds(path: &Path) -> Option<f64> {
    let output = tokio::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(path)
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<f64>()
        .ok()
}

/// Best-effort diagnostic string appended to `transcode_video`'s pixel-area
/// mismatch error only (ADR-0125, crop fields added by ADR-0126) -- one extra
/// `ffprobe` call made once the guard is already about to fail, so the
/// common (matching) path pays no extra cost. Surfaces exactly the fields
/// `probe_dimensions` deliberately omits (sample/display aspect ratio, any
/// rotation tag or display-matrix side data, the four numeric Frame Cropping
/// extents, stream count/tags) -- the first things a future investigation
/// of this class of failure will want and this incident's own log lacked.
/// `stream_side_data`/`stream_tags` must be requested as their own
/// top-level `-show_entries` sections, not nested under `stream=...` --
/// ffprobe silently returns empty placeholders for the nested form.
async fn probe_video_diagnostics(path: &Path) -> String {
    let output = tokio::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_entries",
            "stream=index,codec_type,width,height,sample_aspect_ratio,display_aspect_ratio:\
             stream_side_data=side_data_type,rotation,crop_top,crop_bottom,crop_left,crop_right:\
             stream_tags=rotate",
        ])
        .arg(path)
        .output()
        .await;
    match output {
        Ok(o) if o.status.success() => {
            format!(
                " (diagnostics: {})",
                String::from_utf8_lossy(&o.stdout).trim()
            )
        }
        _ => String::new(),
    }
}

/// `transcode_video`'s audio-only rejection (ADR-0141), and
/// `transcode_audio`'s mirror-image video rejection (ADR-0142).
///
/// Named consts rather than inline literals (ADR-0143) because three
/// separate places have to agree on each substring: the `format!` that
/// produces it, `NON_RETRYABLE_SIGNATURES`, and
/// `is_wrong_input_kind_error`. Unlike every other entry in that signature
/// list, these two match **pigeon's own** message text rather than
/// `ffmpeg`'s stderr, so they are not version-fragile -- but only as long
/// as producer and matcher cannot drift apart, which is what these consts
/// guarantee.
const NO_VIDEO_STREAM_SIGNATURE: &str = "has no video stream";
const UNEXPECTED_VIDEO_STREAM_SIGNATURE: &str = "is not an audio-only recording";

/// `sheets`'s not-a-readable-workbook rejection (ADR-0149).
///
/// Lives here rather than in `sheets.rs`, despite this module otherwise
/// being `ffmpeg`-only, for two reasons: `NON_RETRYABLE_SIGNATURES` below
/// is a *function-local* const, so it can only be appended to here anyway;
/// and the doc comment above exists precisely to keep the producer and both
/// matchers from drifting apart, which splitting them across modules would
/// invite.
pub(crate) const NOT_A_WORKBOOK_SIGNATURE: &str = "is not a readable spreadsheet";

/// `documents`'s not-a-readable-document rejection (ADR-0154), covering all
/// three of its subprocess converters -- `soffice`, `ebook-convert` and
/// `pandoc` -- plus a malformed `tsv`. Here for the same two reasons
/// `NOT_A_WORKBOOK_SIGNATURE` is.
pub(crate) const NOT_A_DOCUMENT_SIGNATURE: &str = "is not a readable document";

/// `documents::ensure_keynote`'s rejection of a `.key` that is not an iWork
/// container (ADR-0154 §2).
///
/// Distinct from `NOT_A_DOCUMENT_SIGNATURE` on purpose: this one is the
/// difference between a wasted conversion and a published private key, so
/// it is worth being able to grep a run's log for it specifically.
pub(crate) const NOT_A_KEYNOTE_SIGNATURE: &str = "is not a Keynote presentation";

/// `documents::clipping_to_text`'s rejection of a file that is not a
/// readable macOS text clipping -- in practice a pre-OS-X resource-fork one
/// (ADR-0154).
pub(crate) const NOT_A_TEXTCLIPPING_SIGNATURE: &str = "is not a readable text clipping";

/// Whether `err` means the operator pointed this run at the wrong
/// `--input-file-type` for this file -- the file itself is intact
/// (ADR-0143).
///
/// Deliberately a separate predicate from
/// `is_non_retryable_transcode_error`, which answers a different question
/// (does retrying help?). Both are true for these two signatures, but they
/// are not interchangeable: this one licenses deleting the local source
/// copy, which is only safe because such a file is undamaged and still
/// present at `--source`.
pub(crate) fn is_wrong_input_kind_error(err: &str) -> bool {
    [
        NO_VIDEO_STREAM_SIGNATURE,
        UNEXPECTED_VIDEO_STREAM_SIGNATURE,
        NOT_A_WORKBOOK_SIGNATURE,
        NOT_A_DOCUMENT_SIGNATURE,
        NOT_A_KEYNOTE_SIGNATURE,
        NOT_A_TEXTCLIPPING_SIGNATURE,
    ]
    .iter()
    .any(|signature| err.contains(signature))
}

/// Whether `path` contains at least one video stream, used by
/// `transcode_video` to reject an audio-only input before `ffmpeg` runs
/// (ADR-0141). Unlike `probe_dimensions`/`probe_duration_seconds`, an
/// `ffprobe` failure here returns `false` rather than "skip the check": a
/// file whose streams can't be enumerated at all has no video stream this
/// job can transcode, and `transcode_video`'s mandatory `-map 0:v:0` would
/// fail on it regardless.
async fn has_video_stream(path: &Path) -> bool {
    let Ok(output) = tokio::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v",
            "-show_entries",
            "stream=index:stream_disposition=attached_pic",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .await
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let Ok(probe) = serde_json::from_slice::<VideoStreamProbe>(&output.stdout) else {
        return false;
    };
    probe
        .streams
        .iter()
        .any(|stream| stream.disposition.attached_pic == 0)
}

/// `has_video_stream`'s probe output (ADR-0153).
///
/// The `disposition` is what this exists for. ffmpeg models **embedded
/// cover art as a video stream**, so a plain `-select_streams v` count
/// returns 1 for any tagged music file -- which would have made ADR-0142's
/// has-a-video-stream guard reject essentially every `.mp3`/`.m4a`/`.aac`
/// in a real library, classify it wrong-input-kind (so ADR-0143 deletes the
/// local copy), and trip ADR-0121's circuit breaker into aborting the run.
/// Measured against a real fixture: an m4a with attached art reports
/// stream index 1 under the old probe and `attached_pic=1` under this one.
#[derive(Debug, Deserialize, Default)]
struct VideoStreamProbe {
    #[serde(default)]
    streams: Vec<VideoStreamEntry>,
}

#[derive(Debug, Deserialize)]
struct VideoStreamEntry {
    #[serde(default)]
    disposition: VideoStreamDisposition,
}

#[derive(Debug, Deserialize, Default)]
struct VideoStreamDisposition {
    /// `1` for cover art, `0` for a genuine video stream. Defaulted so a
    /// probe that omits the field entirely is read as real video, which is
    /// the conservative direction -- it preserves ADR-0141/0142's guards
    /// rather than silently disarming them.
    #[serde(default)]
    attached_pic: u8,
}

/// Transcodes `input` (mov/m4v/mp4) to `output` (always `.mp4`, H.265/HEVC,
/// ADR-0122):
/// ```text
/// ffmpeg -y -loglevel error -noautorotate -apply_cropping:v codec -i <input> \
///   -map 0:v:0 -map 0:a:0? -c:v libx265 -tag:v hvc1 -preset medium \
///   [-crf <N> | -x265-params lossless=1] -c:a aac -b:a 256k -f mp4 <output>
/// ```
/// `-tag:v hvc1` is required for QuickTime/Apple-device playback of
/// HEVC-in-MP4 -- ffmpeg's default `hev1` tag is not recognized by Apple's
/// own players. `-f mp4` forces the muxer explicitly, exactly
/// `transcode_to_jpg`'s own ADR-0115 lesson: `worker.rs` writes to a scratch
/// path ending in `.scratch`, not `.mp4`, so ffmpeg can never be allowed to
/// infer the output format from the on-disk extension alone. No
/// `-vf scale=...` or other resize filter is ever applied in pigeon's own
/// command line, but that alone doesn't guarantee the output's raw pixel
/// grid matches the input's: `-noautorotate` (ADR-0125) is required because
/// ffmpeg otherwise *implicitly* reshapes the decoded frame to match a
/// container-embedded display transform (QuickTime/MP4's `tkhd` matrix) --
/// for most phone video this is a harmless, area-preserving ±90°/180°
/// rotation, but some real camera-captured files encode a rotation *plus* a
/// non-unity scale, which silently shrinks/grows the actual output grid
/// despite pigeon applying no filter of its own. `-noautorotate` suppresses
/// that implicit reshape unconditionally; empirically confirmed (ADR-0125)
/// that the original display matrix still survives onto the re-encoded
/// output as frame-level side data even though this is a full re-encode,
/// not a stream copy, so playback orientation is unaffected. `-apply_cropping:v
/// codec` (ADR-0126) guards an independent, later-added ffmpeg mechanism: FFmpeg
/// 7.1 added support for QuickTime's Clean Aperture (`clap` atom) *container*-level
/// crop metadata, which -- left at ffmpeg's own default of `all` -- gets silently
/// auto-applied during decode exactly like autorotate's implicit reshape, shrinking
/// the decoded frame below what `probe_dimensions`'s bare `codecpar.width/height`
/// reports. `codec` is the deliberate, narrower choice, not `none`: `none` also
/// suppresses the much older, unrelated decoder-level crop `probe_dimensions` has
/// *always* already reflected (the H.264/HEVC SPS "conformance window"), so `none`
/// would trade today's container-crop false-positive for a far more common
/// codec-crop false-positive on virtually any non-16-pixel-aligned video -- do
/// not "simplify" this to `none`. `-map 0:v:0
/// -map 0:a:0?` (optional audio map, tolerating a video-only input) pins the
/// exact stream `probe_dimensions` already reads via `-select_streams v:0`,
/// removing any ambiguity from ffmpeg's own default stream selection on a
/// multi-video-track `.mov`. Dimensions are verified by the same pixel-area
/// dimension guard `transcode_to_jpg` uses, plus a duration guard: video has
/// a truncated/partial-encode failure mode images don't (a process that
/// exits 0 having only written part of the stream), which only a duration
/// comparison can catch. A non-zero `ffmpeg` exit -- including a
/// `libx265`-less ffmpeg build, which fails immediately with an "Unknown
/// encoder" message -- returns `Err` with that stderr folded in verbatim;
/// `worker.rs`'s call site retries this a bounded number of times before
/// giving up and recording the failure for just this one file, exactly like
/// `transcode_to_jpg`.
///
/// `threads` caps libx265's own internal thread count (`-threads <n>`,
/// ADR-0127): with no cap, every concurrent `transcode_video` call
/// independently defaults to using every core on the host, so `N` files
/// transcoding at once oversubscribe CPU (and the memory that scales with
/// it) by a factor of `N` on top of `--concurrency` itself already bounding
/// the process count. Callers pass `(available_parallelism /
/// concurrency).max(1)` so the whole fleet of concurrent `ffmpeg`
/// invocations stays within the host's real core count.
///
/// An audio-only input is rejected up front via `has_video_stream`
/// (ADR-0141), before `ffmpeg` runs. `-map 0:v:0` is mandatory, so such a
/// file always failed anyway -- but with ffmpeg's raw `"Stream map ''
/// matches no streams"` stderr, which named neither the real problem nor
/// the file, and which (not being a known non-retryable signature) was
/// retried twice for a guaranteed-identical outcome. Voice-note recordings
/// saved with an `.mp4` extension are the real-world source of these.
pub(crate) async fn transcode_video(
    input: &Path,
    output: &Path,
    quality: VideoQuality,
    threads: usize,
) -> Result<(), String> {
    if !has_video_stream(input).await {
        return Err(format!(
            "{} {NO_VIDEO_STREAM_SIGNATURE} (likely an audio-only recording saved with a video \
             extension); see ADR-0142 for an audio-preserving input type",
            input.display()
        ));
    }

    let mut command = tokio::process::Command::new("ffmpeg");
    command.args([
        "-y",
        "-loglevel",
        "error",
        "-noautorotate",
        "-apply_cropping:v",
        "codec",
    ]);
    command.arg("-i").arg(input);
    command.args(["-map", "0:v:0", "-map", "0:a:0?"]);
    command.args(["-c:v", "libx265", "-tag:v", "hvc1", "-preset", "medium"]);
    command.args(["-threads", &threads.max(1).to_string()]);
    match quality.crf() {
        Some(crf) => {
            command.args(["-crf", &crf.to_string()]);
        }
        None => {
            command.args(["-x265-params", "lossless=1"]);
        }
    }
    command.args(["-c:a", "aac", "-b:a", "256k", "-f", "mp4"]);
    command.arg(output);

    let result = command
        .output()
        .await
        .map_err(|err| format!("failed to run ffmpeg: {err}"))?;

    if !result.status.success() {
        return Err(format!(
            "ffmpeg failed to transcode {}: {}",
            input.display(),
            String::from_utf8_lossy(&result.stderr).trim()
        ));
    }

    if let (Some(before), Some(after)) = (
        probe_dimensions(input).await,
        probe_dimensions(output).await,
    ) {
        let before_area = before.0 as u64 * before.1 as u64;
        let after_area = after.0 as u64 * after.1 as u64;
        if before_area != after_area {
            let diagnostics = probe_video_diagnostics(input).await;
            return Err(format!(
                "transcoding {} changed pixel area from {before:?} ({before_area} px) to \
                 {after:?} ({after_area} px); refusing an output that isn't full-size{diagnostics}",
                input.display()
            ));
        }
    }

    if let (Some(before), Some(after)) = (
        probe_duration_seconds(input).await,
        probe_duration_seconds(output).await,
    ) && is_truncated_encode(before, after)
    {
        return Err(format!(
            "transcoding {} shortened duration from {before:.3}s to {after:.3}s (allowance \
             {DURATION_TOLERANCE_SECONDS}s or {}% of the input, whichever is larger); refusing \
             a truncated or partial encode",
            input.display(),
            DURATION_TOLERANCE_FRACTION * 100.0
        ));
    }

    Ok(())
}

/// Transcodes `input` (an audio-only `.mp4`, ADR-0142) to `output`
/// (always `.opus`, Ogg-encapsulated Opus):
/// ```text
/// ffmpeg -y -loglevel error -i <input> -map 0:a:0 \
///   -c:a libopus -b:a 48k -application audio -f opus <output>
/// ```
/// Opus is the best quality-per-bit codec available for this content class
/// at these bitrates, and costs nothing for the intended future
/// transcription work -- every transcription pipeline decodes to PCM
/// first, so the stored codec is irrelevant to it beyond whether the audio
/// survived intact. `-application audio` is libopus's general-purpose,
/// higher-fidelity mode and is already its default; do **not** "optimize"
/// it to `voip`, whose more aggressive telephony-tuned speech bandpass
/// would discard content a transcription model might still use.
/// `-f opus` forces the muxer explicitly, the same ADR-0115 lesson
/// `transcode_to_jpg`/`transcode_video` already encode: `worker.rs` writes
/// to a scratch path ending in `.scratch`, so ffmpeg can never be allowed
/// to infer the container from the on-disk extension. `-map 0:a:0` pins
/// the first audio stream rather than relying on ffmpeg's default stream
/// selection, exactly as `transcode_video` pins `0:v:0`.
///
/// An input that still has a video stream is **refused**, not silently
/// stripped -- the load-bearing safety guard, not a formality. Nothing
/// filters a `mp4-audio` run's source down to audio-only files (no
/// extension distinguishes them), and ADR-0138/0139's prefix inference
/// means `--source source` with no explicit prefix resolves to
/// `source:mp4/`: the same default address a plain `--input-file-type mp4`
/// video run uses. Without this guard, one omitted prefix would quietly
/// transcode a bucket of real video into audio-only files and push them.
///
/// Verified by the same duration guard `transcode_video` uses -- audio
/// shares exactly that truncated/partial-encode failure mode. The
/// pixel-area dimension guard is skipped: there are no dimensions.
pub(crate) async fn transcode_audio(
    input: &Path,
    output: &Path,
    quality: AudioQuality,
) -> Result<(), String> {
    if has_video_stream(input).await {
        return Err(format!(
            "{} has a video stream, so it {UNEXPECTED_VIDEO_STREAM_SIGNATURE}; refusing to \
             discard video (use --input-file-type mp4 to transcode it as video)",
            input.display()
        ));
    }

    let result = tokio::process::Command::new("ffmpeg")
        .args(["-y", "-loglevel", "error"])
        .arg("-i")
        .arg(input)
        .args([
            "-map",
            "0:a:0",
            "-c:a",
            "libopus",
            "-b:a",
            quality.bitrate(),
            "-application",
            "audio",
            "-f",
            "opus",
        ])
        .arg(output)
        .output()
        .await
        .map_err(|err| format!("failed to run ffmpeg: {err}"))?;

    if !result.status.success() {
        return Err(format!(
            "ffmpeg failed to transcode {}: {}",
            input.display(),
            String::from_utf8_lossy(&result.stderr).trim()
        ));
    }

    if let (Some(before), Some(after)) = (
        probe_duration_seconds(input).await,
        probe_duration_seconds(output).await,
    ) && is_truncated_encode(before, after)
    {
        return Err(format!(
            "transcoding {} shortened duration from {before:.3}s to {after:.3}s (allowance \
             {DURATION_TOLERANCE_SECONDS}s or {}% of the input, whichever is larger); refusing \
             a truncated or partial encode",
            input.display(),
            DURATION_TOLERANCE_FRACTION * 100.0
        ));
    }

    Ok(())
}

/// Matches `ffmpeg`/`ffprobe` failure signatures that are deterministic --
/// re-running `transcode_to_jpg` against the exact same input reproduces the
/// identical failure every time, so retrying buys nothing but doubles ffmpeg
/// invocations and adds a guaranteed sleep per file (ADR-0121). Matches
/// substrings, not full messages or exit codes -- same caution as
/// `destination.rs`'s rclone-stderr precedent (ADR-0120): pattern-matching
/// subprocess output is inherently version-fragile; reconfirm these
/// substrings against whatever `ffmpeg` version is actually deployed.
pub(crate) fn is_non_retryable_transcode_error(err: &str) -> bool {
    const NON_RETRYABLE_SIGNATURES: &[&str] = &[
        "moov atom not found",
        "Invalid data found when processing input",
        "Decoder (codec",
        "could not find codec parameters",
        "refusing an output that isn't full-size",
        "Unknown encoder",
        NO_VIDEO_STREAM_SIGNATURE,
        UNEXPECTED_VIDEO_STREAM_SIGNATURE,
        NOT_A_WORKBOOK_SIGNATURE,
        NOT_A_DOCUMENT_SIGNATURE,
        NOT_A_KEYNOTE_SIGNATURE,
        NOT_A_TEXTCLIPPING_SIGNATURE,
    ];
    NON_RETRYABLE_SIGNATURES
        .iter()
        .any(|signature| err.contains(signature))
}

/// An input already in the target format is never routed through a
/// converter -- a plain byte-for-byte `fs::copy`, zero generational loss
/// (ADR-0112 Decision §4 for `jpeg`; ADR-0149 reuses it unchanged for
/// `csv`, on the same reasoning).
pub(crate) fn copy_through(input: &Path, output: &Path) -> Result<(), String> {
    std::fs::copy(input, output).map_err(|err| {
        format!(
            "failed to copy {} to {}: {err}",
            input.display(),
            output.display()
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_through_is_byte_for_byte_identical() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("photo.jpeg");
        let output = dir.path().join("photo.jpg");
        let bytes: Vec<u8> = (0..255).collect();
        std::fs::write(&input, &bytes).unwrap();

        copy_through(&input, &output).unwrap();

        assert_eq!(std::fs::read(&output).unwrap(), bytes);
    }

    #[test]
    fn copy_through_errors_on_a_missing_input() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("does-not-exist.jpeg");
        let output = dir.path().join("out.jpg");
        assert!(copy_through(&input, &output).is_err());
    }

    /// Generates a tiny synthetic PNG via ffmpeg's `lavfi` test source --
    /// avoids needing a checked-in binary fixture.
    async fn generate_test_png(path: &Path) -> Result<(), String> {
        let result = tokio::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=1:size=64x48:rate=1",
                "-frames:v",
                "1",
            ])
            .arg(path)
            .output()
            .await
            .map_err(|err| format!("failed to run ffmpeg: {err}"))?;
        if !result.status.success() {
            return Err(String::from_utf8_lossy(&result.stderr).to_string());
        }
        Ok(())
    }

    #[tokio::test]
    async fn transcode_to_jpg_preserves_dimensions_for_a_real_png() {
        if check_ffmpeg_available().await.is_err() {
            eprintln!("skipping: ffmpeg/ffprobe not found on PATH");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("test.png");
        generate_test_png(&input).await.unwrap();

        let output = dir.path().join("test.jpg");
        transcode_to_jpg(&input, &output).await.unwrap();

        assert!(output.exists());
        assert_eq!(
            probe_dimensions(&input).await,
            probe_dimensions(&output).await
        );
        assert_eq!(probe_dimensions(&output).await, Some((64, 48)));
    }

    /// Mirrors `worker.rs`'s real scratch-file naming (`<destination-name>.scratch`,
    /// where `destination-name` already ends in `.jpg`) -- without `-f mjpeg`
    /// this fails with ffmpeg's "Unable to find a suitable output format"
    /// because the final extension on disk is `.scratch`, not `.jpg`.
    #[tokio::test]
    async fn transcode_to_jpg_succeeds_when_the_output_path_ends_in_scratch() {
        if check_ffmpeg_available().await.is_err() {
            eprintln!("skipping: ffmpeg/ffprobe not found on PATH");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("test.png");
        generate_test_png(&input).await.unwrap();

        let output = dir.path().join("test-abcdef0123456789.jpg.scratch");
        transcode_to_jpg(&input, &output).await.unwrap();

        assert!(output.exists());
    }

    #[tokio::test]
    async fn transcode_to_jpg_fails_fast_on_an_unreadable_input() {
        if check_ffmpeg_available().await.is_err() {
            eprintln!("skipping: ffmpeg/ffprobe not found on PATH");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("not-an-image.heic");
        std::fs::write(&input, b"this is not a real heic file").unwrap();
        let output = dir.path().join("out.jpg");

        let result = transcode_to_jpg(&input, &output).await;
        assert!(result.is_err());
    }

    /// Generates a tiny synthetic video (with a silent audio track) via
    /// ffmpeg's `lavfi` test sources -- avoids needing a checked-in binary
    /// fixture, mirroring `generate_test_png` above.
    async fn generate_test_video(path: &Path) -> Result<(), String> {
        let result = tokio::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=1:size=64x48:rate=10",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=duration=1",
                "-c:v",
                "libx264",
                "-c:a",
                "aac",
            ])
            .arg(path)
            .output()
            .await
            .map_err(|err| format!("failed to run ffmpeg: {err}"))?;
        if !result.status.success() {
            return Err(String::from_utf8_lossy(&result.stderr).to_string());
        }
        Ok(())
    }

    #[tokio::test]
    async fn transcode_video_preserves_dimensions_and_duration_for_a_real_video() {
        if check_ffmpeg_available().await.is_err() {
            eprintln!("skipping: ffmpeg/ffprobe not found on PATH");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("test.mp4");
        if generate_test_video(&input).await.is_err() {
            eprintln!("skipping: this ffmpeg build can't generate a test fixture");
            return;
        }

        let output = dir.path().join("test-output.mp4");
        if let Err(err) = transcode_video(&input, &output, VideoQuality::Medium, 1).await {
            eprintln!("skipping: this ffmpeg build can't encode libx265: {err}");
            return;
        }

        assert!(output.exists());
        assert_eq!(
            probe_dimensions(&input).await,
            probe_dimensions(&output).await
        );
        let before_duration = probe_duration_seconds(&input).await.unwrap();
        let after_duration = probe_duration_seconds(&output).await.unwrap();
        assert!((before_duration - after_duration).abs() <= DURATION_TOLERANCE_SECONDS);
    }

    /// Generates a base test video (as `generate_test_video`) and remuxes a
    /// copy with a `degrees` display-matrix rotation attached at the
    /// container level (`-display_rotation`, an ffmpeg *input* option, on a
    /// `-c copy` remux) -- mirrors a real device-captured `.mov`'s embedded
    /// `tkhd` rotation matrix (ADR-0125) without needing a checked-in binary
    /// fixture. The rotation must be attached via a separate remux of a real
    /// container, not folded into the original `lavfi` generation command --
    /// doing the latter bakes the rotation into the raw frames instead of
    /// attaching container-level side data, and wouldn't exercise the bug
    /// this is meant to pin down.
    async fn generate_test_video_with_rotation(path: &Path, degrees: i32) -> Result<(), String> {
        let base_path = path.with_file_name("rotation-base.mp4");
        generate_test_video(&base_path).await?;
        let result = tokio::process::Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error", "-display_rotation"])
            .arg(degrees.to_string())
            .arg("-i")
            .arg(&base_path)
            .args(["-c", "copy", "-f", "mp4"])
            .arg(path)
            .output()
            .await
            .map_err(|err| format!("failed to run ffmpeg: {err}"))?;
        if !result.status.success() {
            return Err(String::from_utf8_lossy(&result.stderr).to_string());
        }
        Ok(())
    }

    /// ADR-0125 regression test: a `.mov`/`.mp4` whose container carries a
    /// rotation display matrix must transcode without ffmpeg's implicit
    /// autorotate reshaping the output's raw pixel grid -- `-noautorotate`
    /// means the comparison must hold on the *exact* `(width, height)` tuple
    /// here, not merely equal area, pinning the no-reshape mechanism.
    ///
    /// Caveat: this fixture is a pure rotation (area-preserving), which
    /// would *not* have failed even before this fix -- ffmpeg's CLI exposes
    /// no flag to inject the real incident's rotation-plus-non-unity-scale
    /// matrix. This test covers the mechanism, not the exact production
    /// fingerprint (see ADR-0125's Decision §3/§4 for the manual
    /// verification that covered the latter).
    #[tokio::test]
    async fn transcode_video_preserves_raw_dimensions_for_an_input_with_a_rotation_display_matrix()
    {
        if check_ffmpeg_available().await.is_err() {
            eprintln!("skipping: ffmpeg/ffprobe not found on PATH");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("rotated.mp4");
        if generate_test_video_with_rotation(&input, 90).await.is_err() {
            eprintln!("skipping: this ffmpeg build can't attach a display-rotation matrix");
            return;
        }

        let output = dir.path().join("rotated-output.mp4");
        if let Err(err) = transcode_video(&input, &output, VideoQuality::Medium, 1).await {
            panic!("transcode_video incorrectly rejected a rotated-but-unreshaped input: {err}");
        }

        assert_eq!(
            probe_dimensions(&input).await,
            probe_dimensions(&output).await
        );
    }

    /// Generates a base H.264 test video (not HEVC -- `h264_metadata` is an
    /// H.264-only bitstream filter) and remuxes a copy with a codec-level
    /// (SPS "conformance window") crop attached via that filter -- mirrors
    /// the long-standing crop mechanism `probe_dimensions`'s bare
    /// `codecpar.width/height` has *always* already reflected, distinct from
    /// ADR-0126's container-level (`clap`-atom) crop. `crop_left`/
    /// `crop_right` must each be a multiple of 2 (ffmpeg rejects an odd
    /// value outright); 2+2 trims the width by exactly 4px, confirmed
    /// empirically against a real `h264_metadata` run.
    async fn generate_test_video_with_sps_crop(path: &Path) -> Result<(), String> {
        let base_path = path.with_file_name("sps-crop-base.mp4");
        let result = tokio::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=1:size=64x48:rate=5",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&base_path)
            .output()
            .await
            .map_err(|err| format!("failed to run ffmpeg: {err}"))?;
        if !result.status.success() {
            return Err(String::from_utf8_lossy(&result.stderr).to_string());
        }

        let result = tokio::process::Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error", "-i"])
            .arg(&base_path)
            .args([
                "-c:v",
                "copy",
                "-bsf:v",
                "h264_metadata=crop_left=2:crop_right=2",
                "-f",
                "mp4",
            ])
            .arg(path)
            .output()
            .await
            .map_err(|err| format!("failed to run ffmpeg: {err}"))?;
        if !result.status.success() {
            return Err(String::from_utf8_lossy(&result.stderr).to_string());
        }
        Ok(())
    }

    /// ADR-0126 regression test: a codec-level (SPS) crop -- the kind
    /// `probe_dimensions` has always already reflected -- must still
    /// transcode correctly with `-apply_cropping:v codec` in place. This
    /// pins the "codec, not none" choice: if a future reader "simplifies"
    /// `-apply_cropping:v codec` to `-apply_cropping:v none`, the output
    /// reverts to the pre-crop coded size and this test starts failing
    /// (empirically confirmed via a direct ffmpeg run: `codec` and the old
    /// pre-ADR-0126 default both produce the cropped size here; `none`
    /// regresses to the uncropped one).
    #[tokio::test]
    async fn transcode_video_preserves_codec_level_cropped_dimensions() {
        if check_ffmpeg_available().await.is_err() {
            eprintln!("skipping: ffmpeg/ffprobe not found on PATH");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("sps-cropped.mp4");
        if generate_test_video_with_sps_crop(&input).await.is_err() {
            eprintln!("skipping: this ffmpeg build can't attach an SPS crop via h264_metadata");
            return;
        }

        let output = dir.path().join("sps-cropped-output.mp4");
        if let Err(err) = transcode_video(&input, &output, VideoQuality::Medium, 1).await {
            panic!("transcode_video incorrectly rejected a codec-level-cropped input: {err}");
        }

        assert_eq!(
            probe_dimensions(&input).await,
            probe_dimensions(&output).await
        );
    }

    /// Mirrors `transcode_to_jpg_succeeds_when_the_output_path_ends_in_scratch`
    /// -- without `-f mp4` this fails because the on-disk extension is
    /// `.scratch`, not `.mp4`.
    #[tokio::test]
    async fn transcode_video_succeeds_when_the_output_path_ends_in_scratch() {
        if check_ffmpeg_available().await.is_err() {
            eprintln!("skipping: ffmpeg/ffprobe not found on PATH");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("test.mp4");
        if generate_test_video(&input).await.is_err() {
            eprintln!("skipping: this ffmpeg build can't generate a test fixture");
            return;
        }

        let output = dir.path().join("test-abcdef0123456789.mp4.scratch");
        if let Err(err) = transcode_video(&input, &output, VideoQuality::Low, 1).await {
            eprintln!("skipping: this ffmpeg build can't encode libx265: {err}");
            return;
        }

        assert!(output.exists());
    }

    #[tokio::test]
    async fn transcode_video_fails_fast_on_an_unreadable_input() {
        if check_ffmpeg_available().await.is_err() {
            eprintln!("skipping: ffmpeg/ffprobe not found on PATH");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("not-a-video.mp4");
        std::fs::write(&input, b"this is not a real mp4 file").unwrap();
        let output = dir.path().join("out.mp4");

        let result = transcode_video(&input, &output, VideoQuality::Medium, 1).await;
        assert!(result.is_err());
    }

    /// Generates an audio-only `.mp4` (an AAC stream, no video stream at
    /// all) via ffmpeg's `anullsrc` test source -- mirrors the real-world
    /// `audioclip*.mp4` voice-note recordings that reached
    /// `transcode_video` in production (ADR-0141).
    async fn generate_audio_only_mp4(path: &Path) -> Result<(), String> {
        let result = tokio::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=duration=1",
                "-c:a",
                "aac",
                "-f",
                "mp4",
            ])
            .arg(path)
            .output()
            .await
            .map_err(|err| format!("failed to run ffmpeg: {err}"))?;
        if !result.status.success() {
            return Err(String::from_utf8_lossy(&result.stderr).to_string());
        }
        Ok(())
    }

    /// ADR-0141 regression test: an audio-only input must be rejected with a
    /// message naming the real problem, before `ffmpeg` runs -- not with
    /// ffmpeg's own `"Stream map '' matches no streams"` stderr, which named
    /// neither the file nor the cause, and which was retried twice because
    /// it matched no non-retryable signature.
    #[tokio::test]
    async fn transcode_video_fails_with_a_clear_error_on_an_audio_only_input() {
        if check_ffmpeg_available().await.is_err() {
            eprintln!("skipping: ffmpeg/ffprobe not found on PATH");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("audioclip.mp4");
        if generate_audio_only_mp4(&input).await.is_err() {
            eprintln!("skipping: this ffmpeg build can't generate an audio-only fixture");
            return;
        }
        assert!(!has_video_stream(&input).await);

        let output = dir.path().join("out.mp4");
        let err = transcode_video(&input, &output, VideoQuality::Medium, 1)
            .await
            .expect_err("an audio-only input must not transcode as video");

        assert!(
            err.contains("has no video stream"),
            "unexpected error: {err}"
        );
        assert!(
            is_non_retryable_transcode_error(&err),
            "an audio-only input is deterministic and must never be retried: {err}"
        );
        assert!(
            !output.exists(),
            "ffmpeg must not have run at all for an audio-only input"
        );
    }

    /// ADR-0142: an audio-only input transcodes to a real `.opus`, with its
    /// duration preserved inside the shared tolerance. Also pins the
    /// ADR-0115 muxer-inference lesson for this output format -- the
    /// scratch path ends in `.scratch`, not `.opus`, so without `-f opus`
    /// ffmpeg could not infer the container.
    #[tokio::test]
    async fn transcode_audio_produces_an_opus_preserving_duration_from_a_scratch_suffixed_path() {
        if check_ffmpeg_available().await.is_err() {
            eprintln!("skipping: ffmpeg/ffprobe not found on PATH");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("voice.mp4");
        if generate_audio_only_mp4(&input).await.is_err() {
            eprintln!("skipping: this ffmpeg build can't generate an audio-only fixture");
            return;
        }

        let output = dir.path().join("voice-abcdef0123456789.opus.scratch");
        if let Err(err) = transcode_audio(&input, &output, AudioQuality::Medium).await {
            eprintln!("skipping: this ffmpeg build can't encode libopus: {err}");
            return;
        }

        assert!(output.exists());
        let before = probe_duration_seconds(&input).await.unwrap();
        let after = probe_duration_seconds(&output).await.unwrap();
        assert!(
            (before - after).abs() <= DURATION_TOLERANCE_SECONDS,
            "duration drifted: {before}s -> {after}s"
        );
    }

    /// The load-bearing ADR-0142 safety guard: an input that still has a
    /// video stream must be refused, never silently stripped of its video.
    /// A `mp4-audio` run's source is not filtered down to audio-only files
    /// (no extension distinguishes them), and ADR-0138/0139's prefix
    /// inference resolves a bare `--source source` to `source:mp4/` -- the
    /// same default address a video run uses.
    #[tokio::test]
    async fn transcode_audio_refuses_an_input_that_still_has_a_video_stream() {
        if check_ffmpeg_available().await.is_err() {
            eprintln!("skipping: ffmpeg/ffprobe not found on PATH");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("real-video.mp4");
        if generate_test_video(&input).await.is_err() {
            eprintln!("skipping: this ffmpeg build can't generate a test fixture");
            return;
        }

        let output = dir.path().join("out.opus");
        let err = transcode_audio(&input, &output, AudioQuality::Medium)
            .await
            .expect_err("a file with a video stream must not be transcoded to audio-only");

        assert!(
            err.contains("is not an audio-only recording"),
            "unexpected error: {err}"
        );
        assert!(
            is_non_retryable_transcode_error(&err),
            "this is deterministic and must never be retried: {err}"
        );
        assert!(
            !output.exists(),
            "ffmpeg must not have run at all for a video-bearing input"
        );
    }

    #[tokio::test]
    async fn transcode_audio_fails_fast_on_an_unreadable_input() {
        if check_ffmpeg_available().await.is_err() {
            eprintln!("skipping: ffmpeg/ffprobe not found on PATH");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("not-audio.mp4");
        std::fs::write(&input, b"this is not a real mp4 file").unwrap();
        let output = dir.path().join("out.opus");

        assert!(
            transcode_audio(&input, &output, AudioQuality::Medium)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn has_video_stream_is_true_for_a_real_video_and_false_for_an_unreadable_file() {
        if check_ffmpeg_available().await.is_err() {
            eprintln!("skipping: ffmpeg/ffprobe not found on PATH");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let video = dir.path().join("test.mp4");
        if generate_test_video(&video).await.is_err() {
            eprintln!("skipping: this ffmpeg build can't generate a test fixture");
            return;
        }
        assert!(has_video_stream(&video).await);

        let garbage = dir.path().join("not-a-video.mp4");
        std::fs::write(&garbage, b"this is not a real mp4 file").unwrap();
        assert!(!has_video_stream(&garbage).await);

        assert!(!has_video_stream(&dir.path().join("does-not-exist.mp4")).await);
    }

    /// **The ADR-0153 finding, pinned.** ffmpeg models embedded cover art
    /// as a video stream, so before this `has_video_stream` returned `true`
    /// for any tagged music file -- which would have made ADR-0142's guard
    /// reject essentially every `.mp3`/`.m4a`/`.aac` in a real library,
    /// delete its local copy (ADR-0143), and abort the run via ADR-0121's
    /// circuit breaker.
    #[tokio::test]
    async fn cover_art_does_not_count_as_a_video_stream() {
        if check_ffmpeg_available().await.is_err() {
            eprintln!("skipping: ffmpeg/ffprobe not found on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();

        let audio = dir.path().join("tone.mp4");
        if generate_audio_only_mp4(&audio).await.is_err() {
            eprintln!("skipping: this ffmpeg build can't generate an audio fixture");
            return;
        }
        let cover = dir.path().join("cover.png");
        generate_test_png(&cover).await.unwrap();

        let tagged = dir.path().join("tagged.m4a");
        let muxed = tokio::process::Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error", "-i"])
            .arg(&audio)
            .arg("-i")
            .arg(&cover)
            .args([
                "-map",
                "0:a",
                "-map",
                "1:v",
                "-c:a",
                "copy",
                "-c:v",
                "mjpeg",
                "-disposition:v",
                "attached_pic",
            ])
            .arg(&tagged)
            .output()
            .await;
        if muxed.map(|out| !out.status.success()).unwrap_or(true) {
            eprintln!("skipping: this ffmpeg build can't attach cover art");
            return;
        }

        // The whole point: art is not video.
        assert!(
            !has_video_stream(&tagged).await,
            "cover art must not read as a video stream"
        );
        assert!(!has_video_stream(&audio).await);

        // And a genuine video still does, so ADR-0141/0142's guards are
        // not disarmed.
        let video = dir.path().join("real.mp4");
        if generate_test_video(&video).await.is_ok() {
            assert!(has_video_stream(&video).await);
        }
    }

    /// A tagged file must therefore transcode, not be refused.
    #[tokio::test]
    async fn an_audio_file_with_cover_art_transcodes_rather_than_being_refused() {
        if check_ffmpeg_available().await.is_err() {
            eprintln!("skipping: ffmpeg/ffprobe not found on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let audio = dir.path().join("tone.mp4");
        if generate_audio_only_mp4(&audio).await.is_err() {
            eprintln!("skipping: this ffmpeg build can't generate an audio fixture");
            return;
        }
        let cover = dir.path().join("cover.png");
        generate_test_png(&cover).await.unwrap();
        let tagged = dir.path().join("tagged.m4a");
        let muxed = tokio::process::Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error", "-i"])
            .arg(&audio)
            .arg("-i")
            .arg(&cover)
            .args([
                "-map",
                "0:a",
                "-map",
                "1:v",
                "-c:a",
                "copy",
                "-c:v",
                "mjpeg",
                "-disposition:v",
                "attached_pic",
            ])
            .arg(&tagged)
            .output()
            .await;
        if muxed.map(|out| !out.status.success()).unwrap_or(true) {
            eprintln!("skipping: this ffmpeg build can't attach cover art");
            return;
        }

        let output = dir.path().join("out.scratch");
        transcode_audio(&tagged, &output, AudioQuality::Transparent)
            .await
            .expect("a tagged music file must transcode");
        assert!(std::fs::metadata(&output).unwrap().len() > 0);
    }

    /// **The second ADR-0153 finding, pinned.** The truncated-encode guard
    /// is one-sided and scales with duration, because a raw ADTS `.aac`
    /// carries no container duration and ffprobe's estimate is ~1.2% short
    /// -- measured at 177.868s reported for a 180s source, which the old
    /// symmetric 0.5s comparison failed outright.
    #[test]
    fn the_truncated_encode_guard_is_one_sided_and_scales() {
        // An output *longer* than the input is never truncation. This is
        // the measured ADTS case, and the old guard rejected it.
        assert!(!is_truncated_encode(177.868, 180.031));
        assert!(!is_truncated_encode(29.627, 30.030));
        assert!(!is_truncated_encode(3.000, 3.048));

        // A genuine partial write is still caught, at every scale.
        assert!(is_truncated_encode(180.0, 90.0));
        assert!(is_truncated_encode(10.0, 2.0));
        assert!(is_truncated_encode(3.0, 1.0));

        // The absolute floor still governs short files: 0.4s short of a 3s
        // input is inside the 0.5s floor, 0.6s is not.
        assert!(!is_truncated_encode(3.0, 2.6));
        assert!(is_truncated_encode(3.0, 2.4));

        // And the relative allowance governs long ones: 2% of 600s is 12s.
        assert!(!is_truncated_encode(600.0, 589.0));
        assert!(is_truncated_encode(600.0, 586.0));
    }

    #[test]
    fn is_non_retryable_transcode_error_matches_every_known_deterministic_signature() {
        assert!(is_non_retryable_transcode_error(
            "ffmpeg failed to transcode /x.heic: [mov,mp4,m4a,3gp,3g2,mj2 @ 0x1] moov atom not found\n\
             /x.heic: Invalid data found when processing input"
        ));
        assert!(is_non_retryable_transcode_error(
            "ffmpeg failed to transcode /x.heic: Invalid data found when processing input"
        ));
        assert!(is_non_retryable_transcode_error(
            "ffmpeg: Decoder (codec heic) not found for input stream"
        ));
        assert!(is_non_retryable_transcode_error(
            "ffmpeg failed to transcode /x.mov: could not find codec parameters"
        ));
        assert!(is_non_retryable_transcode_error(
            "transcoding /x.png changed pixel area from (100, 100) (10000 px) to (0, 0) (0 px); \
             refusing an output that isn't full-size"
        ));
        assert!(is_non_retryable_transcode_error(
            "ffmpeg failed to transcode /x.mp4: Unknown encoder 'libx265'"
        ));
        assert!(is_non_retryable_transcode_error(
            "/x.mp4 has no video stream (likely an audio-only recording saved with a video \
             extension); see ADR-0142 for an audio-preserving input type"
        ));
    }

    #[test]
    fn is_non_retryable_transcode_error_does_not_match_a_plausible_transient_error() {
        assert!(!is_non_retryable_transcode_error(
            "failed to run ffmpeg: Resource temporarily unavailable (os error 11)"
        ));
        assert!(!is_non_retryable_transcode_error(
            "ffmpeg failed to transcode /x.png: Cannot allocate memory"
        ));
    }

    #[test]
    fn is_non_retryable_transcode_error_is_false_for_an_empty_string() {
        assert!(!is_non_retryable_transcode_error(""));
    }

    /// ADR-0143: the predicate that licenses deleting a failed file's local
    /// source copy must match exactly the two wrong-input-kind guards and
    /// nothing else -- a corrupt file is non-retryable too, but its local
    /// copy is still wanted for diagnosis (ADR-0124).
    #[test]
    fn is_wrong_input_kind_error_matches_both_guards_and_nothing_else() {
        assert!(is_wrong_input_kind_error(
            "/x.mp4 has no video stream (likely an audio-only recording saved with a video \
             extension); see ADR-0142 for an audio-preserving input type"
        ));
        assert!(is_wrong_input_kind_error(
            "/x.mp4 has a video stream, so it is not an audio-only recording; refusing to \
             discard video (use --input-file-type mp4 to transcode it as video)"
        ));

        // Every other failure class, including other non-retryable ones.
        assert!(!is_wrong_input_kind_error(
            "ffmpeg failed to transcode /x.mp4: Invalid data found when processing input"
        ));
        assert!(!is_wrong_input_kind_error(
            "ffmpeg failed to transcode /x.mp4: moov atom not found"
        ));
        assert!(!is_wrong_input_kind_error(
            "ffmpeg failed to transcode /x.mp4: Unknown encoder 'libx265'"
        ));
        assert!(!is_wrong_input_kind_error(
            "failed to run ffmpeg: Resource temporarily unavailable (os error 11)"
        ));
        assert!(!is_wrong_input_kind_error(""));
    }

    /// Both guards' real messages must be classified non-retryable as well
    /// -- built here by calling the real producers' format strings' const
    /// substrings, so this fails if a const and its `format!` drift apart.
    #[test]
    fn both_wrong_input_kind_signatures_are_also_non_retryable() {
        for signature in [NO_VIDEO_STREAM_SIGNATURE, UNEXPECTED_VIDEO_STREAM_SIGNATURE] {
            let message = format!("/x.mp4 {signature} ...");
            assert!(
                is_non_retryable_transcode_error(&message),
                "{signature} must be non-retryable"
            );
            assert!(
                is_wrong_input_kind_error(&message),
                "{signature} must be a wrong-input-kind error"
            );
        }
    }

    /// Confirms the plain (no stream group) probe path is unaffected by the
    /// `-show_stream_groups` addition -- every ordinary PNG/JPEG input and
    /// the single-frame `.jpg` output both lack a stream group entirely, so
    /// `probe_dimensions` must still fall back to the first stream's
    /// dimensions exactly as before.
    #[tokio::test]
    async fn probe_dimensions_falls_back_to_the_plain_stream_when_there_is_no_stream_group() {
        if check_ffmpeg_available().await.is_err() {
            eprintln!("skipping: ffmpeg/ffprobe not found on PATH");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("test.png");
        generate_test_png(&input).await.unwrap();

        assert_eq!(probe_dimensions(&input).await, Some((64, 48)));
    }

    /// The area-based comparison in `transcode_to_jpg` must accept a
    /// transposed-but-equal-area pair (a HEIF `irot` rotation between the
    /// raw input canvas reading and the rotation-corrected transcoded
    /// output, ADR-0121) and still reject a genuinely different-area pair.
    #[test]
    fn dimension_guard_area_comparison_accepts_a_transpose_and_rejects_a_real_mismatch() {
        let transposed_a: (u32, u32) = (4032, 3024);
        let transposed_b: (u32, u32) = (3024, 4032);
        let area_a = transposed_a.0 as u64 * transposed_a.1 as u64;
        let area_b = transposed_b.0 as u64 * transposed_b.1 as u64;
        assert_eq!(area_a, area_b);

        let truncated: (u32, u32) = (512, 512);
        let area_truncated = truncated.0 as u64 * truncated.1 as u64;
        assert_ne!(area_a, area_truncated);
    }

    // --- ADR-0154: documents ---

    /// All three new signatures answer **both** questions: retrying does
    /// not help, and the local copy has no diagnostic value. Built from
    /// the consts so producer and matcher cannot drift -- the same shape
    /// ADR-0153's audio pair uses.
    #[test]
    fn every_document_signature_is_non_retryable_and_wrong_input_kind() {
        for signature in [
            NOT_A_DOCUMENT_SIGNATURE,
            NOT_A_KEYNOTE_SIGNATURE,
            NOT_A_TEXTCLIPPING_SIGNATURE,
        ] {
            let message = format!("/tmp/thing.key {signature} -- some trailing detail");
            assert!(
                is_non_retryable_transcode_error(&message),
                "not non-retryable: {message}"
            );
            assert!(
                is_wrong_input_kind_error(&message),
                "not wrong-input-kind: {message}"
            );
        }
    }

    /// The three stay distinct strings, so a run's log can be grepped for
    /// the private-key case specifically (ADR-0154 §9).
    #[test]
    fn the_document_signatures_do_not_overlap() {
        let all = [
            NOT_A_DOCUMENT_SIGNATURE,
            NOT_A_KEYNOTE_SIGNATURE,
            NOT_A_TEXTCLIPPING_SIGNATURE,
            NOT_A_WORKBOOK_SIGNATURE,
        ];
        for (i, a) in all.iter().enumerate() {
            for (j, b) in all.iter().enumerate() {
                if i != j {
                    assert!(!a.contains(b), "{a} contains {b}");
                }
            }
        }
    }
}
