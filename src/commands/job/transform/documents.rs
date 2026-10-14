//! Document conversion (ADR-0154), kept separate from `media.rs` so that
//! module stays `ffmpeg`-only and from `sheets.rs` so that one stays about
//! workbooks.
//!
//! Five routes converge here, which is why this module exists at all --
//! documents are the first input family whose members do not share one
//! output extension, so there is no single converter to name it after:
//!
//! - `doc`/`docx`/`pages`/`rtf`/`pptx`/`key` to `.pdf` via `soffice`.
//! - `epub` to `.pdf` via Calibre's `ebook-convert`. The one kind
//!   LibreOffice cannot read: its EPUB filter is export-only.
//! - `md`/`rst` to `.txt` via `pandoc -t plain`, which discards markup.
//! - `textclipping` to `.txt` in-process, reading a binary property list.
//! - `tsv` to `.csv` in-process via the `csv` crate.
//!
//! `pdf` and `txt` input never reach this module -- they are copied through
//! by `media::copy_through`, the same treatment ADR-0112 gives a `jpeg`.

use std::path::{Path, PathBuf};

use super::format::InputFileType;
use super::media::{
    NOT_A_DOCUMENT_SIGNATURE, NOT_A_KEYNOTE_SIGNATURE, NOT_A_TEXTCLIPPING_SIGNATURE,
};

/// The zip local-file-header magic every modern iWork document begins with.
const ZIP_MAGIC: &[u8; 4] = b"PK\x03\x04";

/// Largest input `tsv_to_csv` will read.
///
/// Mirrors `sheets::MAX_WORKBOOK_BYTES`'s reasoning at this module's much
/// lower memory cost: the CSV writer streams row by row, so this bounds a
/// pathological single *field* rather than the whole file, and is checked
/// against the file on disk because that is cheap and pre-emptive.
const MAX_TEXT_BYTES: u64 = 512 * 1024 * 1024;

/// Confirms Calibre's `ebook-convert` is on `PATH`, for an `epub` run.
///
/// Checks the **exit status**, not merely that the process spawned --
/// following `soffice::check_available` rather than
/// `media::check_ffmpeg_available`, which has the present-but-broken-binary
/// hole its own doc comment admits
/// ([#100](https://github.com/noisypigeon/pigeon-cli/issues/100)).
pub(crate) async fn check_calibre_available() -> Result<(), String> {
    check_binary(
        "ebook-convert",
        &["--version"],
        "required for 'pigeon job run transform --input-file-type epub'. LibreOffice cannot \
         read EPUB (its filter is export-only), so this kind needs Calibre",
    )
    .await
}

/// Confirms `pandoc` is on `PATH`, for an `md`/`rst` run.
pub(crate) async fn check_pandoc_available() -> Result<(), String> {
    check_binary(
        "pandoc",
        &["--version"],
        "required for 'pigeon job run transform' with a markdown or reStructuredText input kind \
         (md, markdown, mkd, rst)",
    )
    .await
}

async fn check_binary(binary: &str, args: &[&str], why: &str) -> Result<(), String> {
    let output = tokio::process::Command::new(binary)
        .args(args)
        .output()
        .await
        .map_err(|_| format!("'{binary}' was not found on PATH -- {why}"))?;
    if !output.status.success() {
        return Err(format!(
            "'{binary} {}' failed (exit {:?}) -- {why}. stderr: {}",
            args.join(" "),
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

/// Rejects a `.key` that is not an iWork document, before any converter
/// sees it (ADR-0154 §2).
///
/// `.key` is Apple Keynote and also the conventional extension for a PEM
/// private key. LibreOffice does **not** refuse the second: it imports the
/// PEM as a plain-text Writer document, exits 0, and writes a valid PDF
/// with every line of key material legible in it -- measured. Because that
/// output is large, well-formed and non-empty, *no* check on the converter's
/// result can distinguish it, which is why this guards the input.
///
/// The check is **positive** -- it requires the iWork container magic
/// rather than blocklisting `-----BEGIN` -- because the set of things that
/// are not Keynote is open-ended: PKCS#8, PKCS#12, DER and whatever else
/// arrives carrying this extension.
///
/// A rejection is classified wrong-input-kind, so ADR-0143 deletes the
/// local copy and the key does not linger on the transform host. The source
/// object at `--source` is untouched, which is where it already was.
pub(crate) fn ensure_keynote(input: &Path) -> Result<(), String> {
    let mut header = [0u8; 4];
    let read = read_header(input, &mut header)?;
    if read == header.len() && &header == ZIP_MAGIC {
        return Ok(());
    }
    Err(format!(
        "{} {NOT_A_KEYNOTE_SIGNATURE} -- a Keynote document is a zip container, and this file \
         does not begin with one. A '.key' file is often a private key rather than a \
         presentation; it has not been converted, and its local copy is removed",
        input.display()
    ))
}

fn read_header(input: &Path, buffer: &mut [u8]) -> Result<usize, String> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(input)
        .map_err(|err| format!("failed to open {}: {err}", input.display()))?;
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(err) => return Err(format!("failed to read {}: {err}", input.display())),
        }
    }
    Ok(filled)
}

/// Converts a `soffice`-readable document to PDF inside `work_dir`,
/// returning the produced path.
///
/// `Key` is sniffed first; every other kind goes straight to LibreOffice.
pub(crate) async fn to_pdf(
    input: &Path,
    kind: InputFileType,
    work_dir: &Path,
) -> Result<PathBuf, String> {
    if kind == InputFileType::Key {
        ensure_keynote(input)?;
    }
    super::soffice::convert(input, work_dir, "pdf", NOT_A_DOCUMENT_SIGNATURE).await
}

/// Converts an EPUB to PDF inside `work_dir`, returning the produced path.
///
/// Calibre infers the output format from the output file's extension, so
/// unlike every `ffmpeg` call in this job -- which writes to a
/// `.scratch`-suffixed path and needs an explicit `-f` muxer (ADR-0115) --
/// this one genuinely requires a real `.pdf` name.
pub(crate) async fn epub_to_pdf(input: &Path, work_dir: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(work_dir)
        .map_err(|err| format!("failed to create {}: {err}", work_dir.display()))?;
    let stem = input
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| format!("{} has no usable file stem", input.display()))?;
    let produced = work_dir.join(format!("{stem}.pdf"));

    let output = tokio::process::Command::new("ebook-convert")
        .arg(input)
        .arg(&produced)
        .output()
        .await
        .map_err(|err| format!("failed to run ebook-convert: {err}"))?;

    // Same reasoning as `soffice::convert`: what the next stage needs is a
    // non-empty file where it expects one, so that is what is checked.
    match std::fs::metadata(&produced).map(|meta| meta.len()).ok() {
        Some(len) if len > 0 => Ok(produced),
        _ => Err(format!(
            "ebook-convert produced no usable .pdf for {} (exit {:?}) -- \
             {NOT_A_DOCUMENT_SIGNATURE}. stdout: {} stderr: {}",
            input.display(),
            output.status.code(),
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

/// Renders Markdown or reStructuredText to plain prose at `output`.
///
/// `-t plain` **discards link URLs and all inline markup** -- lossy and
/// deliberate (ADR-0154 §8). It is also what makes writing to a
/// `.scratch`-suffixed path safe: pandoc would otherwise infer the output
/// format from the extension, the same class of trap ADR-0115 hit when
/// `ffmpeg` was left to infer a muxer from one.
pub(crate) async fn markup_to_text(
    input: &Path,
    kind: InputFileType,
    output: &Path,
) -> Result<(), String> {
    let reader = match kind {
        InputFileType::Md => "markdown",
        InputFileType::Rst => "rst",
        other => return Err(format!("{other} is not a pandoc-readable markup kind")),
    };

    let result = tokio::process::Command::new("pandoc")
        .args(["-f", reader, "-t", "plain", "-o"])
        .arg(output)
        .arg(input)
        .output()
        .await
        .map_err(|err| format!("failed to run pandoc: {err}"))?;

    if !result.status.success() {
        return Err(format!(
            "pandoc failed to render {} (exit {:?}) -- {NOT_A_DOCUMENT_SIGNATURE}. stderr: {}",
            input.display(),
            result.status.code(),
            String::from_utf8_lossy(&result.stderr).trim()
        ));
    }
    // An empty source legitimately renders to an empty file, so unlike the
    // subprocess converters above, a zero length is not treated as failure
    // here -- pandoc's exit status is reliable.
    if !output.exists() {
        return Err(format!(
            "pandoc reported success but wrote no {} for {} -- {NOT_A_DOCUMENT_SIGNATURE}",
            output.display(),
            input.display()
        ));
    }
    Ok(())
}

/// Extracts the text from a macOS `.textclipping` to `output`.
///
/// A modern clipping is a **data-fork binary property list** (measured,
/// ADR-0154), not the resource fork the format's age suggests: one
/// `UTI-Data` key holding the same text under several pasteboard UTIs.
/// `public.utf8-plain-text` is preferred; the Mac-Roman-era key is the
/// fallback. A pre-OS-X resource-fork clipping is refused rather than
/// guessed at ([#110](https://github.com/noisypigeon/pigeon-cli/issues/110)).
pub(crate) fn clipping_to_text(input: &Path, output: &Path) -> Result<(), String> {
    let value = plist::Value::from_file(input)
        .map_err(|err| format!("{} {NOT_A_TEXTCLIPPING_SIGNATURE}: {err}", input.display()))?;

    let data = value
        .as_dictionary()
        .and_then(|dict| dict.get("UTI-Data"))
        .and_then(|inner| inner.as_dictionary())
        .ok_or_else(|| {
            format!(
                "{} {NOT_A_TEXTCLIPPING_SIGNATURE}: no 'UTI-Data' dictionary",
                input.display()
            )
        })?;

    let text = [
        "public.utf8-plain-text",
        "com.apple.traditional-mac-plain-text",
    ]
    .iter()
    .find_map(|key| data.get(key).and_then(plain_text))
    .ok_or_else(|| {
        format!(
            "{} {NOT_A_TEXTCLIPPING_SIGNATURE}: 'UTI-Data' carries no plain-text \
                 representation",
            input.display()
        )
    })?;

    std::fs::write(output, text.as_bytes())
        .map_err(|err| format!("failed to write {}: {err}", output.display()))
}

/// A pasteboard entry may be stored as a plist string or as raw bytes;
/// both appear in real clippings for the same text.
fn plain_text(value: &plist::Value) -> Option<String> {
    match value {
        plist::Value::String(text) => Some(text.clone()),
        plist::Value::Data(bytes) => String::from_utf8(bytes.clone()).ok(),
        _ => None,
    }
}

/// Converts a tab-separated table to CSV at `output`.
///
/// Not a tab-to-comma substitution: a field containing a comma has to
/// become a *quoted* CSV field, and a naive replacement would silently
/// corrupt the table. The `csv` crate ADR-0149 added for exactly this
/// reason ("Quoting/escaping untrusted cell text is not worth
/// hand-rolling") does both halves.
///
/// `flexible(true)` because a hand-maintained TSV routinely has ragged
/// rows, and refusing the whole file over one short row would be a worse
/// outcome than preserving it as-is.
pub(crate) fn tsv_to_csv(input: &Path, output: &Path) -> Result<(), String> {
    let size = std::fs::metadata(input)
        .map(|meta| meta.len())
        .map_err(|err| format!("failed to stat {}: {err}", input.display()))?;
    if size > MAX_TEXT_BYTES {
        return Err(format!(
            "{} is {size} bytes, over this job's {MAX_TEXT_BYTES}-byte text limit",
            input.display()
        ));
    }

    let mut reader = csv::ReaderBuilder::new()
        .delimiter(b'\t')
        .flexible(true)
        .has_headers(false)
        .from_path(input)
        .map_err(|err| format!("{} {NOT_A_DOCUMENT_SIGNATURE}: {err}", input.display()))?;

    let file = std::fs::File::create(output)
        .map_err(|err| format!("failed to create {}: {err}", output.display()))?;
    // `flexible` on **both** halves: the reader accepts a ragged row and
    // the writer must be willing to emit one, or the first short row fails
    // the file -- which is exactly what `flexible` on the reader alone was
    // meant to prevent.
    let mut writer = csv::WriterBuilder::new()
        .flexible(true)
        .from_writer(std::io::BufWriter::new(file));

    let mut rows = 0usize;
    for record in reader.records() {
        let record = record
            .map_err(|err| format!("{} {NOT_A_DOCUMENT_SIGNATURE}: {err}", input.display()))?;
        writer
            .write_record(&record)
            .map_err(|err| format!("failed to write a row to {}: {err}", output.display()))?;
        rows += 1;
    }
    writer
        .flush()
        .map_err(|err| format!("failed to flush {}: {err}", output.display()))?;

    // The structural analogue of `sheets`'s row-count check and every
    // `media` converter's duration guard: a table with no rows at all is a
    // failure rather than a silent empty output.
    if rows == 0 {
        return Err(format!(
            "{} {NOT_A_DOCUMENT_SIGNATURE}: no rows to convert",
            input.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::job::transform::media;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    // --- ADR-0154 §2: the private-key guard ---

    /// **The decision this ADR exists for.** LibreOffice converts a PEM
    /// private key named `.key` into a valid PDF with the key legible in
    /// it, exit 0 -- measured. Nothing downstream could catch that, so the
    /// input is refused here.
    #[test]
    fn a_private_key_named_dot_key_is_refused() {
        let dir = temp_dir();
        let input = dir.path().join("secret.key");
        std::fs::write(
            &input,
            b"-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n\
              -----END OPENSSH PRIVATE KEY-----\n",
        )
        .expect("write key");

        let err = ensure_keynote(&input).expect_err("a private key is not a Keynote document");
        assert!(err.contains(media::NOT_A_KEYNOTE_SIGNATURE), "{err}");
    }

    /// Both classifications matter and answer different questions: one
    /// stops the retries, the other licenses deleting the local copy --
    /// which for a stray private key is the wanted outcome.
    #[test]
    fn the_keynote_refusal_is_both_non_retryable_and_wrong_input_kind() {
        let dir = temp_dir();
        let input = dir.path().join("id_ed25519.key");
        std::fs::write(&input, b"-----BEGIN PRIVATE KEY-----\n").expect("write key");

        let err = ensure_keynote(&input).expect_err("refused");
        assert!(media::is_non_retryable_transcode_error(&err), "{err}");
        assert!(media::is_wrong_input_kind_error(&err), "{err}");
    }

    /// The check is positive -- it requires the iWork container magic --
    /// so a format nobody anticipated is refused rather than converted.
    #[test]
    fn a_der_key_with_no_pem_banner_is_also_refused() {
        let dir = temp_dir();
        let input = dir.path().join("binary.key");
        std::fs::write(&input, [0x30, 0x82, 0x04, 0xa4, 0x02, 0x01, 0x00]).expect("write der");

        let err = ensure_keynote(&input).expect_err("a DER key is not a Keynote document");
        assert!(err.contains(media::NOT_A_KEYNOTE_SIGNATURE), "{err}");
    }

    #[test]
    fn a_zip_container_passes_the_keynote_check() {
        let dir = temp_dir();
        let input = dir.path().join("deck.key");
        // A real Keynote is a zip; the magic is all this guard inspects.
        std::fs::write(&input, b"PK\x03\x04\x14\x00\x00\x00").expect("write zip");

        ensure_keynote(&input).expect("a zip container is accepted");
    }

    #[test]
    fn an_empty_file_is_refused_rather_than_accepted_by_a_short_read() {
        let dir = temp_dir();
        let input = dir.path().join("empty.key");
        std::fs::write(&input, b"").expect("write empty");

        let err = ensure_keynote(&input).expect_err("an empty file is not a Keynote document");
        assert!(err.contains(media::NOT_A_KEYNOTE_SIGNATURE), "{err}");
    }

    // --- ADR-0154: tsv -> csv ---

    /// **The reason this is a conversion and not a tab-to-comma
    /// substitution.** A field holding a comma has to come out quoted, or
    /// the table silently gains a column.
    #[test]
    fn a_field_containing_a_comma_is_quoted() {
        let dir = temp_dir();
        let input = dir.path().join("table.tsv");
        let output = dir.path().join("table.csv");
        std::fs::write(&input, "name\tnotes\nWidget\tsmall, red, round\n").expect("write tsv");

        tsv_to_csv(&input, &output).expect("converts");

        let csv = std::fs::read_to_string(&output).expect("read csv");
        assert_eq!(csv, "name,notes\nWidget,\"small, red, round\"\n");
    }

    /// A quote inside a field is the other escaping case the `csv` crate
    /// exists to get right.
    #[test]
    fn a_field_containing_a_quote_is_escaped() {
        let dir = temp_dir();
        let input = dir.path().join("quotes.tsv");
        let output = dir.path().join("quotes.csv");
        std::fs::write(&input, "label\nthe \"big\" one\n").expect("write tsv");

        tsv_to_csv(&input, &output).expect("converts");

        let csv = std::fs::read_to_string(&output).expect("read csv");
        assert_eq!(csv, "label\n\"the \"\"big\"\" one\"\n");
    }

    /// Ragged rows are normal in a hand-maintained TSV; refusing the whole
    /// file over one short row would lose more than it protects.
    #[test]
    fn a_ragged_row_is_preserved_rather_than_failing_the_file() {
        let dir = temp_dir();
        let input = dir.path().join("ragged.tsv");
        let output = dir.path().join("ragged.csv");
        std::fs::write(&input, "a\tb\tc\n1\t2\n3\t4\t5\t6\n").expect("write tsv");

        tsv_to_csv(&input, &output).expect("converts");

        let csv = std::fs::read_to_string(&output).expect("read csv");
        assert_eq!(csv, "a,b,c\n1,2\n3,4,5,6\n");
    }

    #[test]
    fn an_empty_tsv_fails_rather_than_producing_an_empty_output() {
        let dir = temp_dir();
        let input = dir.path().join("empty.tsv");
        let output = dir.path().join("empty.csv");
        std::fs::write(&input, b"").expect("write tsv");

        let err = tsv_to_csv(&input, &output).expect_err("no rows is a failure");
        assert!(err.contains(media::NOT_A_DOCUMENT_SIGNATURE), "{err}");
        assert!(media::is_non_retryable_transcode_error(&err), "{err}");
    }

    // --- ADR-0154: textclipping ---

    /// Built from the structure measured on a real clipping: one
    /// `UTI-Data` key holding the same text under several pasteboard UTIs.
    fn write_clipping(path: &Path, utf8: Option<&str>, mac_roman: Option<&[u8]>) {
        let mut inner = plist::Dictionary::new();
        if let Some(text) = utf8 {
            inner.insert(
                "public.utf8-plain-text".to_string(),
                plist::Value::String(text.to_string()),
            );
        }
        if let Some(bytes) = mac_roman {
            inner.insert(
                "com.apple.traditional-mac-plain-text".to_string(),
                plist::Value::Data(bytes.to_vec()),
            );
        }
        let mut outer = plist::Dictionary::new();
        outer.insert("UTI-Data".to_string(), plist::Value::Dictionary(inner));
        plist::Value::Dictionary(outer)
            .to_file_binary(path)
            .expect("write clipping");
    }

    #[test]
    fn a_clipping_yields_its_utf8_representation() {
        let dir = temp_dir();
        let input = dir.path().join("snippet.textclipping");
        let output = dir.path().join("snippet.txt");
        write_clipping(&input, Some("emailreceipt_20240818R2801167803"), None);

        clipping_to_text(&input, &output).expect("converts");

        let text = std::fs::read_to_string(&output).expect("read txt");
        assert_eq!(text, "emailreceipt_20240818R2801167803");
    }

    /// The UTF-8 key is preferred when both are present -- it needs no
    /// transcoding and is what a modern clipping always carries.
    #[test]
    fn the_utf8_representation_wins_over_the_mac_roman_one() {
        let dir = temp_dir();
        let input = dir.path().join("both.textclipping");
        let output = dir.path().join("both.txt");
        write_clipping(&input, Some("modern"), Some(b"legacy"));

        clipping_to_text(&input, &output).expect("converts");

        assert_eq!(
            std::fs::read_to_string(&output).expect("read txt"),
            "modern"
        );
    }

    #[test]
    fn a_clipping_without_a_utf8_key_falls_back_to_the_mac_roman_one() {
        let dir = temp_dir();
        let input = dir.path().join("legacy.textclipping");
        let output = dir.path().join("legacy.txt");
        write_clipping(&input, None, Some(b"older clipping"));

        clipping_to_text(&input, &output).expect("converts");

        assert_eq!(
            std::fs::read_to_string(&output).expect("read txt"),
            "older clipping"
        );
    }

    /// A pre-OS-X resource-fork clipping has no property list in its data
    /// fork at all. Refused clearly rather than guessed at (#110).
    #[test]
    fn a_resource_fork_era_clipping_is_refused_not_guessed_at() {
        let dir = temp_dir();
        let input = dir.path().join("ancient.textclipping");
        let output = dir.path().join("ancient.txt");
        std::fs::write(&input, b"\x00\x01\x00\x00not a property list").expect("write");

        let err = clipping_to_text(&input, &output).expect_err("refused");
        assert!(err.contains(media::NOT_A_TEXTCLIPPING_SIGNATURE), "{err}");
        assert!(media::is_non_retryable_transcode_error(&err), "{err}");
        assert!(media::is_wrong_input_kind_error(&err), "{err}");
    }

    #[test]
    fn a_property_list_without_uti_data_is_refused() {
        let dir = temp_dir();
        let input = dir.path().join("wrong.textclipping");
        let output = dir.path().join("wrong.txt");
        let mut outer = plist::Dictionary::new();
        outer.insert(
            "Something-Else".to_string(),
            plist::Value::Integer(1.into()),
        );
        plist::Value::Dictionary(outer)
            .to_file_binary(&input)
            .expect("write plist");

        let err = clipping_to_text(&input, &output).expect_err("refused");
        assert!(err.contains(media::NOT_A_TEXTCLIPPING_SIGNATURE), "{err}");
    }

    // --- ADR-0154: markup, and the binary preflights ---

    #[tokio::test]
    async fn markup_to_text_rejects_a_kind_pandoc_does_not_read() {
        let dir = temp_dir();
        let err = markup_to_text(
            &dir.path().join("x.png"),
            InputFileType::Png,
            &dir.path().join("x.txt"),
        )
        .await
        .expect_err("png is not a markup kind");
        assert!(err.contains("pandoc"), "{err}");
    }

    /// Deliberately **not** gated on `check_ffmpeg_available`: these kinds
    /// invoke no `ffmpeg`, so gating on it would make this silently never
    /// run -- the ADR-0139 mistake ADR-0149 named.
    #[tokio::test]
    async fn markdown_renders_to_prose_with_markup_discarded() {
        if check_pandoc_available().await.is_err() {
            eprintln!("skipping: pandoc not found on PATH");
            return;
        }
        let dir = temp_dir();
        let input = dir.path().join("note.md");
        let output = dir.path().join("note.txt");
        std::fs::write(
            &input,
            "# Heading\n\nSome **bold** text and [a link](https://example.com).\n",
        )
        .expect("write md");

        markup_to_text(&input, InputFileType::Md, &output)
            .await
            .expect("renders");

        let text = std::fs::read_to_string(&output).expect("read txt");
        assert!(text.contains("Heading"), "{text}");
        assert!(text.contains("bold"), "{text}");
        // ADR-0154 §8's recorded loss, pinned so it is a decision and not a
        // surprise: the URL does not survive.
        assert!(!text.contains("example.com"), "{text}");
        assert!(!text.contains("**"), "{text}");
    }

    #[tokio::test]
    async fn restructuredtext_renders_to_prose() {
        if check_pandoc_available().await.is_err() {
            eprintln!("skipping: pandoc not found on PATH");
            return;
        }
        let dir = temp_dir();
        let input = dir.path().join("doc.rst");
        let output = dir.path().join("doc.txt");
        std::fs::write(&input, "Title\n=====\n\nSome ``literal`` text.\n").expect("write rst");

        markup_to_text(&input, InputFileType::Rst, &output)
            .await
            .expect("renders");

        let text = std::fs::read_to_string(&output).expect("read txt");
        assert!(text.contains("Title"), "{text}");
        assert!(text.contains("literal"), "{text}");
    }

    #[tokio::test]
    async fn a_missing_binary_is_reported_clearly() {
        for result in [
            check_calibre_available().await,
            check_pandoc_available().await,
        ] {
            if let Err(err) = result {
                assert!(
                    err.contains("was not found on PATH") || err.contains("failed (exit"),
                    "{err}"
                );
            }
        }
    }
}
