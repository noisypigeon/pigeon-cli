//! `--input-file-type` (ADR-0112): a small, vetted menu, not an arbitrary
//! string -- mirrors `pull_transform::media::ImageFormat`/
//! `rclone::cli::RcloneAction`'s precedent. Named `InputFileType`, not
//! `ImageInputType`, and the job itself named `transform`, not
//! `image-transform`, so a later iteration can add a non-image input kind
//! to this same enum/command without a rename.

/// Which input format this run transcodes. Output is always full-size
/// `.jpg` in this first iteration (ADR-0112) -- there is no corresponding
/// output-type enum yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputFileType {
    Png,
    Jpeg,
    Heic,
}

impl InputFileType {
    /// The extension Phase A's `rclone --include` filter and Phase B's
    /// local validation both match against. Lowercase, no leading dot.
    pub(crate) fn extension(self) -> &'static str {
        match self {
            InputFileType::Png => "png",
            InputFileType::Jpeg => "jpeg",
            InputFileType::Heic => "heic",
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
            other => Err(format!(
                "unknown --input-file-type '{other}' (expected png, jpeg, or heic)"
            )),
        }
    }
}

impl std::fmt::Display for InputFileType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.extension())
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
        for kind in [InputFileType::Png, InputFileType::Jpeg, InputFileType::Heic] {
            assert_eq!(kind.extension(), kind.to_string());
        }
    }
}
