//! `--input-file-type` (ADR-0112, extended to video by ADR-0122): a small,
//! vetted menu, not an arbitrary string -- mirrors
//! `rclone::cli::RcloneAction`'s precedent. Named `InputFileType`, not
//! `ImageInputType`, and the job
//! itself named `transform`, not `image-transform`, so a later iteration
//! could add a non-image input kind to this same enum/command without a
//! rename -- ADR-0122's `Mov`/`M4v`/`Mp4` variants are that iteration.

/// Which input format this run transcodes. Image kinds (ADR-0112) always
/// output full-size, maximum-quality `.jpg`; video kinds (ADR-0122) always
/// output `.mp4` (H.265/HEVC) at the chosen `VideoQuality`; `Mp4Audio`
/// (ADR-0142) outputs `.opus`. See `output_extension`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputFileType {
    Png,
    Jpeg,
    Heic,
    Mov,
    M4v,
    Mp4,
    /// An audio-only recording carrying an `.mp4` extension (ADR-0142) --
    /// a voice note, typically. Shares `Mp4`'s on-disk spelling because no
    /// extension distinguishes the two; `media::transcode_audio`'s
    /// has-a-video-stream guard is what makes selecting this kind safe.
    Mp4Audio,
    /// Apple Numbers. The only kind needing an external converter other
    /// than `ffmpeg` (ADR-0149): no Rust crate reads Apple's IWA format, so
    /// `sheets::convert_numbers_to_xlsx` shells out to `soffice` and the
    /// result rejoins the shared `calamine` path.
    Numbers,
    Xlsx,
    Xls,
    /// Already CSV: copied through byte-for-byte, the same reasoning
    /// ADR-0112 applies to a `jpeg` input already in the target format.
    Csv,
}

impl InputFileType {
    /// Every on-disk spelling this input kind matches -- what the source
    /// listing's `rclone --include` filters and the per-file pipeline's
    /// local validation both check against. Lowercase, no leading dot.
    ///
    /// A kind may have more than one spelling (ADR-0139): `.jpg` and
    /// `.jpeg` are the same format, and `parse` has accepted both as
    /// *input* since ADR-0112 -- this is that same leniency finally applied
    /// to file *matching*, which it never reached. A run against a
    /// directory of `.jpg` files with `--input-file-type=jpeg` silently
    /// matched nothing until this returned a set rather than one string.
    ///
    /// Only `Jpeg` has a second spelling; every other kind keeps exactly
    /// one, deliberately unspeculative -- a new alias gets added when a
    /// real run is observed to miss files, not in anticipation.
    pub(crate) fn extensions(self) -> &'static [&'static str] {
        match self {
            InputFileType::Png => &["png"],
            InputFileType::Jpeg => &["jpg", "jpeg"],
            InputFileType::Heic => &["heic"],
            InputFileType::Mov => &["mov"],
            InputFileType::M4v => &["m4v"],
            InputFileType::Mp4 | InputFileType::Mp4Audio => &["mp4"],
            InputFileType::Numbers => &["numbers"],
            InputFileType::Xlsx => &["xlsx"],
            InputFileType::Xls => &["xls"],
            InputFileType::Csv => &["csv"],
        }
    }

    /// The one spelling used for a default bucket prefix (ADR-0138's
    /// `--source`/`--destination` inference) -- never for matching, which
    /// always considers every entry in `extensions()`, and (since
    /// ADR-0142) never for display either, which uses `name()`.
    ///
    /// For `Jpeg` this is `jpg`, not `jpeg` (ADR-0139): it agrees with
    /// `output_extension()`, which has always returned `jpg` for this
    /// kind, and with ADR-0112's own documented `source:png/` ->
    /// `destination:jpg/` example.
    pub(crate) fn canonical_extension(self) -> &'static str {
        self.extensions()[0]
    }

    /// This kind's `--input-file-type` spelling, used for display.
    ///
    /// Distinct from `canonical_extension()` since ADR-0142: the two
    /// coincided for every kind until `Mp4Audio`, which is addressed by
    /// the `mp4` prefix its files actually live under but must *display*
    /// as `mp4-audio` -- otherwise the wizard's "Input file type" echo
    /// would read `mp4` and be indistinguishable from a video run. Every
    /// other kind's spelling here is its `canonical_extension()`,
    /// preserving ADR-0139's decision that `Jpeg` displays as `jpg`.
    pub(crate) fn name(self) -> &'static str {
        match self {
            InputFileType::Mp4Audio => "mp4-audio",
            InputFileType::Png
            | InputFileType::Jpeg
            | InputFileType::Heic
            | InputFileType::Mov
            | InputFileType::M4v
            | InputFileType::Mp4
            | InputFileType::Numbers
            | InputFileType::Xlsx
            | InputFileType::Xls
            | InputFileType::Csv => self.canonical_extension(),
        }
    }

    /// Whether `ext` (no leading dot, any case) is one of this kind's
    /// spellings -- the membership test `manifest::gather_pending` uses in
    /// place of an exact-string compare against a single extension.
    pub(crate) fn matches_extension(self, ext: &str) -> bool {
        let ext = ext.to_ascii_lowercase();
        self.extensions().contains(&ext.as_str())
    }

    /// Case-insensitive; `jpg` is accepted as an alias for `jpeg`. Any
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
            "mp4-audio" => Ok(InputFileType::Mp4Audio),
            "numbers" => Ok(InputFileType::Numbers),
            "xlsx" => Ok(InputFileType::Xlsx),
            "xls" => Ok(InputFileType::Xls),
            "csv" => Ok(InputFileType::Csv),
            other => Err(format!(
                "unknown --input-file-type '{other}' (expected png, jpeg, heic, mov, m4v, mp4, \
                 mp4-audio, numbers, xlsx, xls, or csv)"
            )),
        }
    }

    /// Video kinds are always recompressed (ADR-0122) -- never
    /// copy-through, unlike `Jpeg` -- so this job's one justified departure
    /// from "preserve what's already in the target format" is explicit here
    /// rather than implied by a `match` elsewhere.
    ///
    /// `Mp4Audio` is **not** a video kind (ADR-0142) despite its `.mp4`
    /// input spelling: there is no video stream to recompress, and this
    /// returning `false` is also what correctly suppresses the
    /// `--video-quality` prompt for such a run.
    pub(crate) fn is_video(self) -> bool {
        matches!(
            self,
            InputFileType::Mov | InputFileType::M4v | InputFileType::Mp4
        )
    }

    /// The fixed output extension for this input kind: `jpg` for every image
    /// kind (ADR-0112), `mp4` for every video kind (ADR-0122) regardless of
    /// which video extension was the input, `opus` for `Mp4Audio`
    /// (ADR-0142).
    ///
    /// An exhaustive `match` rather than ADR-0112's original
    /// `if is_video() { "mp4" } else { "jpg" }`, so a kind added later
    /// cannot silently inherit `jpg` by falling through an `else`.
    pub(crate) fn output_extension(self) -> &'static str {
        match self {
            InputFileType::Png | InputFileType::Jpeg | InputFileType::Heic => "jpg",
            InputFileType::Mov | InputFileType::M4v | InputFileType::Mp4 => "mp4",
            InputFileType::Mp4Audio => "opus",
            InputFileType::Numbers
            | InputFileType::Xlsx
            | InputFileType::Xls
            | InputFileType::Csv => "csv",
        }
    }

    /// Whether this kind is a spreadsheet, i.e. goes through
    /// `super::sheets` rather than `super::media` (ADR-0149).
    ///
    /// Asks what a kind *is* rather than making each call site widen a
    /// `match` arm list. That matters here because a spreadsheet is this
    /// job's first kind where one input yields *several* outputs, so the
    /// branch it selects is structural rather than a choice of encoder --
    /// and a kind added later that forgot to appear in such a list would
    /// silently take the single-output path.
    pub(crate) fn is_spreadsheet(self) -> bool {
        matches!(
            self,
            InputFileType::Numbers | InputFileType::Xlsx | InputFileType::Xls | InputFileType::Csv
        )
    }
}

impl std::fmt::Display for InputFileType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
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
        assert_eq!(
            InputFileType::parse("mp4-audio").unwrap(),
            InputFileType::Mp4Audio
        );
        assert_eq!(
            InputFileType::parse("MP4-AUDIO").unwrap(),
            InputFileType::Mp4Audio
        );
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
        // Neighbouring spellings of the ADR-0142 kind are not aliases.
        assert!(InputFileType::parse("mp4audio").is_err());
        assert!(InputFileType::parse("mp4_audio").is_err());
        assert!(InputFileType::parse("audio").is_err());
        assert!(InputFileType::parse("opus").is_err());
    }

    #[test]
    fn canonical_extension_and_display_agree() {
        // Every kind but `Mp4Audio` (ADR-0142), whose display name and
        // addressing prefix deliberately differ -- see `name()`.
        for kind in [
            InputFileType::Png,
            InputFileType::Jpeg,
            InputFileType::Heic,
            InputFileType::Mov,
            InputFileType::M4v,
            InputFileType::Mp4,
            InputFileType::Numbers,
            InputFileType::Xlsx,
            InputFileType::Xls,
            InputFileType::Csv,
        ] {
            assert_eq!(kind.canonical_extension(), kind.to_string());
        }
    }

    /// ADR-0142: `Mp4Audio` is addressed by the `mp4` prefix its files
    /// actually live under, but must *display* as `mp4-audio` -- otherwise
    /// the wizard's "Input file type" echo would be indistinguishable from
    /// a video run's.
    #[test]
    fn mp4_audio_displays_as_its_flag_spelling_not_its_addressing_prefix() {
        assert_eq!(InputFileType::Mp4Audio.to_string(), "mp4-audio");
        assert_eq!(InputFileType::Mp4Audio.name(), "mp4-audio");
        assert_eq!(InputFileType::Mp4Audio.canonical_extension(), "mp4");
    }

    /// Every kind's `name()` must round-trip back through `parse`, so the
    /// displayed spelling is always a value the operator can actually pass
    /// to `--input-file-type`.
    #[test]
    fn every_kinds_display_name_parses_back_to_itself() {
        for kind in [
            InputFileType::Png,
            InputFileType::Jpeg,
            InputFileType::Heic,
            InputFileType::Mov,
            InputFileType::M4v,
            InputFileType::Mp4,
            InputFileType::Mp4Audio,
            InputFileType::Numbers,
            InputFileType::Xlsx,
            InputFileType::Xls,
            InputFileType::Csv,
        ] {
            assert_eq!(InputFileType::parse(kind.name()).unwrap(), kind);
        }
    }

    #[test]
    fn mp4_audio_shares_mp4s_on_disk_spelling_but_outputs_opus() {
        // No file extension distinguishes an audio-only `.mp4`, so this
        // kind matches the same spelling as `Mp4`; `transcode_audio`'s
        // has-a-video-stream guard is what makes selecting it safe.
        assert_eq!(InputFileType::Mp4Audio.extensions(), &["mp4"]);
        assert!(InputFileType::Mp4Audio.matches_extension("mp4"));
        assert!(InputFileType::Mp4Audio.matches_extension("MP4"));
        assert_eq!(InputFileType::Mp4Audio.output_extension(), "opus");
        for kind in [
            InputFileType::Numbers,
            InputFileType::Xlsx,
            InputFileType::Xls,
            InputFileType::Csv,
        ] {
            assert_eq!(kind.output_extension(), "csv", "{kind}");
        }
    }

    /// ADR-0149: the four spreadsheet kinds.
    #[test]
    fn parse_accepts_every_spreadsheet_kind_case_insensitively() {
        for (value, expected) in [
            ("numbers", InputFileType::Numbers),
            ("NUMBERS", InputFileType::Numbers),
            ("xlsx", InputFileType::Xlsx),
            ("XLSX", InputFileType::Xlsx),
            ("xls", InputFileType::Xls),
            ("XLS", InputFileType::Xls),
            ("csv", InputFileType::Csv),
            ("CSV", InputFileType::Csv),
        ] {
            assert_eq!(InputFileType::parse(value).unwrap(), expected, "{value}");
        }
    }

    /// Neighbouring spellings are not aliases, per ADR-0139's rule that one
    /// is added on evidence of a real miss rather than in anticipation.
    /// `calamine` reads `xlsm`/`xlsb`/`ods` too; they are deliberately out.
    #[test]
    fn parse_rejects_neighbouring_spreadsheet_spellings() {
        for value in [
            "xlsx2",
            "number",
            "numbers2",
            "excel",
            "spreadsheet",
            "sheet",
            "xlsm",
            "xlsb",
            "ods",
            "tsv",
        ] {
            assert!(InputFileType::parse(value).is_err(), "{value} parsed");
        }
    }

    /// `*.xls` does not glob-match `foo.xlsx`, so the two kinds stay
    /// disjoint at the rclone `--include` filter with no special handling --
    /// and `matches_extension` must agree with that.
    #[test]
    fn xls_and_xlsx_are_disjoint_kinds() {
        assert!(InputFileType::Xls.matches_extension("xls"));
        assert!(InputFileType::Xls.matches_extension("XLS"));
        assert!(!InputFileType::Xls.matches_extension("xlsx"));
        assert!(InputFileType::Xlsx.matches_extension("xlsx"));
        assert!(!InputFileType::Xlsx.matches_extension("xls"));
    }

    #[test]
    fn is_spreadsheet_is_true_only_for_spreadsheet_kinds() {
        for kind in [
            InputFileType::Numbers,
            InputFileType::Xlsx,
            InputFileType::Xls,
            InputFileType::Csv,
        ] {
            assert!(kind.is_spreadsheet(), "{kind}");
            // Not video, which is also what suppresses --video-quality.
            assert!(!kind.is_video(), "{kind}");
        }
        for kind in [
            InputFileType::Png,
            InputFileType::Jpeg,
            InputFileType::Heic,
            InputFileType::Mov,
            InputFileType::M4v,
            InputFileType::Mp4,
            InputFileType::Mp4Audio,
        ] {
            assert!(!kind.is_spreadsheet(), "{kind}");
        }
    }

    /// `Mp4Audio` must not be a video kind despite its `.mp4` input
    /// spelling -- this is also what suppresses the `--video-quality`
    /// prompt for such a run (`dispatch_async` gates it on `is_video()`).
    #[test]
    fn mp4_audio_is_not_a_video_kind() {
        assert!(!InputFileType::Mp4Audio.is_video());
    }

    /// The ADR-0139 regression: a `.jpg` file must be matched by a `jpeg`
    /// run. Before this, `extension()` returned only `"jpeg"`, so the
    /// source listing's `--include '*.jpeg'` silently matched nothing in a
    /// directory of `.jpg` files.
    #[test]
    fn jpeg_matches_both_jpg_and_jpeg_spellings() {
        assert_eq!(InputFileType::Jpeg.extensions(), &["jpg", "jpeg"]);
        assert!(InputFileType::Jpeg.matches_extension("jpg"));
        assert!(InputFileType::Jpeg.matches_extension("jpeg"));
    }

    #[test]
    fn jpeg_canonical_extension_is_jpg_agreeing_with_its_output_extension() {
        // Both halves of ADR-0138's prefix inference now agree: a jpeg run
        // defaults to `source:jpg/` -> `destination:jpg/`, where the source
        // half used to default to `jpeg/`.
        assert_eq!(InputFileType::Jpeg.canonical_extension(), "jpg");
        assert_eq!(InputFileType::Jpeg.output_extension(), "jpg");
    }

    #[test]
    fn every_other_kind_has_exactly_one_spelling() {
        for kind in [
            InputFileType::Png,
            InputFileType::Heic,
            InputFileType::Mov,
            InputFileType::M4v,
            InputFileType::Mp4,
            InputFileType::Mp4Audio,
            InputFileType::Numbers,
            InputFileType::Xlsx,
            InputFileType::Xls,
            InputFileType::Csv,
        ] {
            assert_eq!(
                kind.extensions().len(),
                1,
                "{kind} gained an unintended alias"
            );
        }
    }

    #[test]
    fn matches_extension_is_case_insensitive_and_rejects_other_formats() {
        assert!(InputFileType::Jpeg.matches_extension("JPG"));
        assert!(InputFileType::Jpeg.matches_extension("JpEg"));
        assert!(!InputFileType::Jpeg.matches_extension("png"));
        assert!(!InputFileType::Jpeg.matches_extension(""));
        assert!(InputFileType::Png.matches_extension("PNG"));
        assert!(!InputFileType::Png.matches_extension("jpg"));
        // A video kind must not match a sibling video extension -- each
        // run still transcodes exactly one input kind.
        assert!(!InputFileType::Mov.matches_extension("mp4"));
    }

    #[test]
    fn is_video_is_true_only_for_video_kinds() {
        assert!(!InputFileType::Png.is_video());
        assert!(!InputFileType::Jpeg.is_video());
        assert!(!InputFileType::Heic.is_video());
        assert!(InputFileType::Mov.is_video());
        assert!(InputFileType::M4v.is_video());
        assert!(InputFileType::Mp4.is_video());
        assert!(!InputFileType::Mp4Audio.is_video());
    }

    #[test]
    fn output_extension_is_jpg_for_image_kinds_and_mp4_for_video_kinds() {
        assert_eq!(InputFileType::Png.output_extension(), "jpg");
        assert_eq!(InputFileType::Jpeg.output_extension(), "jpg");
        assert_eq!(InputFileType::Heic.output_extension(), "jpg");
        assert_eq!(InputFileType::Mov.output_extension(), "mp4");
        assert_eq!(InputFileType::M4v.output_extension(), "mp4");
        assert_eq!(InputFileType::Mp4.output_extension(), "mp4");
        assert_eq!(InputFileType::Mp4Audio.output_extension(), "opus");
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
