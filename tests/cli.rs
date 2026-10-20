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

/// Writes a fake `keyring.toml` entry directly (no `keyring add` needed for
/// the error paths these tests exercise, which fail before ever reaching a
/// secret lookup or a real IMAP/S3 connection). Appends, so multiple calls
/// build up a mixed store. Includes a dummy secret field (CLI-0135: `secret`/
/// `secret_key` have no `#[serde(default)]`, so a real `keyring.toml` entry
/// always carries one).
fn write_identity(config_dir: &TempDir, alias: &str, email: &str) {
    append_keyring_entry(
        config_dir,
        &format!(
            "[[entries]]\nkind = \"email\"\nalias = \"{alias}\"\nemail = \"{email}\"\nprovider = \"gmail\"\nhost = \"imap.gmail.com\"\nport = 993\nsecret = \"fake-secret\"\n"
        ),
    );
}

fn write_bucket_config(config_dir: &TempDir, alias: &str) {
    append_keyring_entry(
        config_dir,
        &format!(
            "[[entries]]\nkind = \"bucket\"\nalias = \"{alias}\"\nendpoint = \"https://nyc3.digitaloceanspaces.com\"\nbucket = \"my-bucket\"\naccess_key_id = \"AKID\"\nsecret_key = \"fake-secret\"\n"
        ),
    );
}

fn append_keyring_entry(config_dir: &TempDir, toml_block: &str) {
    let path = config_dir.path().join("keyring.toml");
    let existing = fs::read_to_string(&path).unwrap_or_default();
    fs::write(path, existing + toml_block).unwrap();
}

/// Writes a bucket-config to an arbitrary `keyring.toml` outside any
/// `PIGEON_CONFIG_DIR`, for the `--keyring-path` tests (CLI-0145). Mirrors
/// `write_bucket_config`'s entry shape; takes a file path rather than a
/// config directory, which is exactly the distinction the flag draws.
///
/// The endpoint is an unroutable loopback port rather than
/// `write_bucket_config`'s plausible-looking URL, so that a test which
/// accidentally got past alias resolution would fail locally instead of
/// making a real network call to a third party. No test here should reach
/// one: rclone's `--retries 5 --low-level-retries 20` turns even a refused
/// connection into minutes of backoff.
fn write_bucket_config_at(keyring_path: &std::path::Path, alias: &str) {
    fs::write(
        keyring_path,
        format!(
            "[[entries]]\nkind = \"bucket\"\nalias = \"{alias}\"\nendpoint = \"http://127.0.0.1:1\"\nbucket = \"my-bucket\"\naccess_key_id = \"AKID\"\nsecret_key = \"fake-secret\"\n"
        ),
    )
    .unwrap();
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
        .stdout(predicate::str::contains("--port"))
        .stdout(predicate::str::contains("--security"));
}

#[test]
fn keyring_add_email_rejects_a_plaintext_security_mode() {
    // CLI-0162: there is deliberately no plaintext option.
    pigeon()
        .args([
            "keyring",
            "add",
            "email",
            "first.last@example.com",
            "--provider",
            "custom",
            "--host",
            "127.0.0.1",
            "--security",
            "plain",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid value 'plain'"));
}

#[test]
fn keyring_add_email_starttls_against_a_refusing_port_fails_without_persisting() {
    let config_dir = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .args([
            "keyring",
            "add",
            "email",
            "first.last@example.com",
            "--provider",
            "custom",
            "--host",
            "127.0.0.1",
            "--port",
            "1",
            "--security",
            "starttls",
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
        .args(["--no-metrics", "keyring", "list"])
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
    // reaching a real IMAP server.
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
    // CLI-0022 -- not just within email identities the way it worked before.
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

/// Confirming removes the entry from `keyring.toml` directly (CLI-0135: the
/// secret lives in the same file, so removing the entry removes the secret
/// too -- no separate keychain delete step).
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
        .stdout(predicate::str::contains("--destination-bucket"))
        .stdout(predicate::str::contains("--concurrency"))
        .stdout(predicate::str::contains("--transfers"))
        .stdout(predicate::str::contains("--checkers"))
        .stdout(predicate::str::contains("--tpslimit"))
        .stdout(predicate::str::contains("--upload-only"))
        .stdout(predicate::str::contains("--report-bucket"))
        .stdout(predicate::str::contains("--non-interactive"))
        // CLI-0136: upload is now one bulk `rclone copy`, tuned by
        // --transfers/--checkers/--tpslimit, not --upload-concurrency.
        .stdout(predicate::str::contains("--upload-concurrency").not())
        // CLI-0082 §0/§1: deduplicate never offers encryption or file-type/
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
            "--non-interactive",
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

    // No --source-bucket, no --destination-bucket: the preflight check
    // (CLI-0089) must fail before either would ever be resolved, since this
    // empty --local-output never ran a deduplicate pass at all.
    pigeon_in(&config_dir)
        .args([
            "job",
            "run",
            "deduplicate",
            "--upload-only",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--non-interactive",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "no completed deduplicate run found",
        ));
}

#[test]
fn job_run_deduplicate_accepts_a_repeated_source_bucket_flag() {
    // Two --source-bucket occurrences (CLI-0109) -- neither bucket exists,
    // so this just exercises that the repeated flag parses into a Vec and
    // every alias gets validated (the second, unknown one is what trips the
    // failure), not that a real multi-bucket run succeeds end-to-end.
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .args([
            "job",
            "run",
            "deduplicate",
            "--source-bucket",
            "no-such-bucket-a",
            "--source-bucket",
            "no-such-bucket-b",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--concurrency",
            "4",
            "--non-interactive",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "no bucket-config named 'no-such-bucket-a'",
        ));
}

#[test]
fn job_run_deduplicate_upload_only_without_destination_bucket_fails_fast() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();
    // A completed-looking prior run, so the preflight check passes and
    // --destination-bucket's own resolution is actually reached.
    fs::create_dir_all(local_output.path().join(".staging")).unwrap();
    fs::write(
        local_output.path().join(".staging").join(".processed"),
        b"bucket-a\ta.txt\n",
    )
    .unwrap();
    fs::create_dir_all(local_output.path().join("result")).unwrap();
    fs::write(local_output.path().join("result").join("a.txt"), b"a").unwrap();

    pigeon_in(&config_dir)
        .args([
            "job",
            "run",
            "deduplicate",
            "--upload-only",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--non-interactive",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "--destination-bucket is required with --upload-only",
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
            "--non-interactive",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "no bucket-config named 'no-such-bucket'",
        ));
}

#[test]
fn job_run_help_lists_rclone() {
    pigeon()
        .args(["job", "run", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("rclone"));
}

#[test]
fn job_run_rclone_help_lists_copy_and_delete() {
    pigeon()
        .args(["job", "run", "rclone", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("copy"))
        .stdout(predicate::str::contains("delete"));
}

#[test]
fn job_run_rclone_copy_help_shows_source_bucket_destination_bucket_and_report_bucket_flags() {
    pigeon()
        .args(["job", "run", "rclone", "copy", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--source-bucket"))
        .stdout(predicate::str::contains("--destination-bucket"))
        .stdout(predicate::str::contains("--local-output"))
        .stdout(predicate::str::contains("--report-bucket"))
        .stdout(predicate::str::contains("--transfers"))
        .stdout(predicate::str::contains("--checkers"))
        .stdout(predicate::str::contains("--tpslimit"))
        .stdout(predicate::str::contains("--non-interactive"))
        // CLI-0136: this job's rclone performance/retry flags beyond
        // transfers/checkers/tpslimit are fixed, not CLI flags.
        .stdout(predicate::str::contains("--source-path").not())
        .stdout(predicate::str::contains("--concurrency").not())
        .stdout(predicate::str::contains("--encryption-key").not());
}

#[test]
fn job_run_rclone_delete_help_shows_source_bucket_and_report_bucket_flags_but_not_destination_or_transfer_flags()
 {
    pigeon()
        .args(["job", "run", "rclone", "delete", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--source-bucket"))
        .stdout(predicate::str::contains("--local-output"))
        .stdout(predicate::str::contains("--report-bucket"))
        .stdout(predicate::str::contains("--checkers"))
        .stdout(predicate::str::contains("--non-interactive"))
        // CLI-0110: delete has no destination and no file-transfer tuning
        // -- `rclone purge` moves no file content.
        .stdout(predicate::str::contains("--destination-bucket").not())
        .stdout(predicate::str::contains("--transfers").not())
        .stdout(predicate::str::contains("--tpslimit").not());
}

#[test]
fn job_run_rclone_copy_without_source_bucket_fails_fast_non_interactively() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();
    write_bucket_config(&config_dir, "dest");

    pigeon_in(&config_dir)
        .args([
            "job",
            "run",
            "rclone",
            "copy",
            "--destination-bucket",
            "dest",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--non-interactive",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "--source-bucket is required when not running interactively",
        ));
}

#[test]
fn job_run_rclone_copy_without_destination_bucket_fails_fast_non_interactively() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();
    write_bucket_config(&config_dir, "source");

    pigeon_in(&config_dir)
        .args([
            "job",
            "run",
            "rclone",
            "copy",
            "--source-bucket",
            "source",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--non-interactive",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "--destination-bucket is required when not running interactively",
        ));
}

#[test]
fn job_run_rclone_copy_without_rclone_on_path_fails_fast_with_a_clear_error() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();
    let empty_path_dir = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .env("PATH", empty_path_dir.path())
        .args([
            "job",
            "run",
            "rclone",
            "copy",
            "--source-bucket",
            "source",
            "--destination-bucket",
            "dest",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--non-interactive",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("was not found on PATH"));
}

#[test]
fn job_run_rclone_delete_without_source_bucket_fails_fast_non_interactively() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .args([
            "job",
            "run",
            "rclone",
            "delete",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--non-interactive",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "--source-bucket is required when not running interactively",
        ));
}

#[test]
fn job_run_rclone_delete_without_rclone_on_path_fails_fast_with_a_clear_error() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();
    let empty_path_dir = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .env("PATH", empty_path_dir.path())
        .args([
            "job",
            "run",
            "rclone",
            "delete",
            "--source-bucket",
            "source",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--non-interactive",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("was not found on PATH"));
}

#[test]
fn job_run_help_lists_transform() {
    pigeon()
        .args(["job", "run", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("transform"));
}

#[test]
fn job_run_transform_help_shows_input_file_type_source_destination_and_rclone_flags() {
    pigeon()
        .args(["job", "run", "transform", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--input-file-type"))
        .stdout(predicate::str::contains("--source"))
        .stdout(predicate::str::contains("--destination"))
        .stdout(predicate::str::contains("--local-output"))
        .stdout(predicate::str::contains("--concurrency"))
        .stdout(predicate::str::contains("--download-transfers"))
        .stdout(predicate::str::contains("--download-checkers"))
        .stdout(predicate::str::contains("--tpslimit"))
        .stdout(predicate::str::contains("--report-bucket"))
        .stdout(predicate::str::contains("--non-interactive"))
        // CLI-0137: renamed from --source-bucket/--destination-bucket.
        .stdout(predicate::str::contains("--source-bucket").not())
        .stdout(predicate::str::contains("--destination-bucket").not())
        // CLI-0138: the composed form is documented on both flags.
        .stdout(predicate::str::contains("media:mp4/"))
        // CLI-0142: the audio-only input kind is listed.
        .stdout(predicate::str::contains("mp4-audio"))
        // CLI-0149: the four spreadsheet input kinds are listed. Matched
        // with their backticks so a bare `csv`/`xls` substring appearing
        // incidentally in future help text cannot satisfy this.
        .stdout(predicate::str::contains("`numbers`"))
        .stdout(predicate::str::contains("`xlsx`"))
        .stdout(predicate::str::contains("`xls`"))
        .stdout(predicate::str::contains("`csv`"))
        // CLI-0153: the five audio input kinds and their quality menu.
        .stdout(predicate::str::contains("`m4a`"))
        .stdout(predicate::str::contains("`mp3`"))
        .stdout(predicate::str::contains("`aac`"))
        .stdout(predicate::str::contains("`wav`"))
        .stdout(predicate::str::contains("`opus`"))
        .stdout(predicate::str::contains("--audio-quality"))
        .stdout(predicate::str::contains("`transparent`"))
        // CLI-0136: bucket-config aliases, not raw rclone path strings, and
        // no separate upload phase of the shape --upload-concurrency sizes.
        .stdout(predicate::str::contains("--source-path").not())
        .stdout(predicate::str::contains("--encryption-key").not())
        .stdout(predicate::str::contains("--upload-concurrency").not());
}

#[test]
fn job_run_transform_without_input_file_type_fails_fast_non_interactively() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .args([
            "job",
            "run",
            "transform",
            "--source",
            "source",
            "--destination",
            "dest",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--concurrency",
            "4",
            "--non-interactive",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "--input-file-type is required when not running interactively",
        ));
}

#[test]
fn job_run_transform_with_invalid_input_file_type_fails_fast() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .args([
            "job",
            "run",
            "transform",
            "--input-file-type",
            "gif",
            "--source",
            "source",
            "--destination",
            "dest",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--concurrency",
            "4",
            "--non-interactive",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("unknown --input-file-type 'gif'"));
}

#[test]
fn job_run_transform_without_source_fails_fast_non_interactively() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();
    write_bucket_config(&config_dir, "destination");

    // CLI-0138 reversed CLI-0137's alias inference: with no --source there
    // is no alias to guess, so the run fails fast rather than defaulting.
    pigeon_in(&config_dir)
        .args([
            "job",
            "run",
            "transform",
            "--input-file-type",
            "png",
            "--destination",
            "destination",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--concurrency",
            "4",
            "--non-interactive",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "--source is required when not running interactively",
        ));
}

#[test]
fn job_run_transform_without_destination_fails_fast_non_interactively() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();
    write_bucket_config(&config_dir, "source");

    pigeon_in(&config_dir)
        .args([
            "job",
            "run",
            "transform",
            "--input-file-type",
            "png",
            "--source",
            "source",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--concurrency",
            "4",
            "--non-interactive",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "--destination is required when not running interactively",
        ));
}

/// The exact regression CLI-0138 fixes: the composed `alias:prefix/` form
/// that CLI-0136 made unexpressible. Resolution must get past both bucket
/// flags (failing later only on the still-unresolved --report-bucket).
#[test]
fn job_run_transform_accepts_the_composed_alias_prefix_form() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();
    write_bucket_config(&config_dir, "source");
    write_bucket_config(&config_dir, "destination");

    pigeon_in(&config_dir)
        .args([
            "job",
            "run",
            "transform",
            "--input-file-type",
            "mp4",
            "--source",
            "source:mp4/",
            "--destination",
            "destination:mp4/",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--concurrency",
            "4",
            "--non-interactive",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("--source is required").not())
        .stderr(predicate::str::contains("--destination is required").not())
        .stderr(predicate::str::contains("no bucket-config named").not())
        .stderr(predicate::str::contains(
            "--report-bucket is required when not running interactively",
        ));
}

/// One bucket serving as both source and destination, distinguished only
/// by prefix -- impossible under CLI-0137's alias inference, which for a
/// video kind resolved both flags to the same `mp4` alias.
#[test]
fn job_run_transform_accepts_one_bucket_for_both_source_and_destination() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();
    write_bucket_config(&config_dir, "media");

    pigeon_in(&config_dir)
        .args([
            "job",
            "run",
            "transform",
            "--input-file-type",
            "mov",
            "--source",
            "media:mov/",
            "--destination",
            "media:mp4/",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--concurrency",
            "4",
            "--non-interactive",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("no bucket-config named").not())
        .stderr(predicate::str::contains(
            "--report-bucket is required when not running interactively",
        ));
}

#[test]
fn job_run_transform_with_an_unknown_alias_in_the_composed_form_errors_by_alias_name() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .args([
            "job",
            "run",
            "transform",
            "--input-file-type",
            "png",
            "--source",
            "no-such-alias:png/",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--concurrency",
            "4",
            "--non-interactive",
        ])
        .assert()
        .failure()
        .code(1)
        // The prefix must not leak into the error -- only the alias half
        // failed validation.
        .stderr(predicate::str::contains(
            "no bucket-config named 'no-such-alias'",
        ));
}

#[test]
fn job_run_transform_rejects_a_dot_dot_path_segment_in_a_prefix() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();
    write_bucket_config(&config_dir, "source");

    pigeon_in(&config_dir)
        .args([
            "job",
            "run",
            "transform",
            "--input-file-type",
            "png",
            "--source",
            "source:../escape/",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--concurrency",
            "4",
            "--non-interactive",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "a '..' path segment is not allowed",
        ));
}

#[test]
fn job_run_transform_without_rclone_or_ffmpeg_on_path_fails_fast_with_a_clear_error() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();
    let empty_path_dir = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .env("PATH", empty_path_dir.path())
        .args([
            "job",
            "run",
            "transform",
            "--input-file-type",
            "png",
            "--source",
            "source",
            "--destination",
            "dest",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--concurrency",
            "4",
            "--non-interactive",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("was not found on PATH"));
}

/// CLI-0149 §6: the external-converter preflight is per input kind, so a
/// `csv` run must not demand `ffmpeg` it never invokes. It still fails on
/// `rclone`, which every kind needs -- so this asserts on *which* binary is
/// named, not merely that something was missing.
#[test]
fn job_run_transform_with_a_csv_input_kind_does_not_require_ffmpeg() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();
    let empty_path_dir = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .env("PATH", empty_path_dir.path())
        .args([
            "job",
            "run",
            "transform",
            "--input-file-type",
            "csv",
            "--source",
            "source",
            "--destination",
            "dest",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--concurrency",
            "4",
            "--non-interactive",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("'rclone' was not found on PATH"))
        .stderr(predicate::str::contains("'ffmpeg' was not found").not())
        .stderr(predicate::str::contains("'soffice' was not found").not());
}

/// The converse: an image kind still requires `ffmpeg`, and reports it
/// before `rclone` -- which is why the pre-existing
/// `..._without_rclone_or_ffmpeg_on_path_...` test above keeps passing.
#[test]
fn job_run_transform_with_an_image_input_kind_still_requires_ffmpeg() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();
    let empty_path_dir = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .env("PATH", empty_path_dir.path())
        .args([
            "job",
            "run",
            "transform",
            "--input-file-type",
            "png",
            "--source",
            "source",
            "--destination",
            "dest",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--concurrency",
            "4",
            "--non-interactive",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("'ffmpeg' was not found on PATH"));
}

/// CLI-0149: a `numbers` run needs `soffice` rather than `ffmpeg`.
#[test]
fn job_run_transform_with_a_numbers_input_kind_requires_soffice_not_ffmpeg() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();
    let empty_path_dir = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .env("PATH", empty_path_dir.path())
        .args([
            "job",
            "run",
            "transform",
            "--input-file-type",
            "numbers",
            "--source",
            "source",
            "--destination",
            "dest",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--concurrency",
            "4",
            "--non-interactive",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("'soffice' was not found on PATH"))
        .stderr(predicate::str::contains("'ffmpeg' was not found").not());
}

/// CLI-0149: an unknown kind still fails fast, and the error now names the
/// spreadsheet kinds among the valid ones.
#[test]
fn job_run_transform_rejects_an_unknown_kind_and_lists_the_spreadsheet_ones() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .args([
            "job",
            "run",
            "transform",
            "--input-file-type",
            "xlsm",
            "--source",
            "source",
            "--destination",
            "dest",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--concurrency",
            "4",
            "--non-interactive",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("unknown --input-file-type 'xlsm'"))
        .stderr(predicate::str::contains(
            "a spreadsheet kind numbers, xlsx, xls or csv",
        ));
}

/// CLI-0153: an invalid `--audio-quality` fails fast and never falls
/// through to an interactive prompt, the same discipline
/// `--input-file-type` and `--video-quality` already have.
#[test]
fn job_run_transform_with_an_invalid_audio_quality_fails_fast() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .args([
            "job",
            "run",
            "transform",
            "--input-file-type",
            "mp3",
            "--audio-quality",
            "lossless",
            "--source",
            "source",
            "--destination",
            "dest",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--concurrency",
            "4",
            "--non-interactive",
        ])
        .assert()
        .failure()
        .code(1)
        // `lossless` is `--video-quality`'s top tier, deliberately absent
        // here: Opus has no lossless mode, so the name would be a lie.
        .stderr(predicate::str::contains(
            "unknown --audio-quality 'lossless'",
        ))
        .stderr(predicate::str::contains(
            "expected low, medium, high, or transparent",
        ));
}

// --- CLI-0145: --keyring-path -------------------------------------------

#[test]
fn top_level_help_shows_the_keyring_path_flag() {
    pigeon()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("--keyring-path"));
}

#[test]
fn keyring_path_is_global_and_accepted_after_a_job_subcommand() {
    // The whole point of `global = true`: the flag parses *after* the
    // subcommand, which is where an operator naturally types it.
    for args in [
        vec!["job", "run", "transform", "--help"],
        vec!["job", "run", "deduplicate", "--help"],
        vec!["job", "run", "rclone", "copy", "--help"],
        vec!["job", "run", "rclone", "delete", "--help"],
    ] {
        pigeon()
            .args(&args)
            .assert()
            .success()
            .stdout(predicate::str::contains("--keyring-path"));
    }
}

#[test]
fn keyring_path_takes_precedence_over_the_config_dir_env_var() {
    // The env-var keyring HAS the bucket; the --keyring-path one does not.
    // Resolving the alias must therefore fail -- which it only can if the
    // flag beat the env var (CLI-0145 §1).
    let config_dir = TempDir::new().unwrap();
    let alt = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();
    write_bucket_config(&config_dir, "src-bucket");
    write_bucket_config_at(&alt.path().join("keyring.toml"), "a-different-bucket");

    pigeon_in(&config_dir)
        .args([
            "job",
            "run",
            "deduplicate",
            "--keyring-path",
            alt.path().join("keyring.toml").to_str().unwrap(),
            "--source-bucket",
            "src-bucket",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--concurrency",
            "4",
            "--non-interactive",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "no bucket-config named 'src-bucket'",
        ));
}

#[test]
fn an_explicit_keyring_path_that_does_not_exist_fails_fast_naming_the_path() {
    // Without the guard, `Store::load` treats a missing file as an EMPTY
    // store, so a typo'd path surfaces much later as "no bucket-config
    // named ...", naming neither the real problem nor the path.
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();
    write_bucket_config(&config_dir, "src-bucket");

    pigeon_in(&config_dir)
        .args([
            "job",
            "run",
            "deduplicate",
            "--keyring-path",
            "/no/such/dir/keyring.toml",
            "--source-bucket",
            "src-bucket",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--non-interactive",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "--keyring-path /no/such/dir/keyring.toml does not exist",
        ))
        .stderr(predicate::str::contains("no bucket-config named").not());
}

#[test]
fn keyring_list_reads_the_overridden_keyring_path() {
    // `--keyring-path` is global, so it reaches `pigeon keyring` too, not
    // only the job types that motivated it.
    let config_dir = TempDir::new().unwrap();
    let alt = TempDir::new().unwrap();
    write_bucket_config(&config_dir, "in-the-env-dir");
    write_bucket_config_at(&alt.path().join("keyring.toml"), "only-at-the-flag-path");

    pigeon_in(&config_dir)
        .args([
            "keyring",
            "list",
            "--keyring-path",
            alt.path().join("keyring.toml").to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("only-at-the-flag-path"))
        .stdout(predicate::str::contains("in-the-env-dir").not());
}

#[test]
fn deduplicate_generates_the_sandboxed_rclone_config_beside_the_keyring() {
    // CLI-0145 §5: `deduplicate` never called `write_sandboxed_config` at
    // all, so it ran against whatever a previous `transform`/`rclone` run
    // happened to leave behind -- or nothing. The generated config must now
    // appear beside the keyring this run was pointed at, and it must appear
    // even though this run goes on to fail, since preflight precedes
    // resolution.
    let config_dir = TempDir::new().unwrap();
    let alt = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();
    write_bucket_config_at(&alt.path().join("keyring.toml"), "src-bucket");

    let generated = alt.path().join("rclone.conf");
    assert!(!generated.exists());

    pigeon_in(&config_dir)
        .args([
            "job",
            "run",
            "deduplicate",
            "--keyring-path",
            alt.path().join("keyring.toml").to_str().unwrap(),
            "--source-bucket",
            "no-such-bucket",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--non-interactive",
        ])
        .assert()
        .failure();

    assert!(
        generated.exists(),
        "deduplicate did not generate rclone.conf"
    );
    let contents = fs::read_to_string(&generated).unwrap();
    assert!(contents.contains("[src-bucket]"), "{contents}");
    assert!(
        !config_dir.path().join("rclone.conf").exists(),
        "the default config dir must be left untouched"
    );
}

// --- `job run email import` (CLI-0146) ---------------------------------
//
// No test reaches a real IMAP server or rclone transfer: every one fails at
// the wizard, before a connection is attempted. rclone's `--retries 5
// --low-level-retries 20` turns even a refused connection into minutes of
// backoff (the test-cost constraint CLI-0145 recorded).

#[test]
fn job_run_help_lists_the_email_group_and_not_the_removed_jobs() {
    // Until CLI-0147 this asserted only that the listing contains
    // "email", which "email-sync" already satisfied -- so it could not
    // actually distinguish the `email` group from its flat siblings. Both
    // halves are load-bearing now.
    pigeon()
        .args(["job", "run", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("email"))
        .stdout(predicate::str::contains("email-sync").not())
        .stdout(predicate::str::contains("email-pull").not());
}

/// Both removed job types must fail as unrecognised subcommands, not
/// merely be absent from the listing (CLI-0147).
#[test]
fn job_run_rejects_the_removed_email_jobs() {
    for removed in ["email-sync", "email-pull"] {
        pigeon()
            .args(["job", "run", removed, "--help"])
            .assert()
            .failure()
            .stderr(predicate::str::contains("unrecognized subcommand"));
    }
}

#[test]
fn job_run_email_help_lists_import() {
    pigeon()
        .args(["job", "run", "email", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("import"));
}

#[test]
fn job_run_email_import_help_shows_its_full_flag_set() {
    pigeon()
        .args(["job", "run", "email", "import", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--identities"))
        .stdout(predicate::str::contains("--destination"))
        .stdout(predicate::str::contains("--local-output"))
        .stdout(predicate::str::contains("--messages-per-minute"))
        .stdout(predicate::str::contains("--batch-size"))
        .stdout(predicate::str::contains("--max-connections"))
        .stdout(predicate::str::contains("--push-concurrency"))
        .stdout(predicate::str::contains("--report-bucket"))
        .stdout(predicate::str::contains("--non-interactive"));
}

/// Pins CLI-0146 §1's scoping decisions, the way CLI-0081's own
/// `--encryption-key` test pinned the since-removed `email-pull`'s. Each
/// absence is deliberate
/// and each would be easy to add back by accident:
/// `--encryption-key` is permanently out of scope; `--upload-only` is
/// meaningless when delivery is inline; `--concurrency` would imply it
/// governs throughput when `--messages-per-minute` does; and `--tpslimit`
/// and friends do not apply to `rclone copyto`.
#[test]
fn job_run_email_import_help_omits_every_deliberately_absent_flag() {
    pigeon()
        .args(["job", "run", "email", "import", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--encryption-key").not())
        .stdout(predicate::str::contains("--upload-only").not())
        .stdout(predicate::str::contains("--concurrency").not())
        .stdout(predicate::str::contains("--tpslimit").not())
        .stdout(predicate::str::contains("--transfers").not())
        .stdout(predicate::str::contains("--checkers").not());
}

/// `--push-concurrency` ends in "concurrency", so the negative assertion
/// above has to be checking for the standalone `--concurrency` flag rather
/// than a substring that `--push-concurrency` would satisfy.
#[test]
fn job_run_email_import_help_still_shows_push_concurrency() {
    pigeon()
        .args(["job", "run", "email", "import", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--push-concurrency"));
}

#[test]
fn job_run_email_import_without_identities_fails_fast_non_interactively() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();
    write_bucket_config(&config_dir, "dest");

    pigeon_in(&config_dir)
        .args([
            "job",
            "run",
            "email",
            "import",
            "--destination",
            "dest",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--report-bucket",
            "dest",
            "--non-interactive",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "--identities is required when not running interactively",
        ));
}

/// `--destination` is mandatory, unlike the sibling email jobs'
/// `--remote-output`, which silently resolves to "don't upload"
/// (CLI-0146 §1): a job whose only purpose is delivery must not be able to
/// run without somewhere to deliver to.
#[test]
fn job_run_email_import_without_a_destination_fails_fast_non_interactively() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();
    write_identity(&config_dir, "first-last", "first.last@example.com");
    write_bucket_config(&config_dir, "dest");

    pigeon_in(&config_dir)
        .args([
            "job",
            "run",
            "email",
            "import",
            "--identities",
            "first-last",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--report-bucket",
            "dest",
            "--non-interactive",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "--destination is required when not running interactively",
        ));
}

#[test]
fn job_run_email_import_requires_a_report_bucket_non_interactively() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();
    write_identity(&config_dir, "first-last", "first.last@example.com");
    write_bucket_config(&config_dir, "dest");

    pigeon_in(&config_dir)
        .args([
            "job",
            "run",
            "email",
            "import",
            "--identities",
            "first-last",
            "--destination",
            "dest",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--non-interactive",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "--report-bucket is required when not running interactively",
        ));
}

#[test]
fn job_run_email_import_with_an_unknown_identity_fails() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();
    write_bucket_config(&config_dir, "dest");

    pigeon_in(&config_dir)
        .args([
            "job",
            "run",
            "email",
            "import",
            "--identities",
            "nope",
            "--destination",
            "dest",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--report-bucket",
            "dest",
            "--non-interactive",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("no identity with alias 'nope'"));
}

#[test]
fn job_run_email_import_with_an_unknown_destination_alias_fails() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();
    write_identity(&config_dir, "first-last", "first.last@example.com");
    write_bucket_config(&config_dir, "dest");

    pigeon_in(&config_dir)
        .args([
            "job",
            "run",
            "email",
            "import",
            "--identities",
            "first-last",
            "--destination",
            "nope:eml/",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--report-bucket",
            "dest",
            "--non-interactive",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("no bucket-config named 'nope'"));
}

/// CLI-0138's prefix validation reaches this job's `--destination` too: a
/// `..` segment would be taken literally by rclone, writing outside the
/// configured prefix.
#[test]
fn job_run_email_import_rejects_a_parent_directory_segment_in_the_destination_prefix() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();
    write_identity(&config_dir, "first-last", "first.last@example.com");
    write_bucket_config(&config_dir, "dest");

    pigeon_in(&config_dir)
        .args([
            "job",
            "run",
            "email",
            "import",
            "--identities",
            "first-last",
            "--destination",
            "dest:../escape/",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--report-bucket",
            "dest",
            "--non-interactive",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "a '..' path segment is not allowed",
        ));
}

/// The first email job to depend on `rclone` (CLI-0146 §9): both siblings
/// are `minio`-only, so this preflight is new behavior for an email job and
/// must fail before any IMAP credential is touched.
#[test]
fn job_run_email_import_without_rclone_on_path_fails_fast_with_a_clear_error() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();
    let empty_path_dir = TempDir::new().unwrap();
    write_identity(&config_dir, "first-last", "first.last@example.com");
    write_bucket_config(&config_dir, "dest");

    pigeon_in(&config_dir)
        .env("PATH", empty_path_dir.path())
        .args([
            "job",
            "run",
            "email",
            "import",
            "--identities",
            "first-last",
            "--destination",
            "dest",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--report-bucket",
            "dest",
            "--non-interactive",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("was not found on PATH"));
}

/// CLI-0147 §2: a leftover `kind = "encryption-key"` entry must fail with a
/// message naming both the file and the block to delete. Asserted at the
/// CLI level and not only in `Store::load`'s own unit tests because the
/// failure path locks the operator out of *every* keyring command --
/// including the `list`/`delete` they would otherwise use to clean it up --
/// so what reaches stderr is the only remediation they get.
#[test]
fn keyring_list_explains_how_to_remove_a_leftover_encryption_key_entry() {
    let config_dir = TempDir::new().unwrap();
    append_keyring_entry(
        &config_dir,
        "[[entries]]\nkind = \"encryption-key\"\nalias = \"primary\"\n\
         created_at = \"2026-09-24T00:00:00Z\"\nkey_hex = \"0\"\n",
    );

    pigeon_in(&config_dir)
        .args(["keyring", "list"])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("encryption-key"))
        .stderr(predicate::str::contains("CLI-0147"))
        .stderr(predicate::str::contains("[[entries]]"))
        .stderr(predicate::str::contains("keyring.toml"));
}

/// The companion to the above: a stale `encryption_key_alias` on a *bucket*
/// entry is a different compatibility story. Nothing in the crate uses
/// `deny_unknown_fields`, so serde ignores it and no migration is needed.
#[test]
fn keyring_list_ignores_a_stale_encryption_key_alias_on_a_bucket_entry() {
    let config_dir = TempDir::new().unwrap();
    append_keyring_entry(
        &config_dir,
        "[[entries]]\nkind = \"bucket\"\nalias = \"backup\"\n\
         endpoint = \"https://nyc3.digitaloceanspaces.com\"\nbucket = \"my-bucket\"\n\
         access_key_id = \"AKID\"\nsecret_key = \"fake-secret\"\n\
         encryption_key_alias = \"primary\"\n",
    );

    pigeon_in(&config_dir)
        .args(["keyring", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("backup"));
}

/// CLI-0147 removed the `encryption-key` kind, so `keyring add` offers
/// exactly two.
#[test]
fn keyring_add_help_lists_only_email_and_bucket() {
    pigeon()
        .args(["keyring", "add", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("email"))
        .stdout(predicate::str::contains("bucket"))
        .stdout(predicate::str::contains("encryption-key").not());
}

// --- CLI-0154: documents ---

/// CLI-0154: `transform --help` names every new input kind. Backticked so
/// an incidental substring elsewhere in the help text cannot satisfy the
/// assertion.
#[test]
fn job_run_transform_help_lists_every_document_input_kind() {
    let config_dir = TempDir::new().unwrap();
    let assert = pigeon_in(&config_dir)
        .args(["job", "run", "transform", "--help"])
        .assert()
        .success();
    let help = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    for kind in [
        "`doc`",
        "`docx`",
        "`pages`",
        "`rtf`",
        "`pdf`",
        "`epub`",
        "`pptx`",
        "`key`",
        "`txt`",
        "`md`",
        "`rst`",
        "`textclipping`",
        "`tsv`",
    ] {
        assert!(help.contains(kind), "{kind} missing from transform --help");
    }
}

/// The two new binaries are named in the help, so an operator learns what a
/// document run needs before it fails on the first file.
#[test]
fn job_run_transform_help_names_the_new_converters() {
    let config_dir = TempDir::new().unwrap();
    let assert = pigeon_in(&config_dir)
        .args(["job", "run", "transform", "--help"])
        .assert()
        .success();
    let help = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    assert!(help.contains("ebook-convert"), "{help}");
    assert!(help.contains("pandoc"), "{help}");
}

/// CLI-0154 §5: the preflight is now an exhaustive match, so each kind
/// demands exactly its own converter. `tsv` is read and written in-process,
/// so it must demand neither `ffmpeg` nor `soffice`.
#[test]
fn job_run_transform_with_a_tsv_input_kind_requires_no_converter() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();
    let empty_path_dir = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .env("PATH", empty_path_dir.path())
        .args([
            "job",
            "run",
            "transform",
            "--input-file-type",
            "tsv",
            "--source",
            "source",
            "--destination",
            "dest",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--concurrency",
            "4",
            "--non-interactive",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("'rclone' was not found on PATH"))
        .stderr(predicate::str::contains("'ffmpeg' was not found").not())
        .stderr(predicate::str::contains("'soffice' was not found").not())
        .stderr(predicate::str::contains("'pandoc' was not found").not());
}

/// A Word document goes through LibreOffice, never `ffmpeg`. Before
/// CLI-0154 §5 replaced the preflight's wildcard arm, this run would have
/// demanded `ffmpeg` and never checked `soffice` at all.
#[test]
fn job_run_transform_with_a_docx_input_kind_requires_soffice_not_ffmpeg() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();
    let empty_path_dir = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .env("PATH", empty_path_dir.path())
        .args([
            "job",
            "run",
            "transform",
            "--input-file-type",
            "docx",
            "--source",
            "source",
            "--destination",
            "dest",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--concurrency",
            "4",
            "--non-interactive",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("'soffice' was not found on PATH"))
        .stderr(predicate::str::contains("'ffmpeg' was not found").not());
}

/// `epub` is the one kind LibreOffice cannot read -- its EPUB filter is
/// export-only -- so this run must name Calibre, not `soffice`.
#[test]
fn job_run_transform_with_an_epub_input_kind_requires_calibre() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();
    let empty_path_dir = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .env("PATH", empty_path_dir.path())
        .args([
            "job",
            "run",
            "transform",
            "--input-file-type",
            "epub",
            "--source",
            "source",
            "--destination",
            "dest",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--concurrency",
            "4",
            "--non-interactive",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "'ebook-convert' was not found on PATH",
        ))
        .stderr(predicate::str::contains("'ffmpeg' was not found").not())
        .stderr(predicate::str::contains("'soffice' was not found").not());
}

/// Markdown and reStructuredText are rendered by `pandoc`.
#[test]
fn job_run_transform_with_a_markdown_input_kind_requires_pandoc() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();
    let empty_path_dir = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .env("PATH", empty_path_dir.path())
        .args([
            "job",
            "run",
            "transform",
            "--input-file-type",
            "md",
            "--source",
            "source",
            "--destination",
            "dest",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--concurrency",
            "4",
            "--non-interactive",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("'pandoc' was not found on PATH"))
        .stderr(predicate::str::contains("'ffmpeg' was not found").not())
        .stderr(predicate::str::contains("'soffice' was not found").not());
}

/// CLI-0154 §1: `markdown` and `mkd` are accepted spellings of the same
/// kind, so they must reach the same preflight as `md`.
#[test]
fn job_run_transform_accepts_every_markdown_spelling() {
    for spelling in ["md", "markdown", "mkd"] {
        let config_dir = TempDir::new().unwrap();
        let local_output = TempDir::new().unwrap();
        let empty_path_dir = TempDir::new().unwrap();

        pigeon_in(&config_dir)
            .env("PATH", empty_path_dir.path())
            .args([
                "job",
                "run",
                "transform",
                "--input-file-type",
                spelling,
                "--source",
                "source",
                "--destination",
                "dest",
                "--local-output",
                local_output.path().to_str().unwrap(),
                "--concurrency",
                "4",
                "--non-interactive",
            ])
            .assert()
            .failure()
            .code(1)
            .stderr(predicate::str::contains("'pandoc' was not found on PATH"));
    }
}
