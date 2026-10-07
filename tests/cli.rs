use std::fs;

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

fn pigeon() -> Command {
    Command::cargo_bin("pigeon").unwrap()
}

/// A `pigeon` invocation isolated to a throwaway `PIGEON_CONFIG_DIR`, so
/// tests never read or write the developer's real keyring metadata file.
fn pigeon_in(config_dir: &TempDir) -> Command {
    let mut cmd = pigeon();
    cmd.env("PIGEON_CONFIG_DIR", config_dir.path());
    cmd
}

/// Writes a fake `keyring.toml` entry directly (no `keyring add`/keychain
/// needed for the error paths these tests exercise, which fail before ever
/// reaching a secret lookup or a real IMAP/S3 connection). Appends, so
/// multiple calls build up a mixed store.
fn write_identity(config_dir: &TempDir, alias: &str, email: &str) {
    append_keyring_entry(
        config_dir,
        &format!(
            "[[entries]]\nkind = \"email\"\nalias = \"{alias}\"\nemail = \"{email}\"\nprovider = \"gmail\"\nhost = \"imap.gmail.com\"\nport = 993\n"
        ),
    );
}

fn write_bucket_config(config_dir: &TempDir, alias: &str) {
    append_keyring_entry(
        config_dir,
        &format!(
            "[[entries]]\nkind = \"bucket\"\nalias = \"{alias}\"\nendpoint = \"https://nyc3.digitaloceanspaces.com\"\nbucket = \"my-bucket\"\naccess_key_id = \"AKID\"\n"
        ),
    );
}

fn append_keyring_entry(config_dir: &TempDir, toml_block: &str) {
    let path = config_dir.path().join("keyring.toml");
    let existing = fs::read_to_string(&path).unwrap_or_default();
    fs::write(path, existing + toml_block).unwrap();
}

#[test]
fn top_level_help_lists_keyring_and_job_commands() {
    pigeon()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("keyring"))
        .stdout(predicate::str::contains("job"));
}

#[test]
fn keyring_help_lists_all_subcommands() {
    pigeon()
        .args(["keyring", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("add"))
        .stdout(predicate::str::contains("modify"))
        .stdout(predicate::str::contains("delete"))
        .stdout(predicate::str::contains("list"));
}

#[test]
fn keyring_add_help_lists_email_and_bucket() {
    pigeon()
        .args(["keyring", "add", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("email"))
        .stdout(predicate::str::contains("bucket"));
}

#[test]
fn keyring_add_email_help_lists_provider_and_custom_host_flags() {
    pigeon()
        .args(["keyring", "add", "email", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--provider"))
        .stdout(predicate::str::contains("--host"))
        .stdout(predicate::str::contains("--port"));
}

#[test]
fn keyring_add_bucket_help_shows_optional_alias() {
    pigeon()
        .args(["keyring", "add", "bucket", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("[ALIAS]"));
}

#[test]
fn keyring_list_on_empty_store_says_so() {
    let config_dir = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .args(["keyring", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No keyring entries configured."));
}

#[test]
fn log_file_flag_writes_valid_jsonl_with_the_command_name() {
    let config_dir = TempDir::new().unwrap();
    let log_dir = TempDir::new().unwrap();
    let log_file = log_dir.path().join("out.jsonl");

    pigeon_in(&config_dir)
        .args(["--log-file"])
        .arg(&log_file)
        .args(["keyring", "list"])
        .assert()
        .success();

    let contents = fs::read_to_string(&log_file).unwrap();
    assert!(!contents.trim().is_empty());

    let mut saw_command_name = false;
    for line in contents.lines() {
        let value: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|err| panic!("invalid JSON line {line:?}: {err}"));
        let text = value.to_string();
        if text.contains("keyring.list") {
            saw_command_name = true;
        }
    }
    assert!(
        saw_command_name,
        "expected at least one log line naming the keyring.list command, got: {contents}"
    );
}

#[test]
fn keyring_list_shows_both_kinds() {
    let config_dir = TempDir::new().unwrap();
    write_identity(&config_dir, "first-last", "first.last@example.com");
    write_bucket_config(&config_dir, "backup");

    pigeon_in(&config_dir)
        .args(["keyring", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("email"))
        .stdout(predicate::str::contains("first-last"))
        .stdout(predicate::str::contains("bucket"))
        .stdout(predicate::str::contains("backup"));
}

#[test]
fn keyring_add_email_custom_provider_without_host_fails_fast() {
    let config_dir = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .args([
            "keyring",
            "add",
            "email",
            "first.last@example.com",
            "--provider",
            "custom",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("--host is required"));
}

#[test]
fn keyring_add_email_failure_does_not_persist_an_entry() {
    let config_dir = TempDir::new().unwrap();

    // Nothing listens on 127.0.0.1:1 (a privileged, unassigned port), so
    // this deterministically fails at the connection step without ever
    // reaching a real IMAP server or the OS keychain.
    pigeon_in(&config_dir)
        .args([
            "keyring",
            "add",
            "email",
            "first.last@example.com",
            "--alias",
            "first-last",
            "--provider",
            "custom",
            "--host",
            "127.0.0.1",
            "--port",
            "1",
        ])
        .write_stdin("fake-secret\n")
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("failed to connect"));

    pigeon_in(&config_dir)
        .args(["keyring", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No keyring entries configured."));
}

#[test]
fn keyring_add_email_with_alias_already_used_by_a_bucket_config_is_rejected() {
    let config_dir = TempDir::new().unwrap();
    write_bucket_config(&config_dir, "shared-alias");

    // Proves alias uniqueness is enforced globally, across kinds, per
    // ADR-0022 -- not just within email identities the way it worked before.
    pigeon_in(&config_dir)
        .args([
            "keyring",
            "add",
            "email",
            "first.last@example.com",
            "--alias",
            "shared-alias",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "an entry named 'shared-alias' already exists",
        ));
}

#[test]
fn keyring_modify_with_unknown_alias_fails_fast() {
    let config_dir = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .args(["keyring", "modify", "no-such-alias"])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("no entry named 'no-such-alias'"));
}

#[test]
fn keyring_delete_with_unknown_alias_fails_fast() {
    let config_dir = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .args(["keyring", "delete", "no-such-alias"])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("no entry named 'no-such-alias'"));
}

#[test]
fn keyring_delete_declined_keeps_it() {
    let config_dir = TempDir::new().unwrap();
    write_bucket_config(&config_dir, "backup");

    pigeon_in(&config_dir)
        .args(["keyring", "delete", "backup"])
        .write_stdin("n\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("Cancelled."));

    let contents = fs::read_to_string(config_dir.path().join("keyring.toml")).unwrap();
    assert!(contents.contains("backup"));
}

/// Confirming removes the entry even though no keychain secret was ever
/// created for it (`write_bucket_config` bypasses `keyring add`) --
/// exercises `credentials::delete_secret`'s `NoEntry`-tolerant handling end
/// to end.
#[test]
fn keyring_delete_confirmed_deletes_it() {
    let config_dir = TempDir::new().unwrap();
    write_bucket_config(&config_dir, "backup");

    pigeon_in(&config_dir)
        .args(["keyring", "delete", "backup"])
        .write_stdin("y\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("Removed 'backup'."));

    let contents = fs::read_to_string(config_dir.path().join("keyring.toml")).unwrap();
    assert!(!contents.contains("backup"));
}

#[test]
fn job_help_lists_run_subcommand() {
    pigeon()
        .args(["job", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("run"));
}

#[test]
fn job_run_help_lists_email_sync() {
    pigeon()
        .args(["job", "run", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("email-sync"));
}

#[test]
fn job_run_email_sync_help_shows_identities_and_concurrency_flags() {
    pigeon()
        .args(["job", "run", "email-sync", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--identities"))
        .stdout(predicate::str::contains("--local-output"))
        .stdout(predicate::str::contains("--remote-output"))
        .stdout(predicate::str::contains("--concurrency"))
        .stdout(predicate::str::contains("--upload-concurrency"))
        .stdout(predicate::str::contains("--max-connections-per-identity"))
        .stdout(predicate::str::contains("--upload-only"))
        .stdout(predicate::str::contains("--report-bucket"))
        .stdout(predicate::str::contains("--yes"));
}

#[test]
fn job_run_email_sync_upload_only_without_a_completed_run_fails_fast() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();
    write_identity(&config_dir, "first-last", "first.last@example.com");

    // The identity resolves fine (it's configured), but its local output
    // tree was never populated: the per-identity preflight check
    // (ADR-0090) must skip it and, with no other identity selected, fail
    // the whole command -- before ever touching IMAP credentials or
    // --remote-output.
    pigeon_in(&config_dir)
        .args([
            "job",
            "run",
            "email-sync",
            "--upload-only",
            "--identities",
            "first-last",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--yes",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "no completed local run found for any selected identity",
        ));
}

#[test]
fn job_run_email_sync_without_identities_fails_fast_non_interactively() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .args([
            "job",
            "run",
            "email-sync",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--concurrency",
            "4",
            "--yes",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "--identities is required when not running interactively",
        ));
}

#[test]
fn job_run_email_sync_with_unknown_identity_fails_fast() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();
    write_identity(&config_dir, "first-last", "first.last@example.com");

    pigeon_in(&config_dir)
        .args([
            "job",
            "run",
            "email-sync",
            "--identities",
            "no-such-alias",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--concurrency",
            "4",
            "--yes",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "no identity with alias 'no-such-alias'",
        ));
}

#[test]
fn job_run_help_lists_email_pull() {
    pigeon()
        .args(["job", "run", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("email-pull"));
}

#[test]
fn job_run_email_pull_help_shows_identities_and_concurrency_flags() {
    pigeon()
        .args(["job", "run", "email-pull", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--identities"))
        .stdout(predicate::str::contains("--local-output"))
        .stdout(predicate::str::contains("--remote-output"))
        .stdout(predicate::str::contains("--concurrency"))
        .stdout(predicate::str::contains("--upload-concurrency"))
        .stdout(predicate::str::contains("--max-connections-per-identity"))
        .stdout(predicate::str::contains("--upload-only"))
        .stdout(predicate::str::contains("--report-bucket"))
        .stdout(predicate::str::contains("--yes"))
        // ADR-0081 §4: email-pull never offers encryption, at all.
        .stdout(predicate::str::contains("--encryption-key").not());
}

#[test]
fn job_run_email_pull_upload_only_without_a_completed_run_fails_fast() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();
    write_identity(&config_dir, "first-last", "first.last@example.com");

    // The identity resolves fine (it's configured), but its local output
    // tree was never populated: the per-identity preflight check
    // (ADR-0090) must skip it and, with no other identity selected, fail
    // the whole command -- before ever touching IMAP credentials or
    // --remote-output.
    pigeon_in(&config_dir)
        .args([
            "job",
            "run",
            "email-pull",
            "--upload-only",
            "--identities",
            "first-last",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--yes",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "no completed local run found for any selected identity",
        ));
}

#[test]
fn job_run_email_pull_without_identities_fails_fast_non_interactively() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .args([
            "job",
            "run",
            "email-pull",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--concurrency",
            "4",
            "--yes",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "--identities is required when not running interactively",
        ));
}

#[test]
fn job_run_email_pull_with_unknown_identity_fails_fast() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();
    write_identity(&config_dir, "first-last", "first.last@example.com");

    pigeon_in(&config_dir)
        .args([
            "job",
            "run",
            "email-pull",
            "--identities",
            "no-such-alias",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--concurrency",
            "4",
            "--yes",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "no identity with alias 'no-such-alias'",
        ));
}

#[test]
fn job_run_help_lists_decrypt_files() {
    pigeon()
        .args(["job", "run", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("decrypt-files"));
}

#[test]
fn job_run_decrypt_files_help_shows_input_output_and_key_flags() {
    pigeon()
        .args(["job", "run", "decrypt-files", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--input-dir"))
        .stdout(predicate::str::contains("--output-dir"))
        .stdout(predicate::str::contains("--encryption-key"))
        .stdout(predicate::str::contains("--concurrency"))
        .stdout(predicate::str::contains("--report-bucket"))
        .stdout(predicate::str::contains("--yes"))
        // ADR-0091: decrypt-files has no upload phase, so it never gets
        // --upload-concurrency.
        .stdout(predicate::str::contains("--upload-concurrency").not());
}

#[test]
fn job_run_decrypt_files_without_input_dir_fails_fast_non_interactively() {
    let config_dir = TempDir::new().unwrap();
    let output_dir = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .args([
            "job",
            "run",
            "decrypt-files",
            "--output-dir",
            output_dir.path().to_str().unwrap(),
            "--encryption-key",
            "primary",
            "--concurrency",
            "4",
            "--yes",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "--input-dir is required when not running interactively",
        ));
}

#[test]
fn job_run_decrypt_files_rejects_same_input_and_output_dir() {
    let config_dir = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .args([
            "job",
            "run",
            "decrypt-files",
            "--input-dir",
            dir.path().to_str().unwrap(),
            "--output-dir",
            dir.path().to_str().unwrap(),
            "--encryption-key",
            "primary",
            "--concurrency",
            "4",
            "--yes",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "--input-dir and --output-dir must not be the same directory",
        ));
}

#[test]
fn job_run_help_lists_pull_transform() {
    pigeon()
        .args(["job", "run", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("pull-transform"));
}

#[test]
fn job_run_pull_transform_help_shows_source_bucket_and_concurrency_flags() {
    pigeon()
        .args(["job", "run", "pull-transform", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--source-bucket"))
        .stdout(predicate::str::contains("--local-output"))
        .stdout(predicate::str::contains("--remote-output"))
        .stdout(predicate::str::contains("--encryption-key"))
        .stdout(predicate::str::contains("--concurrency"))
        .stdout(predicate::str::contains("--upload-concurrency"))
        .stdout(predicate::str::contains("--upload-only"))
        .stdout(predicate::str::contains("--report-bucket"));
}

#[test]
fn job_run_pull_transform_upload_only_without_a_completed_run_fails_fast() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();

    // No --source-bucket, no --remote-output, and no ffmpeg on PATH needed:
    // the preflight check (ADR-0090) must fail before any of those would
    // ever be resolved/checked, since this empty --local-output never ran
    // a pull-transform pass at all.
    pigeon_in(&config_dir)
        .args([
            "job",
            "run",
            "pull-transform",
            "--upload-only",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--yes",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "no completed pull-transform run found",
        ));
}

#[test]
fn job_run_pull_transform_without_source_bucket_fails_fast_non_interactively() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .args([
            "job",
            "run",
            "pull-transform",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--concurrency",
            "4",
            "--yes",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "--source-bucket is required when not running interactively",
        ));
}

#[test]
fn job_run_pull_transform_with_unknown_bucket_fails_fast() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .args([
            "job",
            "run",
            "pull-transform",
            "--source-bucket",
            "no-such-bucket",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--concurrency",
            "4",
            "--yes",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "no bucket-config named 'no-such-bucket'",
        ));
}

#[test]
fn job_run_help_lists_deduplicate() {
    pigeon()
        .args(["job", "run", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("deduplicate"));
}

#[test]
fn job_run_deduplicate_help_shows_source_bucket_and_concurrency_flags() {
    pigeon()
        .args(["job", "run", "deduplicate", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--source-bucket"))
        .stdout(predicate::str::contains("--local-output"))
        .stdout(predicate::str::contains("--remote-output"))
        .stdout(predicate::str::contains("--concurrency"))
        .stdout(predicate::str::contains("--upload-concurrency"))
        .stdout(predicate::str::contains("--upload-only"))
        .stdout(predicate::str::contains("--report-bucket"))
        .stdout(predicate::str::contains("--yes"))
        // ADR-0082 §0/§1: deduplicate never offers encryption or file-type/
        // zip-expansion selection -- every file is always processed and
        // every zip is always expanded.
        .stdout(predicate::str::contains("--encryption-key").not())
        .stdout(predicate::str::contains("--file-types").not())
        .stdout(predicate::str::contains("--expand-zips").not());
}

#[test]
fn job_run_deduplicate_without_source_bucket_fails_fast_non_interactively() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .args([
            "job",
            "run",
            "deduplicate",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--concurrency",
            "4",
            "--yes",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "--source-bucket is required when not running interactively",
        ));
}

#[test]
fn job_run_deduplicate_upload_only_without_a_completed_run_fails_fast() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();

    // No --source-bucket, no --remote-output: the preflight check (ADR-0089)
    // must fail before either would ever be resolved, since this empty
    // --local-output never ran a deduplicate pass at all.
    pigeon_in(&config_dir)
        .args([
            "job",
            "run",
            "deduplicate",
            "--upload-only",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--yes",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "no completed deduplicate run found",
        ));
}

#[test]
fn job_run_deduplicate_with_unknown_bucket_fails_fast() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .args([
            "job",
            "run",
            "deduplicate",
            "--source-bucket",
            "no-such-bucket",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--concurrency",
            "4",
            "--yes",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "no bucket-config named 'no-such-bucket'",
        ));
}

#[test]
fn job_run_pull_transform_without_ffmpeg_on_path_fails_fast_with_a_clear_error() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();
    let empty_path_dir = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .env("PATH", empty_path_dir.path())
        .args([
            "job",
            "run",
            "pull-transform",
            "--source-bucket",
            "backup",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--concurrency",
            "4",
            "--yes",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("was not found on PATH"));
}

#[test]
fn job_run_help_lists_import() {
    pigeon()
        .args(["job", "run", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("import"));
}

#[test]
fn job_run_import_help_shows_source_destination_and_report_bucket_flags() {
    pigeon()
        .args(["job", "run", "import", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--source"))
        .stdout(predicate::str::contains("--destination"))
        .stdout(predicate::str::contains("--local-output"))
        .stdout(predicate::str::contains("--report-bucket"))
        .stdout(predicate::str::contains("--yes"))
        // ADR-0101: import's rclone performance/retry flags are fixed, not
        // CLI flags, and it never resolves a pigeon bucket-config for
        // source/destination.
        .stdout(predicate::str::contains("--source-bucket").not())
        .stdout(predicate::str::contains("--concurrency").not())
        .stdout(predicate::str::contains("--encryption-key").not());
}

#[test]
fn job_run_import_without_source_fails_fast_non_interactively() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .args([
            "job",
            "run",
            "import",
            "--destination",
            "dest:",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--yes",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "--source is required when not running interactively",
        ));
}

#[test]
fn job_run_import_without_destination_fails_fast_non_interactively() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .args([
            "job",
            "run",
            "import",
            "--source",
            "source:media/",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--yes",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "--destination is required when not running interactively",
        ));
}

#[test]
fn job_run_import_without_rclone_on_path_fails_fast_with_a_clear_error() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();
    let empty_path_dir = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .env("PATH", empty_path_dir.path())
        .args([
            "job",
            "run",
            "import",
            "--source",
            "source:media/",
            "--destination",
            "dest:",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--yes",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("was not found on PATH"));
}
