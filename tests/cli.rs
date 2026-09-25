use std::fs;

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

fn pigeon() -> Command {
    Command::cargo_bin("pigeon").unwrap()
}

/// A `pigeon` invocation isolated to a throwaway `PIGEON_CONFIG_DIR`, so
/// tests never read or write the developer's real identity metadata file.
fn pigeon_in(config_dir: &TempDir) -> Command {
    let mut cmd = pigeon();
    cmd.env("PIGEON_CONFIG_DIR", config_dir.path());
    cmd
}

#[test]
fn top_level_help_lists_email_command() {
    pigeon()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("email"));
}

#[test]
fn top_level_help_lists_dataops_command() {
    pigeon()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("dataops"));
}

#[test]
fn email_help_lists_all_subcommands() {
    pigeon()
        .args(["email", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("authenticate"))
        .stdout(predicate::str::contains("list"));
}

#[test]
fn authenticate_help_lists_provider_and_custom_host_flags() {
    pigeon()
        .args(["email", "authenticate", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--provider"))
        .stdout(predicate::str::contains("--host"))
        .stdout(predicate::str::contains("--port"));
}

#[test]
fn list_identities_on_empty_store_says_so() {
    let config_dir = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .args(["email", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No identities configured."));
}

#[test]
fn authenticate_custom_provider_without_host_fails_fast() {
    let config_dir = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .args([
            "email",
            "authenticate",
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
fn authenticate_failure_does_not_persist_an_identity() {
    let config_dir = TempDir::new().unwrap();

    // Nothing listens on 127.0.0.1:1 (a privileged, unassigned port), so
    // this deterministically fails at the connection step without ever
    // reaching a real IMAP server or the OS keychain.
    pigeon_in(&config_dir)
        .args([
            "email",
            "authenticate",
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
        .args(["email", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No identities configured."));
}

/// Writes a fake `identities.toml` directly (no `authenticate`/keychain
/// needed for the error paths these tests exercise, which fail before ever
/// reaching a secret lookup or an IMAP connection).
fn write_identity(config_dir: &TempDir, alias: &str, email: &str) {
    let toml = format!(
        "[[identities]]\nalias = \"{alias}\"\nemail = \"{email}\"\nprovider = \"gmail\"\nhost = \"imap.gmail.com\"\nport = 993\n"
    );
    fs::write(config_dir.path().join("identities.toml"), toml).unwrap();
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

/// Writes a fake `bucket-configs.toml` directly (no `bucket-config new`/
/// keychain needed -- none of these tests reach a real S3 endpoint or the
/// keychain).
fn write_bucket_config(config_dir: &TempDir, alias: &str) {
    let toml = format!(
        "[[bucket_configs]]\nalias = \"{alias}\"\nendpoint = \"https://nyc3.digitaloceanspaces.com\"\nbucket = \"my-bucket\"\naccess_key_id = \"AKID\"\n"
    );
    fs::write(config_dir.path().join("bucket-configs.toml"), toml).unwrap();
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
        .stdout(predicate::str::contains("--yes"));
}

#[test]
fn dataops_help_lists_bucket_config_subcommand() {
    pigeon()
        .args(["dataops", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("bucket-config"));
}

#[test]
fn dataops_bucket_config_new_help_shows_optional_alias() {
    pigeon()
        .args(["dataops", "bucket-config", "new", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("[ALIAS]"));
}

#[test]
fn dataops_bucket_config_new_with_existing_alias_fails_fast() {
    let config_dir = TempDir::new().unwrap();
    write_bucket_config(&config_dir, "email");

    pigeon_in(&config_dir)
        .args(["dataops", "bucket-config", "new", "email"])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "a bucket-config named 'email' already exists",
        ));
}

#[test]
fn dataops_bucket_config_edit_with_unknown_alias_fails_fast() {
    let config_dir = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .args(["dataops", "bucket-config", "edit", "no-such-bucket-config"])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "no bucket-config named 'no-such-bucket-config'",
        ));
}

#[test]
fn dataops_bucket_config_edit_without_alias_on_empty_store_says_to_create_one() {
    let config_dir = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .args(["dataops", "bucket-config", "edit"])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("bucket-config new"));
}

#[test]
fn dataops_bucket_config_remove_with_unknown_alias_fails_fast() {
    let config_dir = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .args([
            "dataops",
            "bucket-config",
            "remove",
            "no-such-bucket-config",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "no bucket-config named 'no-such-bucket-config'",
        ));
}

#[test]
fn dataops_bucket_config_remove_without_alias_on_empty_store_says_to_create_one() {
    let config_dir = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .args(["dataops", "bucket-config", "remove"])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("bucket-config new"));
}

#[test]
fn dataops_bucket_config_remove_declined_keeps_it() {
    let config_dir = TempDir::new().unwrap();
    write_bucket_config(&config_dir, "email");

    pigeon_in(&config_dir)
        .args(["dataops", "bucket-config", "remove", "email"])
        .write_stdin("n\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("Cancelled."));

    let contents = fs::read_to_string(config_dir.path().join("bucket-configs.toml")).unwrap();
    assert!(contents.contains("email"));
}

/// Confirming removes the bucket-config even though no keychain secret was
/// ever created for it (write_bucket_config() bypasses `bucket-config new`)
/// -- exercises `credentials::delete_secret`'s `NoEntry`-tolerant handling
/// end to end.
#[test]
fn dataops_bucket_config_remove_confirmed_deletes_it() {
    let config_dir = TempDir::new().unwrap();
    write_bucket_config(&config_dir, "email");

    pigeon_in(&config_dir)
        .args(["dataops", "bucket-config", "remove", "email"])
        .write_stdin("y\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("Removed bucket-config 'email'."));

    let contents = fs::read_to_string(config_dir.path().join("bucket-configs.toml")).unwrap();
    assert!(!contents.contains("email"));
}
