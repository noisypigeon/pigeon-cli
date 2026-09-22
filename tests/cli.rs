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
fn email_help_lists_all_subcommands() {
    pigeon()
        .args(["email", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("authenticate"))
        .stdout(predicate::str::contains("list-identities"))
        .stdout(predicate::str::contains("sync"));
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
        .args(["email", "list-identities"])
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
        .args(["email", "list-identities"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No identities configured."));
}

/// Writes a fake `identities.toml` directly (no `authenticate`/keychain
/// needed -- `sync --debug transform` never reads the secret).
fn write_identity(config_dir: &TempDir, alias: &str, email: &str) {
    let toml = format!(
        "[[identities]]\nalias = \"{alias}\"\nemail = \"{email}\"\nprovider = \"gmail\"\nhost = \"imap.gmail.com\"\nport = 993\n"
    );
    fs::write(config_dir.path().join("identities.toml"), toml).unwrap();
}

#[test]
fn sync_help_shows_staging_output_and_debug_flags() {
    pigeon()
        .args(["email", "sync", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("[ALIAS]"))
        .stdout(predicate::str::contains("--staging-dir"))
        .stdout(predicate::str::contains("--output-dir"))
        .stdout(predicate::str::contains("--debug"));
}

#[test]
fn sync_without_alias_on_empty_store_says_to_authenticate() {
    let config_dir = TempDir::new().unwrap();
    let staging_dir = TempDir::new().unwrap();
    let output_dir = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .args([
            "email",
            "sync",
            "--staging-dir",
            staging_dir.path().to_str().unwrap(),
            "--output-dir",
            output_dir.path().to_str().unwrap(),
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("authenticate"));
}

#[test]
fn sync_with_unknown_alias_fails_fast() {
    let config_dir = TempDir::new().unwrap();
    let staging_dir = TempDir::new().unwrap();
    let output_dir = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .args([
            "email",
            "sync",
            "no-such-alias",
            "--staging-dir",
            staging_dir.path().to_str().unwrap(),
            "--output-dir",
            output_dir.path().to_str().unwrap(),
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "no identity with alias 'no-such-alias'",
        ));
}

#[test]
fn sync_debug_sink_without_alias_on_empty_store_says_to_authenticate() {
    let config_dir = TempDir::new().unwrap();
    let staging_dir = TempDir::new().unwrap();
    let output_dir = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .args([
            "email",
            "sync",
            "--staging-dir",
            staging_dir.path().to_str().unwrap(),
            "--output-dir",
            output_dir.path().to_str().unwrap(),
            "--debug",
            "sink",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("authenticate"));
}

#[test]
fn sync_debug_sink_with_unknown_alias_fails_fast() {
    let config_dir = TempDir::new().unwrap();
    let staging_dir = TempDir::new().unwrap();
    let output_dir = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .args([
            "email",
            "sync",
            "no-such-alias",
            "--staging-dir",
            staging_dir.path().to_str().unwrap(),
            "--output-dir",
            output_dir.path().to_str().unwrap(),
            "--debug",
            "sink",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "no identity with alias 'no-such-alias'",
        ));
}

#[test]
fn sync_debug_transform_without_alias_on_empty_store_says_to_authenticate() {
    let config_dir = TempDir::new().unwrap();
    let staging_dir = TempDir::new().unwrap();
    let output_dir = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .args([
            "email",
            "sync",
            "--staging-dir",
            staging_dir.path().to_str().unwrap(),
            "--output-dir",
            output_dir.path().to_str().unwrap(),
            "--debug",
            "transform",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("authenticate"));
}

#[test]
fn sync_debug_transform_converts_a_plain_text_message() {
    let config_dir = TempDir::new().unwrap();
    let staging_dir = TempDir::new().unwrap();
    let output_dir = TempDir::new().unwrap();

    write_identity(&config_dir, "first-last", "first.last@example.com");

    let inbox_dir = staging_dir.path().join("inbox");
    fs::create_dir_all(&inbox_dir).unwrap();
    fs::write(
        inbox_dir.join("1.eml"),
        "From: Jane Doe <jane.doe@example.com>\r\n\
         To: first.last@example.com\r\n\
         Subject: Hello, World!\r\n\
         Date: Fri, 26 Jan 2024 09:15:00 +0000\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         \r\n\
         Hello there!\r\n\
         This is a test message.\r\n",
    )
    .unwrap();

    pigeon_in(&config_dir)
        .args([
            "email",
            "sync",
            "first-last",
            "--staging-dir",
            staging_dir.path().to_str().unwrap(),
            "--output-dir",
            output_dir.path().to_str().unwrap(),
            "--debug",
            "transform",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("1 message"));

    let md_path = output_dir
        .path()
        .join("first-last-example-com")
        .join("2024-01-26-hello-world.md");
    let contents = fs::read_to_string(&md_path).unwrap();
    assert!(contents.contains("from: \"Jane Doe <jane.doe@example.com>\""));
    assert!(contents.contains("subject: \"Hello, World!\""));
    assert!(contents.contains("mailbox/inbox"));
    assert!(contents.contains("identity/first-last"));
    assert!(contents.contains("sender/example-com"));
    assert!(contents.contains("uid: 1"));
    assert!(contents.contains("Hello there!"));

    // --debug transform never deletes the source .eml.
    assert!(inbox_dir.join("1.eml").exists());
}

#[test]
fn sync_debug_transform_converts_an_html_only_message() {
    let config_dir = TempDir::new().unwrap();
    let staging_dir = TempDir::new().unwrap();
    let output_dir = TempDir::new().unwrap();

    write_identity(&config_dir, "first-last", "first.last@example.com");

    let inbox_dir = staging_dir.path().join("inbox");
    fs::create_dir_all(&inbox_dir).unwrap();
    fs::write(
        inbox_dir.join("2.eml"),
        "From: Newsletter <news@example.org>\r\n\
         To: first.last@example.com\r\n\
         Subject: Weekly Update\r\n\
         Date: Mon, 05 Feb 2024 12:00:00 +0000\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         \r\n\
         <html><body><h1>Weekly Update</h1><p>Hello <b>world</b>!</p></body></html>\r\n",
    )
    .unwrap();

    pigeon_in(&config_dir)
        .args([
            "email",
            "sync",
            "first-last",
            "--staging-dir",
            staging_dir.path().to_str().unwrap(),
            "--output-dir",
            output_dir.path().to_str().unwrap(),
            "--debug",
            "transform",
        ])
        .assert()
        .success();

    let md_path = output_dir
        .path()
        .join("first-last-example-com")
        .join("2024-02-05-weekly-update.md");
    let contents = fs::read_to_string(&md_path).unwrap();
    assert!(contents.contains("# Weekly Update"));
    assert!(contents.contains("world"));
    assert!(contents.contains("uid: 2"));
}
