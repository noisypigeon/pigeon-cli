//! Spreadsheet -> CSV conversion (ADR-0149), kept separate from `media.rs`
//! so that module stays `ffmpeg`-only.
//!
//! Two input routes converge on one splitter. `.xlsx`/`.xls` are read
//! in-process by `calamine`; `.numbers` is first converted to `.xlsx` by
//! `soffice` and then takes the same path. Routing Numbers through `.xlsx`
//! rather than letting LibreOffice emit CSV directly is deliberate: its
//! all-sheets CSV export names outputs by sheet *name* with no index, so
//! workbook order would be unrecoverable and the filenames would be its
//! choice rather than `placement`'s. Using it purely as a format reader
//! keeps **one** multi-sheet code path, and confines the only dependency on
//! LibreOffice to the one format with no Rust reader.
//!
//! `.csv` input never reaches this module -- it is copied through by
//! `media::copy_through`, the same treatment ADR-0112 gives a `jpeg`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use calamine::{Data, Range, Reader};

use super::media::NOT_A_WORKBOOK_SIGNATURE;

/// How long `soffice` gets before being treated as hung.
///
/// Unlike every `ffmpeg` call in this job, which has no timeout at all,
/// LibreOffice's documented failure mode is hanging rather than exiting
/// non-zero -- it is a desktop application running headless, and a
/// half-initialised profile can leave it waiting forever. A per-file cap is
/// the difference between one failed file and a run that never ends.
const SOFFICE_TIMEOUT: Duration = Duration::from_secs(300);

/// Largest input this module will hand to `calamine`.
///
/// This, not `MAX_SHEET_CELLS`, is the real memory bound: `worksheet_range`
/// materialises an entire sheet before its size can be asked, so a cap
/// expressed in cells can only fire *after* the allocation it would have
/// prevented. Checked against the file on disk, which is cheap and
/// genuinely pre-emptive. 512 MiB of compressed workbook is far past
/// anything this archive holds.
const MAX_WORKBOOK_BYTES: u64 = 512 * 1024 * 1024;

/// Largest single sheet, in cells, this module will write out.
///
/// Bounds the *output* and produces a clear, non-retryable error rather
/// than a pathological write; it does not bound the peak allocation (see
/// `MAX_WORKBOOK_BYTES`). In the spirit of `archive::EXTRACTION_RATIO_LIMIT`
/// -- generous headroom over anything legitimate, while still catching a
/// sheet whose declared range is absurd.
const MAX_SHEET_CELLS: usize = 50_000_000;

/// One sheet's converted CSV, ready to be placed.
///
/// `index` is 1-based and is the workbook's own sheet order; `total` counts
/// only the non-empty sheets, so both halves of `placement::
/// sheet_destination_name`'s `{nn}of{tt}` marker come from here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SheetOutput {
    pub index: usize,
    pub total: usize,
    pub sheet_name: String,
    pub path: PathBuf,
}

/// Confirms `soffice` is on `PATH`, for a `--input-file-type numbers` run
/// only (ADR-0149 §6).
///
/// Unlike `media::check_ffmpeg_available`, this checks the **exit status**
/// and not merely that the process spawned. That function's
/// `.output().await.map_err(..)` catches only spawn failure, so a
/// present-but-broken binary passes it; not replicating that here.
pub(crate) async fn check_soffice_available() -> Result<(), String> {
    let output = tokio::process::Command::new("soffice")
        .arg("--version")
        .output()
        .await
        .map_err(|_| {
            "'soffice' was not found on PATH -- required for \
             'pigeon job run transform --input-file-type numbers' to read Apple Numbers files"
                .to_string()
        })?;
    if !output.status.success() {
        return Err(format!(
            "'soffice --version' failed (exit {:?}) -- required for \
             'pigeon job run transform --input-file-type numbers'. stderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

/// Converts an Apple Numbers file to `.xlsx` inside `work_dir`, returning
/// the produced path (ADR-0149 §4).
///
/// The output keeps a real `.xlsx` extension, which matters twice over:
/// `calamine::open_workbook_auto` dispatches on the extension, and this is
/// why the ADR-0115 scratch-suffix trap that forces every `ffmpeg` call to
/// pass an explicit `-f` muxer does not apply here.
pub(crate) async fn convert_numbers_to_xlsx(
    input: &Path,
    work_dir: &Path,
) -> Result<PathBuf, String> {
    std::fs::create_dir_all(work_dir)
        .map_err(|err| format!("failed to create {}: {err}", work_dir.display()))?;
    let profile_dir = work_dir.join("lo-profile");

    // `-env:UserInstallation` is not optional. Without it every concurrent
    // `soffice` shares one profile directory, and they either serialise on
    // its lock or fail outright -- and this job runs `--concurrency` files
    // at once.
    let mut command = tokio::process::Command::new("soffice");
    command
        .arg("--headless")
        .arg(format!(
            "-env:UserInstallation=file://{}",
            profile_dir.display()
        ))
        .args(["--convert-to", "xlsx", "--outdir"])
        .arg(work_dir)
        .arg(input);

    let output = match tokio::time::timeout(SOFFICE_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(err)) => return Err(format!("failed to run soffice: {err}")),
        Err(_) => {
            return Err(format!(
                "soffice timed out after {}s converting {} -- {NOT_A_WORKBOOK_SIGNATURE} this run \
                 can process",
                SOFFICE_TIMEOUT.as_secs(),
                input.display()
            ));
        }
    };

    let stem = input
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| format!("{} has no usable file stem", input.display()))?;
    let produced = work_dir.join(format!("{stem}.xlsx"));

    // The exit code is deliberately not the authority: `soffice` exits 0 on
    // some conversion failures, printing its complaint to stdout. What the
    // next stage actually needs is a non-empty file where it expects one.
    let produced_len = std::fs::metadata(&produced).map(|meta| meta.len()).ok();
    match produced_len {
        Some(len) if len > 0 => Ok(produced),
        _ => Err(format!(
            "soffice produced no usable .xlsx for {} (exit {:?}) -- \
             {NOT_A_WORKBOOK_SIGNATURE}. stdout: {} stderr: {}",
            input.display(),
            output.status.code(),
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

/// Splits a workbook into one CSV per non-empty sheet, written into
/// `work_dir` and named by `index`, returning them in workbook order
/// (ADR-0149 §4).
///
/// `calamine` parsing is CPU-bound and blocking, so the whole conversion
/// runs inside `spawn_blocking` (ADR-0090).
pub(crate) async fn split_workbook_to_csv(
    input: &Path,
    work_dir: &Path,
) -> Result<Vec<SheetOutput>, String> {
    let input = input.to_path_buf();
    let work_dir = work_dir.to_path_buf();
    tokio::task::spawn_blocking(move || split_workbook_blocking(&input, &work_dir))
        .await
        .map_err(|err| format!("spreadsheet conversion task failed: {err}"))?
}

fn split_workbook_blocking(input: &Path, work_dir: &Path) -> Result<Vec<SheetOutput>, String> {
    let size = std::fs::metadata(input)
        .map(|meta| meta.len())
        .map_err(|err| format!("failed to stat {}: {err}", input.display()))?;
    if size > MAX_WORKBOOK_BYTES {
        return Err(format!(
            "{} is {size} bytes, over this job's {MAX_WORKBOOK_BYTES}-byte workbook limit -- \
             reading it would have to hold a whole sheet in memory at once",
            input.display()
        ));
    }

    std::fs::create_dir_all(work_dir)
        .map_err(|err| format!("failed to create {}: {err}", work_dir.display()))?;

    let mut workbook = calamine::open_workbook_auto(input)
        .map_err(|err| format!("{} {NOT_A_WORKBOOK_SIGNATURE}: {err}", input.display()))?;

    // Sheet order is the workbook's own; it is the only place that ordering
    // survives into the output, so it is never sorted or deduplicated.
    let sheet_names = workbook.sheet_names();
    let mut ranges: Vec<(String, Range<Data>)> = Vec::new();
    for name in sheet_names {
        let range = workbook.worksheet_range(&name).map_err(|err| {
            format!(
                "failed to read sheet '{name}' of {}: {err}",
                input.display()
            )
        })?;
        // An empty sheet is skipped and excluded from the total, which keeps
        // the total a deterministic function of file content -- all §3's
        // completeness check needs.
        if range.is_empty() {
            tracing::debug!(sheet = %name, "transform: skipping empty sheet");
            continue;
        }
        let (rows, columns) = range.get_size();
        let cells = rows.saturating_mul(columns);
        if cells > MAX_SHEET_CELLS {
            return Err(format!(
                "sheet '{name}' of {} declares {rows}x{columns} = {cells} cells, over this job's \
                 {MAX_SHEET_CELLS}-cell limit",
                input.display()
            ));
        }
        ranges.push((name, range));
    }

    // A `0of0` group would make ADR-0149 §3's completeness check ambiguous,
    // so a workbook with nothing in it is a failure rather than a silent
    // no-op producing no outputs.
    if ranges.is_empty() {
        return Err(format!(
            "{} has no non-empty sheets -- {NOT_A_WORKBOOK_SIGNATURE} with any data to extract",
            input.display()
        ));
    }

    let total = ranges.len();
    let mut outputs = Vec::with_capacity(total);
    for (position, (sheet_name, range)) in ranges.into_iter().enumerate() {
        let index = position + 1;
        let path = work_dir.join(format!("sheet-{index:02}.csv"));
        let expected_rows = range.get_size().0;
        let written_rows = write_range_csv(&range, &path)?;

        // The structural analogue of the pixel-area and duration guards
        // every `media` converter has. A CSV has no intrinsic invariant like
        // a duration, so what is checked is that the output accounts for the
        // input: every row of the used range reached the file.
        if written_rows != expected_rows {
            return Err(format!(
                "sheet '{sheet_name}' of {} has {expected_rows} row(s) but {written_rows} were \
                 written to {}",
                input.display(),
                path.display()
            ));
        }
        let written_len = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
        if written_len == 0 {
            return Err(format!(
                "sheet '{sheet_name}' of {} produced an empty {}",
                input.display(),
                path.display()
            ));
        }
        outputs.push(SheetOutput {
            index,
            total,
            sheet_name,
            path,
        });
    }

    if outputs.len() != total {
        return Err(format!(
            "{} produced {} CSV(s) for {total} non-empty sheet(s)",
            input.display(),
            outputs.len()
        ));
    }
    Ok(outputs)
}

/// Writes one sheet's used range to `path`, returning the row count.
fn write_range_csv(range: &Range<Data>, path: &Path) -> Result<usize, String> {
    let file = std::fs::File::create(path)
        .map_err(|err| format!("failed to create {}: {err}", path.display()))?;
    let mut writer = csv::Writer::from_writer(std::io::BufWriter::new(file));
    let mut rows = 0;
    for row in range.rows() {
        let fields: Vec<String> = row.iter().map(render_cell).collect();
        writer
            .write_record(&fields)
            .map_err(|err| format!("failed to write a row to {}: {err}", path.display()))?;
        rows += 1;
    }
    writer
        .flush()
        .map_err(|err| format!("failed to flush {}: {err}", path.display()))?;
    Ok(rows)
}

/// Renders one cell as CSV text.
///
/// The load-bearing case is `Data::DateTime`. `Display for ExcelDateTime`
/// writes the **bare serial**, so the obvious `cell.to_string()` emits
/// `45234` where the sheet shows a date -- the single likeliest silent
/// corruption in this whole path. `to_ymd_hms_milli` is used instead: it is
/// ungated (no `chrono`/`dates` feature needed) and applies the workbook's
/// 1904-vs-1900 epoch itself, which matters because that flag is private
/// with no accessor, so converting from `as_f64()` by hand would silently
/// shift a Mac-authored workbook by roughly four years.
///
/// A formula cell yields its **cached** value, which is what `calamine`
/// reads from the file; formulas are never re-evaluated.
fn render_cell(cell: &Data) -> String {
    match cell {
        Data::Empty => String::new(),
        Data::String(value) => value.clone(),
        Data::Int(value) => value.to_string(),
        Data::Float(value) => value.to_string(),
        Data::Bool(value) => value.to_string(),
        // Already ISO-8601 in the file; passed through verbatim.
        Data::DateTimeIso(value) | Data::DurationIso(value) => value.clone(),
        Data::DateTime(value) => {
            if value.is_duration() {
                // An elapsed time, not a date -- `to_ymd_hms_milli` would be
                // meaningless. Rendered as Excel displays it.
                render_duration(value.as_f64())
            } else {
                let (year, month, day, hour, minute, second, _milli) = value.to_ymd_hms_milli();
                if hour == 0 && minute == 0 && second == 0 {
                    format!("{year:04}-{month:02}-{day:02}")
                } else {
                    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}")
                }
            }
        }
        Data::Error(err) => {
            tracing::warn!(error = %err, "transform: spreadsheet cell holds an error value");
            String::new()
        }
    }
}

/// Formats a fractional-day duration as `HH:MM:SS`, which is how Excel
/// displays one. Hours are not wrapped at 24 -- a duration of 30 hours is
/// `30:00:00`, not `06:00:00`.
fn render_duration(days: f64) -> String {
    let total_seconds = (days * 86_400.0).round().max(0.0) as u64;
    let hours = total_seconds / 3_600;
    let minutes = (total_seconds % 3_600) / 60;
    let seconds = total_seconds % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// Builds a minimal but genuinely valid `.xlsx` from `sheets`
    /// (name, rows-of-cells) using the `zip` crate already in this tree --
    /// `ZipWriter` is unconditionally exported, so no dev-dependency is
    /// needed. Mirrors `worker.rs`'s `generate_test_png` in spirit: the
    /// fixture is constructed by the test rather than committed as a binary.
    ///
    /// Four parts is all `calamine`'s reader needs: `_rels/.rels` to find
    /// the workbook, `xl/workbook.xml` for the sheet names and order,
    /// `xl/_rels/workbook.xml.rels` to resolve each to its part, and the
    /// sheet parts themselves. Cell values are written as inline strings or
    /// raw numbers, so no `sharedStrings.xml` is required.
    fn write_xlsx(path: &Path, sheets: &[(&str, Vec<Vec<&str>>)]) {
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();

        zip.start_file("[Content_Types].xml", opts).unwrap();
        zip.write_all(
            br#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/></Types>"#,
        )
        .unwrap();

        zip.start_file("_rels/.rels", opts).unwrap();
        zip.write_all(
            br#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#,
        )
        .unwrap();

        let mut workbook = String::from(
            r#"<?xml version="1.0"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets>"#,
        );
        let mut rels = String::from(
            r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
        );
        for (position, (name, _)) in sheets.iter().enumerate() {
            let id = position + 1;
            workbook.push_str(&format!(
                r#"<sheet name="{name}" sheetId="{id}" r:id="rId{id}"/>"#
            ));
            rels.push_str(&format!(
                r#"<Relationship Id="rId{id}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet{id}.xml"/>"#
            ));
        }
        workbook.push_str("</sheets></workbook>");
        rels.push_str("</Relationships>");

        zip.start_file("xl/workbook.xml", opts).unwrap();
        zip.write_all(workbook.as_bytes()).unwrap();
        zip.start_file("xl/_rels/workbook.xml.rels", opts).unwrap();
        zip.write_all(rels.as_bytes()).unwrap();

        for (position, (_, rows)) in sheets.iter().enumerate() {
            let id = position + 1;
            let mut sheet = String::from(
                r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>"#,
            );
            for (row_index, row) in rows.iter().enumerate() {
                let row_number = row_index + 1;
                sheet.push_str(&format!(r#"<row r="{row_number}">"#));
                for (column_index, value) in row.iter().enumerate() {
                    let column = (b'A' + column_index as u8) as char;
                    let reference = format!("{column}{row_number}");
                    // A bare number is written as a numeric cell; anything
                    // else as an inline string, which avoids needing a
                    // shared-strings part.
                    if value.parse::<f64>().is_ok() {
                        sheet.push_str(&format!(r#"<c r="{reference}"><v>{value}</v></c>"#));
                    } else {
                        let escaped = value
                            .replace('&', "&amp;")
                            .replace('<', "&lt;")
                            .replace('>', "&gt;");
                        sheet.push_str(&format!(
                            r#"<c r="{reference}" t="inlineStr"><is><t>{escaped}</t></is></c>"#
                        ));
                    }
                }
                sheet.push_str("</row>");
            }
            sheet.push_str("</sheetData></worksheet>");
            zip.start_file(format!("xl/worksheets/sheet{id}.xml"), opts)
                .unwrap();
            zip.write_all(sheet.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }

    /// The headline case: three sheets, one of them empty, yields exactly
    /// two CSVs numbered against the non-empty total and in workbook order.
    #[tokio::test]
    async fn an_empty_sheet_is_skipped_and_excluded_from_the_total() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("Budget.xlsx");
        write_xlsx(
            &input,
            &[
                ("Summary", vec![vec!["a", "b"], vec!["1", "2"]]),
                ("Blank", vec![]),
                ("Notes", vec![vec!["hello"]]),
            ],
        );

        let outputs = split_workbook_to_csv(&input, &dir.path().join("work"))
            .await
            .unwrap();

        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].sheet_name, "Summary");
        assert_eq!((outputs[0].index, outputs[0].total), (1, 2));
        assert_eq!(outputs[1].sheet_name, "Notes");
        assert_eq!((outputs[1].index, outputs[1].total), (2, 2));

        assert_eq!(
            std::fs::read_to_string(&outputs[0].path).unwrap(),
            "a,b\n1,2\n"
        );
        assert_eq!(
            std::fs::read_to_string(&outputs[1].path).unwrap(),
            "hello\n"
        );
    }

    #[tokio::test]
    async fn a_single_sheet_workbook_reports_one_of_one() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("Solo.xlsx");
        write_xlsx(&input, &[("Only", vec![vec!["x"]])]);

        let outputs = split_workbook_to_csv(&input, &dir.path().join("work"))
            .await
            .unwrap();
        assert_eq!(outputs.len(), 1);
        assert_eq!((outputs[0].index, outputs[0].total), (1, 1));
    }

    /// A cell holding a comma, a quote, or a newline must be quoted rather
    /// than breaking the row -- the reason this uses the `csv` crate instead
    /// of a hand-rolled writer.
    #[tokio::test]
    async fn a_cell_needing_quoting_is_quoted() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("Quoting.xlsx");
        write_xlsx(
            &input,
            &[("S", vec![vec!["a,b", "say \"hi\"", "one\ntwo"]])],
        );

        let outputs = split_workbook_to_csv(&input, &dir.path().join("work"))
            .await
            .unwrap();
        let body = std::fs::read_to_string(&outputs[0].path).unwrap();
        assert!(body.contains("\"a,b\""), "{body:?}");
        assert!(body.contains("\"say \"\"hi\"\"\""), "{body:?}");
        assert!(body.contains("\"one\ntwo\""), "{body:?}");
    }

    /// A `0of0` group would make ADR-0149 §3's completeness check
    /// ambiguous, so a workbook with no data is a failure rather than a
    /// silent no-op producing nothing.
    #[tokio::test]
    async fn a_workbook_with_no_non_empty_sheets_fails_non_retryably() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("Empty.xlsx");
        write_xlsx(&input, &[("A", vec![]), ("B", vec![])]);

        let err = split_workbook_to_csv(&input, &dir.path().join("work"))
            .await
            .unwrap_err();
        assert!(err.contains("no non-empty sheets"), "{err}");
        assert!(super::super::media::is_non_retryable_transcode_error(&err));
        assert!(super::super::media::is_wrong_input_kind_error(&err));
    }

    #[tokio::test]
    async fn a_file_that_is_not_a_workbook_fails_non_retryably() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("Truncated.xlsx");
        std::fs::write(&input, b"this is not a zip, let alone a workbook").unwrap();

        let err = split_workbook_to_csv(&input, &dir.path().join("work"))
            .await
            .unwrap_err();
        assert!(err.contains(NOT_A_WORKBOOK_SIGNATURE), "{err}");
        // Classified both ways: retrying cannot help (ADR-0121), and the
        // local source copy has no diagnostic value (ADR-0143).
        assert!(super::super::media::is_non_retryable_transcode_error(&err));
        assert!(super::super::media::is_wrong_input_kind_error(&err));
    }

    #[tokio::test]
    async fn a_missing_input_fails() {
        let dir = tempfile::tempdir().unwrap();
        let err = split_workbook_to_csv(&dir.path().join("nope.xlsx"), dir.path())
            .await
            .unwrap_err();
        assert!(err.contains("failed to stat"), "{err}");
    }

    #[test]
    fn a_date_cell_renders_iso_8601_not_a_bare_serial() {
        // 45234 is 2023-11-04 in the 1900 system. `Display` would emit the
        // serial itself, which is the silent corruption this guards against.
        let cell = Data::DateTime(calamine::ExcelDateTime::new(
            45234.0,
            calamine::ExcelDateTimeType::DateTime,
            false,
        ));
        assert_eq!(render_cell(&cell), "2023-11-04");
        assert_eq!(cell.to_string(), "45234");
    }

    #[test]
    fn a_date_cell_with_a_time_component_keeps_it() {
        let cell = Data::DateTime(calamine::ExcelDateTime::new(
            45234.5,
            calamine::ExcelDateTimeType::DateTime,
            false,
        ));
        assert_eq!(render_cell(&cell), "2023-11-04T12:00:00");
    }

    /// The 1904 epoch is applied by `to_ymd_hms_milli` itself. This matters
    /// because the flag is private with no accessor, so a hand-rolled
    /// conversion from `as_f64()` would shift a Mac-authored workbook by
    /// roughly four years.
    #[test]
    fn the_1904_date_system_is_honoured() {
        let serial = 45234.0;
        let nineteen_hundred = Data::DateTime(calamine::ExcelDateTime::new(
            serial,
            calamine::ExcelDateTimeType::DateTime,
            false,
        ));
        let nineteen_oh_four = Data::DateTime(calamine::ExcelDateTime::new(
            serial,
            calamine::ExcelDateTimeType::DateTime,
            true,
        ));
        assert_ne!(
            render_cell(&nineteen_hundred),
            render_cell(&nineteen_oh_four),
            "the two epochs must not render identically"
        );
        // 1462 days -- four years and a day -- ahead of the 1900 answer.
        assert_eq!(render_cell(&nineteen_oh_four), "2027-11-05");
    }

    /// A duration is elapsed time, not a date, so `to_ymd_hms_milli` would
    /// be meaningless for it. Hours are not wrapped at 24.
    #[test]
    fn a_duration_cell_renders_as_elapsed_time() {
        let cell = Data::DateTime(calamine::ExcelDateTime::new(
            1.5,
            calamine::ExcelDateTimeType::TimeDelta,
            false,
        ));
        assert_eq!(render_cell(&cell), "36:00:00");
        assert_eq!(render_duration(0.0), "00:00:00");
        assert_eq!(render_duration(1.0 / 24.0), "01:00:00");
        assert_eq!(render_duration(90.0 / 1440.0), "01:30:00");
    }

    #[test]
    fn ordinary_cell_kinds_render_directly() {
        assert_eq!(render_cell(&Data::Empty), "");
        assert_eq!(render_cell(&Data::String("hi".into())), "hi");
        assert_eq!(render_cell(&Data::Int(42)), "42");
        assert_eq!(render_cell(&Data::Bool(true)), "true");
        assert_eq!(render_cell(&Data::Bool(false)), "false");
        // Already ISO in the file: passed through untouched.
        assert_eq!(
            render_cell(&Data::DateTimeIso("2026-10-14T01:02:03".into())),
            "2026-10-14T01:02:03"
        );
        // An error cell becomes empty rather than leaking `#REF!` into data.
        assert_eq!(render_cell(&Data::Error(calamine::CellErrorType::Ref)), "");
    }

    #[tokio::test]
    async fn check_soffice_available_reports_a_missing_binary_clearly() {
        // Only meaningful where `soffice` is genuinely absent; where it is
        // installed this asserts the success path instead.
        match check_soffice_available().await {
            Ok(()) => {}
            Err(err) => assert!(err.contains("soffice"), "{err}"),
        }
    }

    #[tokio::test]
    async fn convert_numbers_to_xlsx_fails_clearly_without_a_usable_output() {
        if check_soffice_available().await.is_err() {
            eprintln!("skipping: soffice not found on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("NotReally.numbers");
        std::fs::write(&input, b"not an iwa bundle").unwrap();

        let err = convert_numbers_to_xlsx(&input, &dir.path().join("work"))
            .await
            .unwrap_err();
        assert!(err.contains(NOT_A_WORKBOOK_SIGNATURE), "{err}");
        assert!(super::super::media::is_non_retryable_transcode_error(&err));
    }
}
