//! One place to run `soffice` (LibreOffice) headlessly (ADR-0154 §6).
//!
//! ADR-0149 introduced this machinery inside `sheets.rs` to convert
//! `.numbers` to `.xlsx`. ADR-0154 added six document kinds that convert to
//! `.pdf` the same way, so it is parameterized by target extension and
//! lives here. Two consumers rather than the three this repo's hoisting
//! precedent asks for, deliberately: this is a parameterization of one
//! existing function, not a speculative generalization, and the
//! alternative was a byte-for-byte copy differing in one string literal.
//!
//! Everything hardened about it is load-bearing and is preserved here: the
//! per-invocation profile directory, the timeout, and treating the exit
//! code as *not* authoritative.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long `soffice` gets before being treated as hung.
///
/// Unlike every `ffmpeg` call in this job, which has no timeout at all,
/// LibreOffice's documented failure mode is hanging rather than exiting
/// non-zero -- it is a desktop application running headless, and a
/// half-initialised profile can leave it waiting forever. A per-file cap is
/// the difference between one failed file and a run that never ends.
const SOFFICE_TIMEOUT: Duration = Duration::from_secs(300);

/// Confirms `soffice` is on `PATH`, for the input kinds that need it.
///
/// Unlike `media::check_ffmpeg_available`, this checks the **exit status**
/// and not merely that the process spawned. That function's
/// `.output().await.map_err(..)` catches only spawn failure, so a
/// present-but-broken binary passes it; not replicating that here
/// ([#100](https://github.com/noisypigeon/pigeon-cli/issues/100)).
pub(crate) async fn check_available() -> Result<(), String> {
    let output = tokio::process::Command::new("soffice")
        .arg("--version")
        .output()
        .await
        .map_err(|_| {
            "'soffice' was not found on PATH -- required for \
             'pigeon job run transform' with an Apple Numbers or document input kind \
             (doc, docx, pages, rtf, pptx, key)"
                .to_string()
        })?;
    if !output.status.success() {
        return Err(format!(
            "'soffice --version' failed (exit {:?}) -- required for \
             'pigeon job run transform' with an Apple Numbers or document input kind. stderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

/// Converts `input` to `target_ext` inside `work_dir`, returning the
/// produced path.
///
/// `signature` is folded into the failure message so the caller's
/// wrong-input-kind classification works: `sheets` passes
/// `NOT_A_WORKBOOK_SIGNATURE`, `documents` passes
/// `NOT_A_DOCUMENT_SIGNATURE`, and both are matched by
/// `media::is_wrong_input_kind_error`.
///
/// The output keeps a real extension, which matters twice over:
/// `calamine::open_workbook_auto` dispatches on it for the `.xlsx` route,
/// and it is why the ADR-0115 scratch-suffix trap that forces every
/// `ffmpeg` call to pass an explicit `-f` muxer does not apply here --
/// `soffice` derives the output name itself from the input stem.
pub(crate) async fn convert(
    input: &Path,
    work_dir: &Path,
    target_ext: &str,
    signature: &str,
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
        .args(["--convert-to", target_ext, "--outdir"])
        .arg(work_dir)
        .arg(input);

    let output = match tokio::time::timeout(SOFFICE_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(err)) => return Err(format!("failed to run soffice: {err}")),
        Err(_) => {
            return Err(format!(
                "soffice timed out after {}s converting {} -- {signature} this run can process",
                SOFFICE_TIMEOUT.as_secs(),
                input.display()
            ));
        }
    };

    let stem = input
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| format!("{} has no usable file stem", input.display()))?;
    let produced = work_dir.join(format!("{stem}.{target_ext}"));

    // The exit code is deliberately not the authority: `soffice` exits 0 on
    // some conversion failures, printing its complaint to stdout. What the
    // next stage actually needs is a non-empty file where it expects one.
    //
    // ADR-0154 found how far that cuts: given a PEM private key named
    // `.key`, soffice exits 0 *and* produces a large, well-formed PDF with
    // the key legible in it. No output check can catch that, which is why
    // `documents::ensure_keynote` guards the **input** instead.
    let produced_len = std::fs::metadata(&produced).map(|meta| meta.len()).ok();
    match produced_len {
        Some(len) if len > 0 => Ok(produced),
        _ => Err(format!(
            "soffice produced no usable .{target_ext} for {} (exit {:?}) -- \
             {signature}. stdout: {} stderr: {}",
            input.display(),
            output.status.code(),
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn check_available_reports_a_missing_binary_clearly() {
        // Only meaningful where `soffice` is genuinely absent; where it is
        // installed this asserts the success path instead.
        match check_available().await {
            Ok(()) => {}
            Err(err) => assert!(err.contains("soffice"), "{err}"),
        }
    }

    #[tokio::test]
    async fn convert_fails_clearly_without_a_usable_output() {
        if check_available().await.is_err() {
            eprintln!("skipping: soffice not found on PATH");
            return;
        }
        let temp = tempfile::tempdir().expect("tempdir");
        let missing = temp.path().join("nope.docx");
        let err = convert(
            &missing,
            &temp.path().join("work"),
            "pdf",
            "is not a readable document",
        )
        .await
        .expect_err("a missing input cannot convert");
        assert!(err.contains("is not a readable document"), "{err}");
    }

    /// The parameterization ADR-0154 §6 exists for: the same runner
    /// produces a different target format purely from its argument.
    #[tokio::test]
    async fn convert_produces_the_requested_target_extension() {
        if check_available().await.is_err() {
            eprintln!("skipping: soffice not found on PATH");
            return;
        }
        let temp = tempfile::tempdir().expect("tempdir");
        let input = temp.path().join("fixture.rtf");
        std::fs::write(
            &input,
            br"{\rtf1\ansi\deff0 Hello from a Rich Text Format fixture.\par}",
        )
        .expect("write rtf");

        let produced = convert(
            &input,
            &temp.path().join("work"),
            "pdf",
            "is not a readable document",
        )
        .await
        .expect("rtf converts to pdf");

        assert_eq!(produced.extension().and_then(|e| e.to_str()), Some("pdf"));
        let bytes = std::fs::read(&produced).expect("read pdf");
        assert!(bytes.starts_with(b"%PDF-"), "not a PDF: {:?}", &bytes[..8]);
    }
}
