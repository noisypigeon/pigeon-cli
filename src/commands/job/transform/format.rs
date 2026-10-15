//! `--input-file-type` (ADR-0112, extended to video by ADR-0122, audio by
//! ADR-0142/0153, spreadsheets by ADR-0149 and documents by ADR-0154): a
//! small, vetted menu, not an arbitrary string -- mirrors
//! `rclone::cli::RcloneAction`'s precedent. Named `InputFileType`, not
//! `ImageInputType`, and the job itself named `transform`, not
//! `image-transform`, so later iterations could add non-image kinds to this
//! same enum/command without a rename -- which is exactly what the five
//! families here became.

/// Which input format this run transcodes.
///
/// The output extension is **not** derivable from any one predicate: image
/// kinds output `.jpg` (ADR-0112), video kinds `.mp4` at the chosen
/// `VideoQuality` (ADR-0122), audio kinds `.opus` at the chosen
/// `AudioQuality` (ADR-0142, ADR-0153), spreadsheet kinds one `.csv` per
/// non-empty sheet (ADR-0149), and document kinds -- the first family whose
/// members disagree -- `.pdf`, `.txt` or `.csv` (ADR-0154). See
/// `output_extension`, which is the single authority on this and is
/// deliberately an exhaustive `match`.
///
/// `Jpeg`, `Csv`, `Pdf` and `Txt` are copy-through: already in the target
/// format, so the bytes are preserved untouched. `Opus` is the one kind that
/// arrives in its target format and is re-encoded anyway (ADR-0153 §6).
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
    /// AAC in an MP4 container -- the ordinary iTunes-era music file
    /// (ADR-0153).
    M4a,
    Mp3,
    /// Raw AAC in an ADTS stream, with no container. Worth distinguishing
    /// from `M4a` in one's head because ADTS carries no duration, which is
    /// why ADR-0153 had to make the truncated-encode guard one-sided.
    Aac,
    /// Uncompressed PCM. The only **lossless** input this job takes, which
    /// is why it defaults to the top quality tier: the encode is the first
    /// lossy generation the file has ever had.
    Wav,
    /// Already Opus. Deliberately **re-encoded** rather than copied
    /// through (ADR-0153), so a mixed library normalizes to one bitrate --
    /// a departure from ADR-0112's jpeg and ADR-0149's csv copy-through,
    /// at the cost of a second lossy generation.
    Opus,
    /// Microsoft Word's old binary format (CFBF). A genuinely different
    /// format from `Docx`, reached by a different LibreOffice filter, so
    /// they are disjoint kinds rather than two spellings of one --
    /// following `Xls`/`Xlsx` (ADR-0154).
    Doc,
    /// Microsoft Word's modern OOXML format.
    Docx,
    /// Apple Pages. Imported by the same `libetonyek` that reads
    /// `Numbers`, so it needs `soffice` for the same reason (ADR-0154).
    Pages,
    /// Rich Text Format.
    Rtf,
    /// Already PDF: copied through byte-for-byte, the same reasoning
    /// ADR-0112 applies to a `jpeg` and ADR-0149 to a `csv` input already
    /// in the target format.
    Pdf,
    /// An e-book. The **only** document kind LibreOffice cannot read:
    /// its EPUB filter is export-only (`flags=EXPORT ALIEN 3RDPARTYFILTER`
    /// with no IMPORT counterpart anywhere in its registry), which is why
    /// this kind alone shells out to Calibre's `ebook-convert` (ADR-0154).
    Epub,
    /// A PowerPoint presentation.
    Pptx,
    /// Apple Keynote -- and also the conventional extension for a PEM
    /// private key, which is why this is the one kind whose input is
    /// sniffed before conversion (ADR-0154 §2).
    ///
    /// LibreOffice does **not** refuse a private key named `.key`: it
    /// imports it as a plain-text Writer document, exits 0, and writes a
    /// valid PDF with the key legible in it. Measured. Without
    /// `documents::ensure_keynote` a run of this kind would publish
    /// private keys to the destination bucket.
    Key,
    /// Already plain text: copied through byte-for-byte.
    Txt,
    /// Markdown. The first kind with **three** spellings -- `md`,
    /// `markdown` and `mkd` are one format under different extensions,
    /// generalizing `Jpeg`'s two (ADR-0154 §1).
    ///
    /// Rendered to prose by `pandoc -t plain`, which **discards link URLs
    /// and all inline markup**. Lossy and deliberate: this family is
    /// defined by its output being readable prose (ADR-0154 §8).
    Md,
    /// reStructuredText. Shares `Md`'s converter and its markup loss; a
    /// separate kind because `pandoc` needs to be told which reader to
    /// use, and because the two are genuinely different formats.
    Rst,
    /// The macOS snippet created by dragging selected text to the Finder.
    ///
    /// A modern one is a **data-fork binary property list**, not the
    /// resource fork the name suggests (measured, ADR-0154): one
    /// `UTI-Data` key holding three encodings of the same text, one of
    /// them already UTF-8. So it is read in-process with no external
    /// binary and no platform dependency.
    Textclipping,
    /// A tab-separated table, converted to CSV. Deliberately **not** a
    /// member of `is_spreadsheet()` (ADR-0154 §4): that predicate is
    /// consulted by three unrelated concerns and this kind is
    /// single-output, so it would be right for only one of them.
    Tsv,
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
    /// `Jpeg` has two spellings and `Md` (ADR-0154) three; every other
    /// kind keeps exactly one, deliberately unspeculative -- a new alias
    /// gets added when a real run is observed to miss files, not in
    /// anticipation.
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
            InputFileType::M4a => &["m4a"],
            InputFileType::Mp3 => &["mp3"],
            InputFileType::Aac => &["aac"],
            InputFileType::Wav => &["wav"],
            InputFileType::Opus => &["opus"],
            InputFileType::Doc => &["doc"],
            InputFileType::Docx => &["docx"],
            InputFileType::Pages => &["pages"],
            InputFileType::Rtf => &["rtf"],
            InputFileType::Pdf => &["pdf"],
            InputFileType::Epub => &["epub"],
            InputFileType::Pptx => &["pptx"],
            InputFileType::Key => &["key"],
            InputFileType::Txt => &["txt"],
            // One format, three extensions in the wild (ADR-0154 §1).
            InputFileType::Md => &["md", "markdown", "mkd"],
            InputFileType::Rst => &["rst"],
            InputFileType::Textclipping => &["textclipping"],
            InputFileType::Tsv => &["tsv"],
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
            | InputFileType::Csv
            | InputFileType::M4a
            | InputFileType::Mp3
            | InputFileType::Aac
            | InputFileType::Wav
            | InputFileType::Opus
            | InputFileType::Doc
            | InputFileType::Docx
            | InputFileType::Pages
            | InputFileType::Rtf
            | InputFileType::Pdf
            | InputFileType::Epub
            | InputFileType::Pptx
            | InputFileType::Key
            | InputFileType::Txt
            | InputFileType::Md
            | InputFileType::Rst
            | InputFileType::Textclipping
            | InputFileType::Tsv => self.canonical_extension(),
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
            "m4a" => Ok(InputFileType::M4a),
            "mp3" => Ok(InputFileType::Mp3),
            "aac" => Ok(InputFileType::Aac),
            "wav" => Ok(InputFileType::Wav),
            "opus" => Ok(InputFileType::Opus),
            "doc" => Ok(InputFileType::Doc),
            "docx" => Ok(InputFileType::Docx),
            "pages" => Ok(InputFileType::Pages),
            "rtf" => Ok(InputFileType::Rtf),
            "pdf" => Ok(InputFileType::Pdf),
            "epub" => Ok(InputFileType::Epub),
            "pptx" => Ok(InputFileType::Pptx),
            "key" => Ok(InputFileType::Key),
            "txt" => Ok(InputFileType::Txt),
            "md" | "markdown" | "mkd" => Ok(InputFileType::Md),
            "rst" => Ok(InputFileType::Rst),
            "textclipping" => Ok(InputFileType::Textclipping),
            "tsv" => Ok(InputFileType::Tsv),
            // Grouped by output family rather than listed flat: at 29
            // spellings a single run-on list is unreadable, and the
            // grouping is the thing a reader actually needs -- which
            // extension they will find their output under (ADR-0154).
            other => Err(format!(
                "unknown --input-file-type '{other}' (expected an image kind png, jpeg or heic; \
                 a video kind mov, m4v or mp4; an audio kind mp4-audio, m4a, mp3, aac, wav or \
                 opus; a spreadsheet kind numbers, xlsx, xls or csv; a document kind doc, docx, \
                 pages, rtf, pdf, epub, pptx or key; a text kind txt, md, markdown, mkd, rst or \
                 textclipping; or tsv)"
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
    /// (ADR-0142), and -- since ADR-0154 -- `pdf`, `txt` or `csv` for a
    /// document kind.
    ///
    /// Documents are the first input family whose members do **not** share
    /// one output extension, which is why ADR-0154 §4 adds no
    /// `is_document()` to sit beside `is_video()`/`is_audio()`/
    /// `is_spreadsheet()`: those work because each of their families has a
    /// single output, so the predicate and the branch it selects coincide.
    /// Here this function carries the distinction instead.
    ///
    /// An exhaustive `match` rather than ADR-0112's original
    /// `if is_video() { "mp4" } else { "jpg" }`, so a kind added later
    /// cannot silently inherit `jpg` by falling through an `else`.
    pub(crate) fn output_extension(self) -> &'static str {
        match self {
            InputFileType::Png | InputFileType::Jpeg | InputFileType::Heic => "jpg",
            InputFileType::Mov | InputFileType::M4v | InputFileType::Mp4 => "mp4",
            InputFileType::Mp4Audio
            | InputFileType::M4a
            | InputFileType::Mp3
            | InputFileType::Aac
            | InputFileType::Wav
            | InputFileType::Opus => "opus",
            InputFileType::Numbers
            | InputFileType::Xlsx
            | InputFileType::Xls
            | InputFileType::Csv
            | InputFileType::Tsv => "csv",
            InputFileType::Doc
            | InputFileType::Docx
            | InputFileType::Pages
            | InputFileType::Rtf
            | InputFileType::Pdf
            | InputFileType::Epub
            | InputFileType::Pptx
            | InputFileType::Key => "pdf",
            InputFileType::Txt
            | InputFileType::Md
            | InputFileType::Rst
            | InputFileType::Textclipping => "txt",
        }
    }

    /// Whether this kind is transcoded to Opus by `media::transcode_audio`
    /// (ADR-0153).
    ///
    /// Asks what a kind *is*, like `is_video`/`is_spreadsheet`, rather than
    /// making each call site widen a `match` arm list -- the shape that let
    /// ADR-0141's audio-only dispatch bug hide in one. It gates the
    /// `--audio-quality` prompt as well as the converter dispatch.
    pub(crate) fn is_audio(self) -> bool {
        matches!(
            self,
            InputFileType::Mp4Audio
                | InputFileType::M4a
                | InputFileType::Mp3
                | InputFileType::Aac
                | InputFileType::Wav
                | InputFileType::Opus
        )
    }

    /// This kind's `--audio-quality` default (ADR-0153). Only consulted
    /// when `is_audio()`.
    ///
    /// The default matters more than the menu: one that defaulted to
    /// `Medium` everywhere would still quietly destroy a music library,
    /// which is the whole reason ADR-0142's single fixed bitrate was not
    /// simply extended to these kinds.
    pub(crate) fn default_audio_quality(self) -> AudioQuality {
        match self {
            // ADR-0142's own choice, kept so its output is byte-identical
            // when the flag is omitted.
            InputFileType::Mp4Audio => AudioQuality::Medium,
            // Lossy sources: the encode is a *second* generation, so the
            // top tier limits the compounding. `Wav` is lossless, which
            // makes this the *first* lossy generation it has ever had and
            // is the strongest case of all for the top tier.
            InputFileType::M4a
            | InputFileType::Mp3
            | InputFileType::Aac
            | InputFileType::Wav
            | InputFileType::Opus => AudioQuality::Transparent,
            // Not an audio kind; never consulted.
            _ => AudioQuality::Medium,
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
    ///
    /// `Tsv` is deliberately **not** a member despite also producing a
    /// `.csv` (ADR-0154 §4). This predicate is consulted by three
    /// unrelated concerns -- `strict_stem` selection in
    /// `placement::group_key`, `place_sheet` versus `place_one`, and
    /// converter dispatch -- and a `tsv` is single-output, so joining it
    /// here would be right for only one of the three.
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

/// `--audio-quality` (ADR-0153): a small, vetted Opus bitrate menu,
/// mirroring `VideoQuality`'s shape so there is one convention for a
/// quality menu in this job rather than two. Only consulted when
/// `InputFileType::is_audio()`.
///
/// ADR-0142 deliberately shipped one fixed bitrate and deferred this
/// "if a real need surfaces". It surfaced: that bitrate was chosen for
/// voice notes of unknown type, and applying it to a music library would
/// be a severe, irreversible loss in a pipeline whose source may not
/// survive to redo it from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AudioQuality {
    Low,
    Medium,
    High,
    /// Deliberately **not** called `Lossless`, unlike `VideoQuality`'s tier
    /// of that name: Opus has no lossless mode, so the name would be a lie.
    /// `Transparent` claims only what it delivers -- perceptual
    /// indistinguishability, the widely-cited threshold for Opus stereo.
    Transparent,
}

impl AudioQuality {
    /// `libopus` target bitrate for this tier, as ffmpeg's `-b:a` takes it.
    ///
    /// `Medium`'s `48k` carries ADR-0142's reasoning, which still applies
    /// to the kind it was chosen for: 32k is the speech-tuned figure, and
    /// 48k is the lowest rate that stays comfortably transparent for speech
    /// while remaining acceptable for general audio -- the right call for
    /// personal archival audio of *unknown* type, deliberately a notch
    /// better than the conventional default, the same
    /// archival-over-streaming reasoning as `VideoQuality::Medium`'s CRF.
    pub(crate) fn bitrate(self) -> &'static str {
        match self {
            AudioQuality::Low => "32k",
            AudioQuality::Medium => "48k",
            AudioQuality::High => "96k",
            AudioQuality::Transparent => "128k",
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            AudioQuality::Low => "low",
            AudioQuality::Medium => "medium",
            AudioQuality::High => "high",
            AudioQuality::Transparent => "transparent",
        }
    }

    /// Case-insensitive; any other value is a hard error -- same discipline
    /// as `InputFileType::parse` and `VideoQuality::parse`.
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value.to_ascii_lowercase().as_str() {
            "low" => Ok(AudioQuality::Low),
            "medium" => Ok(AudioQuality::Medium),
            "high" => Ok(AudioQuality::High),
            "transparent" => Ok(AudioQuality::Transparent),
            other => Err(format!(
                "unknown --audio-quality '{other}' (expected low, medium, high, or transparent)"
            )),
        }
    }
}

impl std::fmt::Display for AudioQuality {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
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
        // `opus` was rejected here until ADR-0153, which makes it a real
        // input kind -- it was only ever an *output* extension before.
        assert_eq!(InputFileType::parse("opus").unwrap(), InputFileType::Opus);
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
            InputFileType::M4a,
            InputFileType::Mp3,
            InputFileType::Aac,
            InputFileType::Wav,
            InputFileType::Opus,
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
            InputFileType::M4a,
            InputFileType::Mp3,
            InputFileType::Aac,
            InputFileType::Wav,
            InputFileType::Opus,
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
            // `tsv` was here until ADR-0154 made it a real kind -- the
            // same reversal ADR-0153 made for `opus`.
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
            InputFileType::M4a,
            InputFileType::Mp3,
            InputFileType::Aac,
            InputFileType::Wav,
            InputFileType::Opus,
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

    // --- ADR-0153: audio kinds ------------------------------------------

    #[test]
    fn parse_accepts_every_audio_kind_case_insensitively() {
        for (value, expected) in [
            ("m4a", InputFileType::M4a),
            ("M4A", InputFileType::M4a),
            ("mp3", InputFileType::Mp3),
            ("MP3", InputFileType::Mp3),
            ("aac", InputFileType::Aac),
            ("AAC", InputFileType::Aac),
            ("wav", InputFileType::Wav),
            ("WAV", InputFileType::Wav),
            ("opus", InputFileType::Opus),
            ("OPUS", InputFileType::Opus),
        ] {
            assert_eq!(InputFileType::parse(value).unwrap(), expected, "{value}");
        }
    }

    /// Per ADR-0139's rule, an alias is added on evidence of a real miss
    /// rather than in anticipation -- so none of these is one.
    #[test]
    fn parse_rejects_neighbouring_audio_spellings() {
        for value in [
            "mpeg3", "mp2", "m4b", "m4p", "oga", "ogg", "wave", "adts", "flac", "aiff", "wma",
        ] {
            assert!(InputFileType::parse(value).is_err(), "{value} parsed");
        }
    }

    #[test]
    fn every_audio_kind_outputs_opus_and_is_neither_video_nor_spreadsheet() {
        for kind in [
            InputFileType::Mp4Audio,
            InputFileType::M4a,
            InputFileType::Mp3,
            InputFileType::Aac,
            InputFileType::Wav,
            InputFileType::Opus,
        ] {
            assert!(kind.is_audio(), "{kind}");
            assert_eq!(kind.output_extension(), "opus", "{kind}");
            // Not video, which is also what suppresses --video-quality.
            assert!(!kind.is_video(), "{kind}");
            assert!(!kind.is_spreadsheet(), "{kind}");
        }
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
            assert!(!kind.is_audio(), "{kind}");
        }
    }

    /// **The assertion that pins ADR-0142's behaviour as unchanged.** Its
    /// 48k was chosen for voice notes of unknown type and is still right
    /// for them; the new kinds are music or lossless and would be badly
    /// served by it.
    #[test]
    fn the_audio_quality_default_varies_by_kind() {
        assert_eq!(
            InputFileType::Mp4Audio.default_audio_quality(),
            AudioQuality::Medium,
            "ADR-0142's output must be byte-identical when the flag is omitted"
        );
        for kind in [
            InputFileType::M4a,
            InputFileType::Mp3,
            InputFileType::Aac,
            InputFileType::Wav,
            InputFileType::Opus,
        ] {
            assert_eq!(
                kind.default_audio_quality(),
                AudioQuality::Transparent,
                "{kind} is music or lossless"
            );
        }
    }

    #[test]
    fn audio_quality_parse_accepts_every_tier_case_insensitively() {
        assert_eq!(AudioQuality::parse("low").unwrap(), AudioQuality::Low);
        assert_eq!(AudioQuality::parse("MEDIUM").unwrap(), AudioQuality::Medium);
        assert_eq!(AudioQuality::parse("high").unwrap(), AudioQuality::High);
        assert_eq!(
            AudioQuality::parse("Transparent").unwrap(),
            AudioQuality::Transparent
        );
    }

    #[test]
    fn audio_quality_parse_rejects_anything_else() {
        // Notably `lossless`, which is `VideoQuality`'s top tier but would
        // be a lie for Opus -- it has no lossless mode.
        for value in ["lossless", "best", "128k", "", "ultra"] {
            assert!(AudioQuality::parse(value).is_err(), "{value} parsed");
        }
    }

    #[test]
    fn audio_quality_bitrates_increase_with_the_tier() {
        assert_eq!(AudioQuality::Low.bitrate(), "32k");
        assert_eq!(AudioQuality::Medium.bitrate(), "48k");
        assert_eq!(AudioQuality::High.bitrate(), "96k");
        assert_eq!(AudioQuality::Transparent.bitrate(), "128k");
    }

    #[test]
    fn audio_quality_as_str_and_display_agree_and_round_trip() {
        for quality in [
            AudioQuality::Low,
            AudioQuality::Medium,
            AudioQuality::High,
            AudioQuality::Transparent,
        ] {
            assert_eq!(quality.as_str(), quality.to_string());
            assert_eq!(AudioQuality::parse(quality.as_str()).unwrap(), quality);
        }
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

    // --- ADR-0154: documents ---

    #[test]
    fn parse_accepts_every_document_kind_case_insensitively() {
        for (value, expected) in [
            ("doc", InputFileType::Doc),
            ("DOC", InputFileType::Doc),
            ("docx", InputFileType::Docx),
            ("DocX", InputFileType::Docx),
            ("pages", InputFileType::Pages),
            ("rtf", InputFileType::Rtf),
            ("RTF", InputFileType::Rtf),
            ("pdf", InputFileType::Pdf),
            ("epub", InputFileType::Epub),
            ("pptx", InputFileType::Pptx),
            ("key", InputFileType::Key),
            ("txt", InputFileType::Txt),
            ("rst", InputFileType::Rst),
            ("textclipping", InputFileType::Textclipping),
            ("textClipping", InputFileType::Textclipping),
            ("tsv", InputFileType::Tsv),
            ("TSV", InputFileType::Tsv),
        ] {
            assert_eq!(InputFileType::parse(value), Ok(expected), "{value}");
        }
    }

    /// **The first kind with three spellings** (ADR-0154 §1), generalizing
    /// `Jpeg`'s two. They are one format, so one variant -- and the
    /// canonical one is `md`, which is the prefix a run addresses.
    #[test]
    fn markdowns_three_spellings_are_one_kind() {
        for value in ["md", "markdown", "mkd", "MD", "Markdown", "MKD"] {
            assert_eq!(
                InputFileType::parse(value),
                Ok(InputFileType::Md),
                "{value}"
            );
        }
        assert_eq!(InputFileType::Md.canonical_extension(), "md");
        assert_eq!(InputFileType::Md.name(), "md");
        for ext in ["md", "markdown", "mkd", "MARKDOWN"] {
            assert!(InputFileType::Md.matches_extension(ext), "{ext}");
        }
    }

    /// `doc` and `docx` are genuinely different formats reached by
    /// different LibreOffice filters, so they are disjoint kinds -- the
    /// `xls`/`xlsx` precedent (ADR-0154 §1). `*.doc` does not glob-match
    /// `foo.docx`, so they stay disjoint at the rclone filter too.
    #[test]
    fn doc_and_docx_are_disjoint_kinds() {
        assert!(InputFileType::Doc.matches_extension("doc"));
        assert!(!InputFileType::Doc.matches_extension("docx"));
        assert!(InputFileType::Docx.matches_extension("docx"));
        assert!(!InputFileType::Docx.matches_extension("doc"));
        assert_ne!(InputFileType::Doc, InputFileType::Docx);
    }

    #[test]
    fn parse_rejects_neighbouring_document_spellings() {
        for value in [
            "odt",
            "odp",
            "ods",
            "pps",
            "ppt",
            "text",
            "markdown2",
            "mdown",
            "restructuredtext",
            "clipping",
            "textclip",
            "tab",
            "tsvx",
            "doc.x",
            "pagesx",
            "keynote",
            "ebook",
        ] {
            assert!(InputFileType::parse(value).is_err(), "{value} parsed");
        }
    }

    /// Documents are the first family whose members do **not** share one
    /// output extension (ADR-0154), which is the whole reason §4 adds no
    /// `is_document()` predicate.
    #[test]
    fn the_document_kinds_fan_out_to_three_output_extensions() {
        for kind in [
            InputFileType::Doc,
            InputFileType::Docx,
            InputFileType::Pages,
            InputFileType::Rtf,
            InputFileType::Pdf,
            InputFileType::Epub,
            InputFileType::Pptx,
            InputFileType::Key,
        ] {
            assert_eq!(kind.output_extension(), "pdf", "{kind}");
        }
        for kind in [
            InputFileType::Txt,
            InputFileType::Md,
            InputFileType::Rst,
            InputFileType::Textclipping,
        ] {
            assert_eq!(kind.output_extension(), "txt", "{kind}");
        }
        assert_eq!(InputFileType::Tsv.output_extension(), "csv");
    }

    /// No document kind is video, audio, or -- crucially -- a spreadsheet.
    /// `Tsv` outputs `.csv` but is single-output, and `is_spreadsheet()`
    /// selects three unrelated behaviours it must not take (ADR-0154 §4).
    #[test]
    fn no_document_kind_is_video_audio_or_spreadsheet() {
        for kind in [
            InputFileType::Doc,
            InputFileType::Docx,
            InputFileType::Pages,
            InputFileType::Rtf,
            InputFileType::Pdf,
            InputFileType::Epub,
            InputFileType::Pptx,
            InputFileType::Key,
            InputFileType::Txt,
            InputFileType::Md,
            InputFileType::Rst,
            InputFileType::Textclipping,
            InputFileType::Tsv,
        ] {
            assert!(!kind.is_video(), "{kind} is_video");
            assert!(!kind.is_audio(), "{kind} is_audio");
            assert!(!kind.is_spreadsheet(), "{kind} is_spreadsheet");
        }
        // The four real spreadsheet kinds are unchanged.
        for kind in [
            InputFileType::Numbers,
            InputFileType::Xlsx,
            InputFileType::Xls,
            InputFileType::Csv,
        ] {
            assert!(kind.is_spreadsheet(), "{kind}");
        }
    }

    /// Prefix inference comes free from `canonical_extension()` /
    /// `output_extension()`: `source:docx/` -> `destination:pdf/`.
    #[test]
    fn every_document_kinds_display_name_parses_back_to_itself() {
        for kind in [
            InputFileType::Doc,
            InputFileType::Docx,
            InputFileType::Pages,
            InputFileType::Rtf,
            InputFileType::Pdf,
            InputFileType::Epub,
            InputFileType::Pptx,
            InputFileType::Key,
            InputFileType::Txt,
            InputFileType::Md,
            InputFileType::Rst,
            InputFileType::Textclipping,
            InputFileType::Tsv,
        ] {
            assert_eq!(InputFileType::parse(kind.name()), Ok(kind), "{kind}");
            assert_eq!(kind.name(), kind.canonical_extension(), "{kind}");
        }
    }

    /// The error text is grouped by output family rather than listed flat
    /// (ADR-0154): at 29 spellings a run-on list is unreadable, and the
    /// grouping is what a reader actually needs.
    #[test]
    fn the_parse_error_names_every_family() {
        let err = InputFileType::parse("nonsense").expect_err("rejected");
        for fragment in [
            "image kind",
            "video kind",
            "audio kind",
            "spreadsheet kind",
            "document kind",
            "text kind",
            "tsv",
        ] {
            assert!(err.contains(fragment), "{fragment} missing from: {err}");
        }
    }
}
