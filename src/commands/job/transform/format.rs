//! `--input-file-type` (ADR-0112, extended to video by ADR-0122): a small,
//! vetted menu, not an arbitrary string -- mirrors
//! `pull_transform::media::ImageFormat`/`rclone::cli::RcloneAction`'s
//! precedent. Named `InputFileType`, not `ImageInputType`, and the job
//! itself named `transform`, not `image-transform`, so a later iteration
//! could add a non-image input kind to this same enum/command without a
//! rename -- ADR-0122's `Mov`/`M4v`/`Mp4` variants are that iteration.

/// Which input format this run transcodes. Image kinds (ADR-0112) always
/// output full-size, maximum-quality `.jpg`; video kinds (ADR-0122) always
/// output `.mp4` (H.265/HEVC) at the chosen `VideoQuality`. See
/// `output_extension`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputFileType {
    Png,
    Jpeg,
    Heic,
    Mov,
    M4v,
    Mp4,
}

impl InputFileType {
    /// The extension the bulk pull's `rclone --include` filter and the
    /// per-file pipeline's local validation both match against. Lowercase,
    /// no leading dot.
    pub(crate) fn extension(self) -> &'static str {
        match self {
            InputFileType::Png => "png",
            InputFileType::Jpeg => "jpeg",
            InputFileType::Heic => "heic",
            InputFileType::Mov => "mov",
            InputFileType::M4v => "m4v",
            InputFileType::Mp4 => "mp4",
        }
    }

    /// Case-insensitive; `jpg` is accepted as an alias for `jpeg` (mirrors
    /// `pull_transform::media::ImageFormat::parse`'s own leniency). Any
    /// other value is a hard error -- an invalid `--input-file-type` never
    /// falls through to an interactive prompt.
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value.to_ascii_lowercase().as_str() {
            "png" => Ok(InputFileType::Png),
            "jpeg" | "jpg" => Ok(InputFileType::Jpeg),
            "heic" => Ok(InputFileType::Heic),
            "mov" => Ok(InputFileType::Mov),
            "m4v" => Ok(InputFileType::M4v),
            "mp4" => Ok(InputFileType::Mp4),
            other => Err(format!(
                "unknown --input-file-type '{other}' (expected png, jpeg, heic, mov, m4v, or mp4)"
            )),
        }
    }

    /// Video kinds are always recompressed (ADR-0122) -- never
    /// copy-through, unlike `Jpeg` -- so this job's one justified departure
    /// from "preserve what's already in the target format" is explicit here
    /// rather than implied by a `match` elsewhere.
    pub(crate) fn is_video(self) -> bool {
        matches!(
            self,
            InputFileType::Mov | InputFileType::M4v | InputFileType::Mp4
        )
    }

    /// The fixed output extension for this input kind: `jpg` for every image
    /// kind (ADR-0112), `mp4` for every video kind (ADR-0122) regardless of
    /// which video extension was the input.
    pub(crate) fn output_extension(self) -> &'static str {
        if self.is_video() { "mp4" } else { "jpg" }
    }
}

impl std::fmt::Display for InputFileType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.extension())
    }
}

/// `--video-quality` (ADR-0122): a small, vetted CRF menu for `libx265`,
/// mirroring ADR-0077's per-category menu precedent -- never raw CRF/bitrate
/// exposed directly. Only consulted when `InputFileType::is_video()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VideoQuality {
    Low,
    Medium,
    High,
    Lossless,
}

impl VideoQuality {
    /// `libx265` CRF for this tier, or `None` for `Lossless`, which uses
    /// `-x265-params lossless=1` instead of a CRF value (see `media.rs`).
    /// `Medium`'s `24` is deliberately tuned a bit better than the
    /// conventional H.265 "medium" of ~26-28 -- this project's own stated
    /// preference for personal archival video, not a generic default.
    pub(crate) fn crf(self) -> Option<u8> {
        match self {
            VideoQuality::Low => Some(30),
            VideoQuality::Medium => Some(24),
            VideoQuality::High => Some(20),
            VideoQuality::Lossless => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            VideoQuality::Low => "low",
            VideoQuality::Medium => "medium",
            VideoQuality::High => "high",
            VideoQuality::Lossless => "lossless",
        }
    }

    /// Case-insensitive; any other value is a hard error -- same discipline
    /// as `InputFileType::parse`.
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value.to_ascii_lowercase().as_str() {
            "low" => Ok(VideoQuality::Low),
            "medium" => Ok(VideoQuality::Medium),
            "high" => Ok(VideoQuality::High),
            "lossless" => Ok(VideoQuality::Lossless),
            other => Err(format!(
                "unknown --video-quality '{other}' (expected low, medium, high, or lossless)"
            )),
        }
    }
}

impl std::fmt::Display for VideoQuality {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_every_canonical_value_case_insensitively() {
        assert_eq!(InputFileType::parse("png").unwrap(), InputFileType::Png);
        assert_eq!(InputFileType::parse("PNG").unwrap(), InputFileType::Png);
        assert_eq!(InputFileType::parse("jpeg").unwrap(), InputFileType::Jpeg);
        assert_eq!(InputFileType::parse("JPEG").unwrap(), InputFileType::Jpeg);
        assert_eq!(InputFileType::parse("heic").unwrap(), InputFileType::Heic);
        assert_eq!(InputFileType::parse("HEIC").unwrap(), InputFileType::Heic);
        assert_eq!(InputFileType::parse("mov").unwrap(), InputFileType::Mov);
        assert_eq!(InputFileType::parse("MOV").unwrap(), InputFileType::Mov);
        assert_eq!(InputFileType::parse("m4v").unwrap(), InputFileType::M4v);
        assert_eq!(InputFileType::parse("M4V").unwrap(), InputFileType::M4v);
        assert_eq!(InputFileType::parse("mp4").unwrap(), InputFileType::Mp4);
        assert_eq!(InputFileType::parse("MP4").unwrap(), InputFileType::Mp4);
    }

    #[test]
    fn parse_accepts_jpg_as_an_alias_for_jpeg() {
        assert_eq!(InputFileType::parse("jpg").unwrap(), InputFileType::Jpeg);
        assert_eq!(InputFileType::parse("JPG").unwrap(), InputFileType::Jpeg);
    }

    #[test]
    fn parse_rejects_anything_else() {
        assert!(InputFileType::parse("gif").is_err());
        assert!(InputFileType::parse("").is_err());
    }

    #[test]
    fn extension_and_display_agree() {
        for kind in [
            InputFileType::Png,
            InputFileType::Jpeg,
            InputFileType::Heic,
            InputFileType::Mov,
            InputFileType::M4v,
            InputFileType::Mp4,
        ] {
            assert_eq!(kind.extension(), kind.to_string());
        }
    }

    #[test]
    fn is_video_is_true_only_for_video_kinds() {
        assert!(!InputFileType::Png.is_video());
        assert!(!InputFileType::Jpeg.is_video());
        assert!(!InputFileType::Heic.is_video());
        assert!(InputFileType::Mov.is_video());
        assert!(InputFileType::M4v.is_video());
        assert!(InputFileType::Mp4.is_video());
    }

    #[test]
    fn output_extension_is_jpg_for_image_kinds_and_mp4_for_video_kinds() {
        assert_eq!(InputFileType::Png.output_extension(), "jpg");
        assert_eq!(InputFileType::Jpeg.output_extension(), "jpg");
        assert_eq!(InputFileType::Heic.output_extension(), "jpg");
        assert_eq!(InputFileType::Mov.output_extension(), "mp4");
        assert_eq!(InputFileType::M4v.output_extension(), "mp4");
        assert_eq!(InputFileType::Mp4.output_extension(), "mp4");
    }

    #[test]
    fn video_quality_parse_accepts_every_canonical_value_case_insensitively() {
        assert_eq!(VideoQuality::parse("low").unwrap(), VideoQuality::Low);
        assert_eq!(VideoQuality::parse("LOW").unwrap(), VideoQuality::Low);
        assert_eq!(VideoQuality::parse("medium").unwrap(), VideoQuality::Medium);
        assert_eq!(VideoQuality::parse("high").unwrap(), VideoQuality::High);
        assert_eq!(
            VideoQuality::parse("lossless").unwrap(),
            VideoQuality::Lossless
        );
    }

    #[test]
    fn video_quality_parse_rejects_anything_else() {
        assert!(VideoQuality::parse("ultra").is_err());
        assert!(VideoQuality::parse("").is_err());
    }

    #[test]
    fn video_quality_crf_is_none_only_for_lossless() {
        assert_eq!(VideoQuality::Low.crf(), Some(30));
        assert_eq!(VideoQuality::Medium.crf(), Some(24));
        assert_eq!(VideoQuality::High.crf(), Some(20));
        assert_eq!(VideoQuality::Lossless.crf(), None);
    }

    #[test]
    fn video_quality_as_str_and_display_agree() {
        for quality in [
            VideoQuality::Low,
            VideoQuality::Medium,
            VideoQuality::High,
            VideoQuality::Lossless,
        ] {
            assert_eq!(quality.as_str(), quality.to_string());
        }
    }
}
