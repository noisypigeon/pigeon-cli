//! ffmpeg/ffprobe-backed media handling: probing, recoding, and
//! post-recode verification (ADR-0074 §4), plus EXIF date extraction and
//! the screenshot-vs-photo dimension heuristic. Real audio/video re-encoding
//! has no mature pure-Rust implementation, so this shells out to the
//! `ffmpeg`/`ffprobe` binaries rather than adding a codec crate.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use serde::Deserialize;

use super::date::{SimpleDate, parse_exif_datetime, parse_iso_date};

/// What canonical extension/encoding a classified media file recodes to.
/// The photo/screenshot, video, and audio targets are each independently
/// adaptable (ADR-0077) via `TranscodeTargets`; `canonical_extension` takes
/// the resolved targets rather than hardcoding jpg/mp4/m4a.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MediaKind {
    Photo,
    Screenshot,
    Video,
    Audio,
}

impl MediaKind {
    pub(crate) fn canonical_extension(self, targets: &TranscodeTargets) -> &'static str {
        match self {
            MediaKind::Photo | MediaKind::Screenshot => targets.image.extension(),
            MediaKind::Video => targets.video.extension(),
            MediaKind::Audio => targets.audio.extension(),
        }
    }
}

/// A small, vetted menu of alternative image-recode targets (ADR-0077) --
/// deliberately not arbitrary user-supplied extensions/ffmpeg args, so no
/// combination the user can choose is untested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImageFormat {
    Jpg,
    Png,
}

impl ImageFormat {
    pub(crate) fn extension(self) -> &'static str {
        match self {
            ImageFormat::Jpg => "jpg",
            ImageFormat::Png => "png",
        }
    }

    pub(crate) fn all() -> &'static [ImageFormat] {
        &[ImageFormat::Jpg, ImageFormat::Png]
    }

    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value.to_ascii_lowercase().as_str() {
            "jpg" | "jpeg" => Ok(ImageFormat::Jpg),
            "png" => Ok(ImageFormat::Png),
            other => Err(format!(
                "unknown image format '{other}' (expected jpg or png)"
            )),
        }
    }
}

impl std::fmt::Display for ImageFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.extension())
    }
}

/// A small, vetted menu of alternative video-recode targets (ADR-0077).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VideoFormat {
    Mp4,
    Mkv,
    Webm,
}

impl VideoFormat {
    pub(crate) fn extension(self) -> &'static str {
        match self {
            VideoFormat::Mp4 => "mp4",
            VideoFormat::Mkv => "mkv",
            VideoFormat::Webm => "webm",
        }
    }

    pub(crate) fn all() -> &'static [VideoFormat] {
        &[VideoFormat::Mp4, VideoFormat::Mkv, VideoFormat::Webm]
    }

    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value.to_ascii_lowercase().as_str() {
            "mp4" => Ok(VideoFormat::Mp4),
            "mkv" => Ok(VideoFormat::Mkv),
            "webm" => Ok(VideoFormat::Webm),
            other => Err(format!(
                "unknown video format '{other}' (expected mp4, mkv, or webm)"
            )),
        }
    }
}

impl std::fmt::Display for VideoFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.extension())
    }
}

/// A small, vetted menu of alternative audio-recode targets (ADR-0077).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AudioFormat {
    M4a,
    Mp3,
    Flac,
}

impl AudioFormat {
    pub(crate) fn extension(self) -> &'static str {
        match self {
            AudioFormat::M4a => "m4a",
            AudioFormat::Mp3 => "mp3",
            AudioFormat::Flac => "flac",
        }
    }

    pub(crate) fn all() -> &'static [AudioFormat] {
        &[AudioFormat::M4a, AudioFormat::Mp3, AudioFormat::Flac]
    }

    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value.to_ascii_lowercase().as_str() {
            "m4a" => Ok(AudioFormat::M4a),
            "mp3" => Ok(AudioFormat::Mp3),
            "flac" => Ok(AudioFormat::Flac),
            other => Err(format!(
                "unknown audio format '{other}' (expected m4a, mp3, or flac)"
            )),
        }
    }
}

impl std::fmt::Display for AudioFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.extension())
    }
}

/// The resolved recode target for each media category -- confirmed/adapted
/// once per run in the wizard (ADR-0077), never persisted. `Default`
/// reproduces the exact mapping this job used before ADR-0077 existed.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TranscodeTargets {
    pub image: ImageFormat,
    pub video: VideoFormat,
    pub audio: AudioFormat,
}

impl Default for TranscodeTargets {
    fn default() -> Self {
        TranscodeTargets {
            image: ImageFormat::Jpg,
            video: VideoFormat::Mp4,
            audio: AudioFormat::M4a,
        }
    }
}

/// Structural facts about a media file, probed both before a recode (to
/// know what "no loss" means for this specific file) and after (to check
/// against it).
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ProbeInfo {
    pub duration_secs: Option<f64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub creation_date: Option<SimpleDate>,
}

#[derive(Deserialize)]
struct FfprobeOutput {
    #[serde(default)]
    format: FfprobeFormat,
    #[serde(default)]
    streams: Vec<FfprobeStream>,
}

#[derive(Deserialize, Default)]
struct FfprobeFormat {
    duration: Option<String>,
    #[serde(default)]
    tags: FfprobeTags,
}

#[derive(Deserialize, Default)]
struct FfprobeTags {
    creation_time: Option<String>,
}

#[derive(Deserialize, Default)]
struct FfprobeStream {
    width: Option<u32>,
    height: Option<u32>,
}

/// Confirms `ffmpeg`/`ffprobe` are on `PATH` -- checked once, up front in
/// the wizard, so a missing binary fails the whole job immediately with one
/// clear error instead of failing per-file deep into a long run.
pub(crate) async fn check_ffmpeg_available() -> Result<(), String> {
    for binary in ["ffmpeg", "ffprobe"] {
        tokio::process::Command::new(binary)
            .arg("-version")
            .output()
            .await
            .map_err(|_| {
                format!(
                    "'{binary}' was not found on PATH -- required for \
                     'pigeon job run pull-transform' to recode media"
                )
            })?;
    }
    Ok(())
}

/// Probes `path` (any file `ffprobe` can open) for duration and, if it has a
/// video/image stream, pixel dimensions -- used both to classify
/// screenshots/photos by resolution and to verify a recode didn't
/// truncate/resize the content.
pub(crate) async fn probe(path: &Path) -> Result<ProbeInfo, String> {
    let output = tokio::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(path)
        .output()
        .await
        .map_err(|err| format!("failed to run ffprobe: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "ffprobe failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let parsed: FfprobeOutput = serde_json::from_slice(&output.stdout).map_err(|err| {
        format!(
            "failed to parse ffprobe output for {}: {err}",
            path.display()
        )
    })?;

    let duration_secs = parsed.format.duration.and_then(|value| value.parse().ok());
    let (width, height) = parsed
        .streams
        .iter()
        .find(|stream| stream.width.is_some() && stream.height.is_some())
        .map(|stream| (stream.width, stream.height))
        .unwrap_or((None, None));
    let creation_date = parsed
        .format
        .tags
        .creation_time
        .as_deref()
        .and_then(parse_iso_date);

    Ok(ProbeInfo {
        duration_secs,
        width,
        height,
        creation_date,
    })
}

/// Re-encodes `input_path` into `output_path` at a fixed, conservative
/// "visually lossless" preset for `kind`'s resolved `targets` entry
/// (ADR-0074: not a dynamic per-file quality search; ADR-0077: the target
/// per category is adaptable, but each option's args are still fixed and
/// pre-vetted, never user-supplied). Audio already in the target's own
/// container/codec is a cheap remux (`-c:a copy`), not a re-encode, since
/// there's nothing to gain from touching already-matching audio.
pub(crate) async fn recode(
    input_path: &Path,
    output_path: &Path,
    kind: MediaKind,
    targets: &TranscodeTargets,
    input_already_matches_target: bool,
) -> Result<(), String> {
    let mut command = tokio::process::Command::new("ffmpeg");
    command
        .args(["-y", "-loglevel", "error", "-i"])
        .arg(input_path);
    match kind {
        MediaKind::Photo | MediaKind::Screenshot => match targets.image {
            ImageFormat::Jpg => {
                command.args(["-frames:v", "1", "-q:v", "3"]);
            }
            ImageFormat::Png => {
                command.args(["-frames:v", "1"]);
            }
        },
        MediaKind::Video => match targets.video {
            VideoFormat::Mp4 | VideoFormat::Mkv => {
                command.args([
                    "-c:v", "libx264", "-preset", "medium", "-crf", "23", "-c:a", "aac", "-b:a",
                    "256k",
                ]);
            }
            VideoFormat::Webm => {
                command.args([
                    "-c:v",
                    "libvpx-vp9",
                    "-crf",
                    "32",
                    "-b:v",
                    "0",
                    "-c:a",
                    "libopus",
                    "-b:a",
                    "192k",
                ]);
            }
        },
        MediaKind::Audio if input_already_matches_target => {
            command.args(["-c:a", "copy"]);
        }
        MediaKind::Audio => match targets.audio {
            AudioFormat::M4a => {
                command.args(["-c:a", "aac", "-b:a", "256k"]);
            }
            AudioFormat::Mp3 => {
                command.args(["-c:a", "libmp3lame", "-b:a", "256k"]);
            }
            AudioFormat::Flac => {
                command.args(["-c:a", "flac"]);
            }
        },
    }
    command.arg(output_path);

    let output = command
        .output()
        .await
        .map_err(|err| format!("failed to run ffmpeg: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "ffmpeg failed to recode {}: {}",
            input_path.display(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

/// Duration tolerance for verification: the larger of 2% or half a second,
/// so a recode's container-level rounding never registers as a mismatch on
/// short clips while still catching a truncated/corrupted re-encode of a
/// long one.
fn duration_tolerance(seconds: f64) -> f64 {
    (seconds * 0.02).max(0.5)
}

/// Compares `before` (probed pre-recode) against `after_path` (probed
/// post-recode): duration within tolerance, dimensions exact when both are
/// known. Anything outside that is treated as data loss, not just "a
/// different encode" -- the caller retries, then falls back to the
/// original file (ADR-0074 §4).
pub(crate) async fn verify(before: ProbeInfo, after_path: &Path) -> Result<(), String> {
    let after = probe(after_path).await?;
    if let (Some(b), Some(a)) = (before.duration_secs, after.duration_secs) {
        let tolerance = duration_tolerance(b);
        if (b - a).abs() > tolerance {
            return Err(format!(
                "duration mismatch after recode: {b:.2}s before, {a:.2}s after (tolerance {tolerance:.2}s)"
            ));
        }
    }
    if let (Some(bw), Some(bh), Some(aw), Some(ah)) =
        (before.width, before.height, after.width, after.height)
        && (bw, bh) != (aw, ah)
    {
        return Err(format!(
            "dimension mismatch after recode: {bw}x{bh} before, {aw}x{ah} after"
        ));
    }
    Ok(())
}

/// Reads EXIF `DateTimeOriginal` from an image file on disk (JPEG/TIFF/
/// HEIF/PNG/WebP -- `kamadak-exif` auto-detects the container and only
/// reads as much of the file as each format actually needs, e.g. just the
/// leading segments for JPEG). Reads from a real file rather than an
/// in-memory buffer (ADR-0076) -- images are read straight off disk now,
/// never held in memory as a whole `Vec<u8>`. Absent for most screenshots
/// (no camera wrote EXIF into them) and for images with no EXIF segment at
/// all -- `None` either way, letting the caller fall through to its next
/// date source.
pub(crate) fn exif_date(path: &Path) -> Option<SimpleDate> {
    let file = File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let exif = exif::Reader::new().read_from_container(&mut reader).ok()?;
    let field = exif.get_field(exif::Tag::DateTimeOriginal, exif::In::PRIMARY)?;
    parse_exif_datetime(&field.display_value().to_string())
}

/// Common device/monitor screen resolutions (width, height as captured,
/// either orientation) -- an image whose dimensions match one of these is
/// classified a screenshot rather than a photo (ADR-0074's chosen
/// heuristic). Not exhaustive by design: a miss just means "treated as a
/// photo," never a lost or corrupted file.
const COMMON_SCREEN_RESOLUTIONS: &[(u32, u32)] = &[
    (750, 1334),  // iPhone SE/8
    (828, 1792),  // iPhone 11/XR
    (1080, 1920), // common Android/1080p portrait
    (1125, 2436), // iPhone X/XS/11 Pro
    (1170, 2532), // iPhone 12/13
    (1179, 2556), // iPhone 15/16
    (1206, 2622), // iPhone 15/16 Pro Max
    (1284, 2778), // iPhone 12/13 Pro Max
    (1290, 2796), // iPhone 15/16 Pro Max variant
    (1440, 2960), // Galaxy S-series
    (1440, 3200), // Galaxy S20+/Note
    (2048, 1536), // iPad (4:3)
    (2224, 1668), // iPad Pro 10.5"
    (2360, 1640), // iPad Air
    (2732, 2048), // iPad Pro 12.9"
    (1920, 1080), // 1080p monitor
    (2560, 1440), // 1440p monitor
    (3840, 2160), // 4K monitor
    (1366, 768),  // common laptop
    (2560, 1600), // MacBook-class
    (2880, 1800), // MacBook Pro Retina
    (3024, 1964), // MacBook Pro 14"
    (3456, 2234), // MacBook Pro 16"
];

pub(crate) fn is_screenshot_resolution(width: u32, height: u32) -> bool {
    COMMON_SCREEN_RESOLUTIONS
        .iter()
        .any(|&(w, h)| (w, h) == (width, height) || (w, h) == (height, width))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_extension_maps_each_category_to_the_default_targets() {
        let targets = TranscodeTargets::default();
        assert_eq!(MediaKind::Photo.canonical_extension(&targets), "jpg");
        assert_eq!(MediaKind::Screenshot.canonical_extension(&targets), "jpg");
        assert_eq!(MediaKind::Video.canonical_extension(&targets), "mp4");
        assert_eq!(MediaKind::Audio.canonical_extension(&targets), "m4a");
    }

    #[test]
    fn canonical_extension_follows_adapted_targets() {
        let targets = TranscodeTargets {
            image: ImageFormat::Png,
            video: VideoFormat::Mkv,
            audio: AudioFormat::Mp3,
        };
        assert_eq!(MediaKind::Photo.canonical_extension(&targets), "png");
        assert_eq!(MediaKind::Video.canonical_extension(&targets), "mkv");
        assert_eq!(MediaKind::Audio.canonical_extension(&targets), "mp3");
    }

    #[test]
    fn image_format_parse_accepts_jpg_jpeg_and_png() {
        assert_eq!(ImageFormat::parse("jpg").unwrap(), ImageFormat::Jpg);
        assert_eq!(ImageFormat::parse("JPEG").unwrap(), ImageFormat::Jpg);
        assert_eq!(ImageFormat::parse("png").unwrap(), ImageFormat::Png);
        assert!(ImageFormat::parse("gif").is_err());
    }

    #[test]
    fn video_format_parse_accepts_mp4_mkv_and_webm() {
        assert_eq!(VideoFormat::parse("mp4").unwrap(), VideoFormat::Mp4);
        assert_eq!(VideoFormat::parse("MKV").unwrap(), VideoFormat::Mkv);
        assert_eq!(VideoFormat::parse("webm").unwrap(), VideoFormat::Webm);
        assert!(VideoFormat::parse("avi").is_err());
    }

    #[test]
    fn audio_format_parse_accepts_m4a_mp3_and_flac() {
        assert_eq!(AudioFormat::parse("m4a").unwrap(), AudioFormat::M4a);
        assert_eq!(AudioFormat::parse("MP3").unwrap(), AudioFormat::Mp3);
        assert_eq!(AudioFormat::parse("flac").unwrap(), AudioFormat::Flac);
        assert!(AudioFormat::parse("wav").is_err());
    }

    #[test]
    fn transcode_targets_default_matches_the_pre_adr_0077_mapping() {
        let targets = TranscodeTargets::default();
        assert_eq!(targets.image, ImageFormat::Jpg);
        assert_eq!(targets.video, VideoFormat::Mp4);
        assert_eq!(targets.audio, AudioFormat::M4a);
    }

    #[test]
    fn is_screenshot_resolution_matches_known_phone_size() {
        assert!(is_screenshot_resolution(1170, 2532));
        assert!(is_screenshot_resolution(2532, 1170));
    }

    #[test]
    fn is_screenshot_resolution_false_for_typical_camera_photo() {
        // A common DSLR/phone-camera photo resolution, not a screen size.
        assert!(!is_screenshot_resolution(4032, 3024));
    }

    #[test]
    fn duration_tolerance_has_a_floor_for_short_clips() {
        assert_eq!(duration_tolerance(1.0), 0.5);
    }

    #[test]
    fn duration_tolerance_scales_for_long_clips() {
        assert!((duration_tolerance(100.0) - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn exif_date_returns_none_for_non_image_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-an-image.jpg");
        std::fs::write(&path, b"not an image").unwrap();
        assert_eq!(exif_date(&path), None);
    }

    #[test]
    fn exif_date_returns_none_for_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(exif_date(&dir.path().join("does-not-exist.jpg")), None);
    }

    /// Generates a tiny synthetic clip via `ffmpeg`'s `lavfi` test-source
    /// input -- real regression coverage for the `probe`/`recode`/`verify`
    /// trio against an actual `ffmpeg` invocation, not just the pure
    /// parsing/heuristic functions above. Skipped (not failed) if `ffmpeg`
    /// genuinely isn't on `PATH`, since this whole job already refuses to
    /// run without it (`check_ffmpeg_available`) -- this test would just be
    /// redundant with that failure on a machine that can't run it anyway.
    async fn generate_test_clip(dir: &Path, name: &str) -> Option<std::path::PathBuf> {
        if check_ffmpeg_available().await.is_err() {
            eprintln!("skipping: ffmpeg/ffprobe not found on PATH");
            return None;
        }
        let path = dir.join(name);
        let output = tokio::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:duration=1:rate=10",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&path)
            .output()
            .await
            .expect("failed to spawn ffmpeg");
        assert!(
            output.status.success(),
            "ffmpeg fixture generation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Some(path)
    }

    #[tokio::test]
    async fn probe_reports_dimensions_and_duration_for_a_real_clip() {
        let dir = tempfile::tempdir().unwrap();
        let Some(clip) = generate_test_clip(dir.path(), "clip.mov").await else {
            return;
        };

        let info = probe(&clip).await.unwrap();

        assert_eq!(info.width, Some(320));
        assert_eq!(info.height, Some(240));
        assert!(info.duration_secs.unwrap() > 0.0);
    }

    #[tokio::test]
    async fn recode_then_verify_round_trips_a_real_video() {
        let dir = tempfile::tempdir().unwrap();
        let Some(clip) = generate_test_clip(dir.path(), "clip.mov").await else {
            return;
        };
        let before = probe(&clip).await.unwrap();
        let output_path = dir.path().join("out.mp4");

        recode(
            &clip,
            &output_path,
            MediaKind::Video,
            &TranscodeTargets::default(),
            false,
        )
        .await
        .expect("recode should succeed");
        verify(before, &output_path)
            .await
            .expect("recoded output should verify against the original");
    }
}
