use std::fs;
use std::path::{Path, PathBuf};

use futures::{StreamExt, stream};
use indicatif::{MultiProgress, ProgressBar};

use crate::commands::job::email_sync::sink;
use crate::core::crypto::{Aes256GcmSivEncryptor, Encryptor};
use crate::core::data::collect_files;

/// One file to decrypt: its ciphertext path under `--input-dir` and the
/// plaintext path it's written to under `--output-dir`.
pub(crate) struct DecryptTask {
    pub input_path: PathBuf,
    pub output_path: PathBuf,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct DecryptSummary {
    pub decrypted: usize,
    pub failed: usize,
}

/// Walks `input_dir` for `*.enc` files and builds the task list, mapping
/// each to its output path under `output_dir` with the `.enc` suffix
/// stripped and the relative directory structure preserved.
pub(crate) fn collect_decrypt_tasks(
    input_dir: &Path,
    output_dir: &Path,
) -> Result<Vec<DecryptTask>, String> {
    let mut tasks = Vec::new();
    for path in collect_files(input_dir)? {
        if path.extension().and_then(|ext| ext.to_str()) != Some("enc") {
            continue;
        }
        let relative = path
            .strip_prefix(input_dir)
            .map_err(|_| format!("{} is not under {}", path.display(), input_dir.display()))?;
        let output_path = output_dir.join(relative.with_extension(""));
        tasks.push(DecryptTask {
            input_path: path,
            output_path,
        });
    }
    Ok(tasks)
}

/// Reads, decrypts, and writes one file. `false` on any failure (bad key,
/// truncated, tampered ciphertext) -- warned about via
/// `multi_progress.println`, never fatal to the whole run.
async fn decrypt_one(
    task: DecryptTask,
    encryptor: &Aes256GcmSivEncryptor,
    bar: &ProgressBar,
    multi_progress: &MultiProgress,
) -> bool {
    let outcome = async {
        let data = fs::read(&task.input_path)
            .map_err(|err| format!("failed to read {}: {err}", task.input_path.display()))?;
        let plaintext = encryptor.decrypt(&data)?;
        if let Some(parent) = task.output_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
        }
        fs::write(&task.output_path, plaintext)
            .map_err(|err| format!("failed to write {}: {err}", task.output_path.display()))
    }
    .await;

    bar.inc(1);
    match outcome {
        Ok(()) => true,
        Err(err) => {
            let _ = multi_progress.println(format!(
                "Warning: failed to decrypt {}: {err}",
                task.input_path.display()
            ));
            false
        }
    }
}

/// Decrypts every task concurrently via `stream::buffer_unordered`,
/// mirroring `email_sync::worker::run_upload_phase`'s exact shape -- no
/// retry (local disk I/O, never transient), no per-identity bookkeeping
/// (decrypt has no `.uploaded`-style checkpoint concept).
pub(crate) async fn run_decrypt_phase(
    tasks: Vec<DecryptTask>,
    encryptor: &Aes256GcmSivEncryptor,
    concurrency: usize,
) -> DecryptSummary {
    if tasks.is_empty() {
        return DecryptSummary::default();
    }
    let multi_progress = MultiProgress::new();
    let bar = sink::new_progress_bar("decrypt".to_string(), tasks.len() as u64, &multi_progress);

    let summary = stream::iter(tasks)
        .map(|task| decrypt_one(task, encryptor, &bar, &multi_progress))
        .buffer_unordered(concurrency.max(1))
        .fold(DecryptSummary::default(), |mut summary, ok| async move {
            if ok {
                summary.decrypted += 1;
            } else {
                summary.failed += 1;
            }
            summary
        })
        .await;

    bar.finish();
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_HEX: &str = "0101010101010101010101010101010101010101010101010101010101010101";

    #[test]
    fn collect_decrypt_tasks_finds_enc_files_and_strips_suffix() {
        let input = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        fs::create_dir_all(input.path().join("mailbox")).unwrap();
        fs::write(input.path().join("mailbox/a.eml.enc"), b"ciphertext").unwrap();

        let tasks = collect_decrypt_tasks(input.path(), output.path()).unwrap();

        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].output_path, output.path().join("mailbox/a.eml"));
    }

    #[test]
    fn collect_decrypt_tasks_ignores_non_enc_files() {
        let input = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        fs::write(input.path().join("a.eml"), b"plaintext").unwrap();
        fs::write(input.path().join("b.eml.enc"), b"ciphertext").unwrap();

        let tasks = collect_decrypt_tasks(input.path(), output.path()).unwrap();

        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].output_path, output.path().join("b.eml"));
    }

    #[tokio::test]
    async fn run_decrypt_phase_reports_decrypted_and_failed_counts() {
        let input = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let encryptor = Aes256GcmSivEncryptor::from_hex_key(KEY_HEX).unwrap();

        let good_ciphertext = encryptor.encrypt(b"hello, pigeon").unwrap();
        fs::write(input.path().join("good.eml.enc"), good_ciphertext).unwrap();
        fs::write(input.path().join("bad.eml.enc"), b"not valid ciphertext").unwrap();

        let tasks = collect_decrypt_tasks(input.path(), output.path()).unwrap();
        let summary = run_decrypt_phase(tasks, &encryptor, 2).await;

        assert_eq!(
            summary,
            DecryptSummary {
                decrypted: 1,
                failed: 1,
            }
        );
        assert_eq!(
            fs::read(output.path().join("good.eml")).unwrap(),
            b"hello, pigeon"
        );
        assert!(!output.path().join("bad.eml").exists());
    }

    #[tokio::test]
    async fn run_decrypt_phase_is_a_no_op_for_an_empty_task_list() {
        let encryptor = Aes256GcmSivEncryptor::from_hex_key(KEY_HEX).unwrap();
        let summary = run_decrypt_phase(Vec::new(), &encryptor, 4).await;
        assert_eq!(summary, DecryptSummary::default());
    }
}
