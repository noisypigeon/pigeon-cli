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
/// build up a mixed store. Includes a dummy secret field (ADR-0135: `secret`/
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
/// `PIGEON_CONFIG_DIR`, for the `--keyring-path` tests (ADR-0145). Mirrors
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

/// Confirming removes the entry from `keyring.toml` directly (ADR-0135: the
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
        .stdout(predicate::str::contains("--non-interactive"));
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
            "--non-interactive",
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
            "--non-interactive",
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
            "--non-interactive",
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
        .stdout(predicate::str::contains("--non-interactive"))
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
            "--non-interactive",
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
            "--non-interactive",
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
            "--non-interactive",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "no identity with alias 'no-such-alias'",
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
        .stdout(predicate::str::contains("--destination-bucket"))
        .stdout(predicate::str::contains("--concurrency"))
        .stdout(predicate::str::contains("--transfers"))
        .stdout(predicate::str::contains("--checkers"))
        .stdout(predicate::str::contains("--tpslimit"))
        .stdout(predicate::str::contains("--upload-only"))
        .stdout(predicate::str::contains("--report-bucket"))
        .stdout(predicate::str::contains("--non-interactive"))
        // ADR-0136: upload is now one bulk `rclone copy`, tuned by
        // --transfers/--checkers/--tpslimit, not --upload-concurrency.
        .stdout(predicate::str::contains("--upload-concurrency").not())
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
    // (ADR-0089) must fail before either would ever be resolved, since this
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
    // Two --source-bucket occurrences (ADR-0109) -- neither bucket exists,
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
        // ADR-0136: this job's rclone performance/retry flags beyond
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
        // ADR-0110: delete has no destination and no file-transfer tuning
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
        // ADR-0137: renamed from --source-bucket/--destination-bucket.
        .stdout(predicate::str::contains("--source-bucket").not())
        .stdout(predicate::str::contains("--destination-bucket").not())
        // ADR-0138: the composed form is documented on both flags.
        .stdout(predicate::str::contains("media:mp4/"))
        // ADR-0142: the audio-only input kind is listed.
        .stdout(predicate::str::contains("mp4-audio"))
        // ADR-0136: bucket-config aliases, not raw rclone path strings, and
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

    // ADR-0138 reversed ADR-0137's alias inference: with no --source there
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

/// The exact regression ADR-0138 fixes: the composed `alias:prefix/` form
/// that ADR-0136 made unexpressible. Resolution must get past both bucket
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
/// by prefix -- impossible under ADR-0137's alias inference, which for a
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

// --- ADR-0145: --keyring-path -------------------------------------------

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
    // flag beat the env var (ADR-0145 §1).
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
    // ADR-0145 §5: `deduplicate` never called `write_sandboxed_config` at
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
