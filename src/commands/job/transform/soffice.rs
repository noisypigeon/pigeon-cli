//! One place to run `soffice` (LibreOffice) headlessly (CLI-0154 §6).
//!
//! CLI-0149 introduced this machinery inside `sheets.rs` to convert
//! `.numbers` to `.xlsx`. CLI-0154 added six document kinds that convert to
//! `.pdf` the same way, so it is parameterized by target extension and
//! lives here. Two consumers rather than the three this repo's hoisting
//! precedent asks for, deliberately: this is a parameterization of one
//! existing function, not a speculative generalization, and the
//! alternative was a byte-for-byte copy differing in one string literal.
//!
//! Everything hardened about it is load-bearing and is preserved here: the
//! per-invocation profile directory, the timeout, and treating the exit
//! code as *not* authoritative.
//!
//! CLI-0158 adds two more. soffice reads both its profile and its
//! `--outdir` as URLs, and the work directory is named after the source
//! file, so both are percent-encoded: an unencoded space crashed it, a `#`
//! hung it, and a `%41` silently became `A`. And a timed-out soffice is
//! killed rather than orphaned.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::media::CONVERTER_CRASHED_SIGNATURE;

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
/// and it is why the CLI-0115 scratch-suffix trap that forces every
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
    let profile_url = directory_url(&profile_dir)?;
    let outdir_url = directory_url(work_dir)?;

    // `-env:UserInstallation` is not optional. Without it every concurrent
    // `soffice` shares one profile directory, and they either serialise on
    // its lock or fail outright -- and this job runs `--concurrency` files
    // at once.
    //
    // Both directories go in as encoded URLs (CLI-0158). The input file
    // does not need it: soffice treats a plain path argument as a path,
    // measured with a space, `#`, `%` and `:` in the name.
    let mut command = tokio::process::Command::new("soffice");
    command
        .arg("--headless")
        .arg(format!("-env:UserInstallation={profile_url}"))
        .args(["--convert-to", target_ext, "--outdir"])
        .arg(&outdir_url)
        .arg(input)
        // CLI-0158: without this, the timeout below drops the future and
        // leaves soffice running. Two runs' hung processes were still alive
        // half an hour later.
        .kill_on_drop(true);

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
    // CLI-0154 found how far that cuts: given a PEM private key named
    // `.key`, soffice exits 0 *and* produces a large, well-formed PDF with
    // the key legible in it. No output check can catch that, which is why
    // `documents::ensure_keynote` guards the **input** instead.
    let produced_len = std::fs::metadata(&produced).map(|meta| meta.len()).ok();
    if let Some(len) = produced_len
        && len > 0
    {
        return Ok(produced);
    }

    // A signal death is the converter failing, not the file (CLI-0158), so
    // it must not carry the caller's wrong-input signature: that would
    // delete the local copy under CLI-0143 and send the reader looking at
    // the wrong cause. It stays non-retryable, so a systemic crash still
    // trips the circuit breaker.
    if output.status.code().is_none() {
        return Err(format!(
            "soffice crashed ({}) converting {} -- {CONVERTER_CRASHED_SIGNATURE}. stderr: {}",
            termination_signal(&output.status),
            input.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    Err(format!(
        "soffice produced no usable .{target_ext} for {} (exit {:?}) -- \
         {signature}. stdout: {} stderr: {}",
        input.display(),
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).trim(),
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

/// `path` as a percent-encoded `file://` directory URL (CLI-0158).
///
/// Made absolute first, since a URL cannot express a relative path.
fn directory_url(path: &Path) -> Result<String, String> {
    let absolute = std::path::absolute(path)
        .map_err(|err| format!("failed to resolve {}: {err}", path.display()))?;
    url::Url::from_directory_path(&absolute)
        .map(String::from)
        .map_err(|()| format!("{} cannot be expressed as a file URL", absolute.display()))
}

#[cfg(unix)]
fn termination_signal(status: &std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match status.signal() {
        Some(signal) => format!("signal {signal}"),
        None => "no exit code".to_string(),
    }
}

#[cfg(not(unix))]
fn termination_signal(_status: &std::process::ExitStatus) -> String {
    "no exit code".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CLI-0158: each of these crashed, hung, or redirected soffice when
    /// passed unencoded.
    #[test]
    fn directory_url_percent_encodes_reserved_characters() {
        let url = directory_url(Path::new("/tmp/a b#1 (c)%41/lo-profile")).unwrap();
        assert_eq!(url, "file:///tmp/a%20b%231%20(c)%2541/lo-profile/");
        assert!(!url.contains(' ') && !url.contains('#'), "{url}");
    }

    #[test]
    fn directory_url_resolves_a_relative_path() {
        let url = directory_url(Path::new("relative/dir")).unwrap();
        assert!(url.starts_with("file:///"), "{url}");
        assert!(url.ends_with("/relative/dir/"), "{url}");
    }

    /// CLI-0158 §4: a crash stops retries and counts toward the breaker,
    /// but is not the file's fault, so its local copy is kept.
    #[test]
    fn a_crash_is_non_retryable_but_not_wrong_input_kind() {
        let err = format!(
            "soffice crashed (signal 6) converting /x.docx -- {CONVERTER_CRASHED_SIGNATURE}. stderr: boom"
        );
        assert!(super::super::media::is_non_retryable_transcode_error(&err));
        assert!(!super::super::media::is_wrong_input_kind_error(&err));
    }

    /// End to end against the real binary: a work directory carrying every
    /// character class that broke the docx run still converts, and the
    /// output lands where the caller expects it rather than at a decoded
    /// path.
    #[tokio::test]
    async fn convert_succeeds_in_a_work_dir_with_url_reserved_characters() {
        if check_available().await.is_err() {
            eprintln!("skipping: soffice not found on PATH");
            return;
        }
        let temp = tempfile::tempdir().expect("tempdir");
        let input = temp.path().join("fixture.rtf");
        std::fs::write(&input, br"{\rtf1\ansi\deff0 Reserved characters.\par}").unwrap();
        let work_dir = temp.path().join("a b#1 (c)%41");

        let produced = convert(&input, &work_dir, "pdf", "is not a readable document")
            .await
            .expect("converts despite the directory name");
        assert_eq!(produced, work_dir.join("fixture.pdf"));
        assert!(
            work_dir.join("lo-profile").is_dir(),
            "profile went elsewhere"
        );
        assert!(!temp.path().join("a b#1 (c)A").exists());
    }

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

    /// The parameterization CLI-0154 §6 exists for: the same runner
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
