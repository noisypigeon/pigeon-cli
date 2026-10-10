//! `ffmpeg`/`ffprobe`-backed png/heic -> jpg transcoding (ADR-0112 Decision
//! §4), and the jpeg copy-through path that skips both entirely. Unlike
//! `pull_transform::media`'s `-q:v 3` default (tuned for screenshots, not
//! "no quality loss"), this always uses ffmpeg's highest JPEG quality
//! setting and forces 4:4:4 chroma, with no resize filter -- dimensions are
//! therefore preserved by construction, and `transcode_to_jpg` additionally
//! confirms that with a cheap post-encode `ffprobe` check as a defensive
//! guard against an unexpected ffmpeg default silently resizing the image.

use std::path::Path;

/// Confirms `ffmpeg`/`ffprobe` are on `PATH`, checked once up front before
/// any prompts (mirrors `pull_transform::media::check_ffmpeg_available`), so
/// a missing binary fails the whole job immediately instead of partway
/// through a long Phase B.
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

/// Pixel dimensions of `path`'s first video/image stream, or `None` if
/// `ffprobe` can't determine them (e.g. a format it can't open at all) --
/// treated as "skip the check," not an error, since this is a defensive
/// guard, not the primary correctness mechanism (no resize filter is ever
/// applied, so dimensions are already preserved by construction).
async fn probe_dimensions(path: &Path) -> Option<(u32, u32)> {
    let output = tokio::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut parts = text.trim().split(',');
    let width = parts.next()?.parse::<u32>().ok()?;
    let height = parts.next()?.parse::<u32>().ok()?;
    Some((width, height))
}

/// Transcodes `input` (png/heic) to `output` (always `.jpg`):
/// ```text
/// ffmpeg -y -loglevel error -i <input> -frames:v 1 -q:v 1 -pix_fmt yuvj444p <output>
/// ```
/// `-q:v 1` is ffmpeg's mjpeg encoder's highest quality setting (1=best ..
/// 31=worst); `-pix_fmt yuvj444p` forces 4:4:4 chroma instead of ffmpeg's
/// default 4:2:0 subsampling for JPEG output -- the single largest avoidable
/// quality loss in a naive encode beyond quantization itself. No `-vf
/// scale=...` or other filter is ever applied, preserving the original
/// pixel dimensions exactly. A non-zero `ffmpeg` exit -- including a HEIC
/// input on a `libheif`-less build, which fails immediately with ffmpeg's
/// own decoder-missing message -- returns `Err` with that stderr folded in
/// verbatim; this *is* the fail-fast trigger (ADR-0112 Decision §7), there
/// is no separate HEIC-capability preflight probe.
pub(crate) async fn transcode_to_jpg(input: &Path, output: &Path) -> Result<(), String> {
    let result = tokio::process::Command::new("ffmpeg")
        .args(["-y", "-loglevel", "error"])
        .arg("-i")
        .arg(input)
        .args(["-frames:v", "1", "-q:v", "1", "-pix_fmt", "yuvj444p"])
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
    ) && before != after
    {
        return Err(format!(
            "transcoding {} changed dimensions from {before:?} to {after:?}; \
             refusing an output that isn't full-size",
            input.display()
        ));
    }

    Ok(())
}

/// `jpeg` input is never routed through ffmpeg -- a plain byte-for-byte
/// `fs::copy`, zero generational loss (ADR-0112 Decision §4).
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
}
