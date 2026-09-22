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
fn email_help_lists_all_four_subcommands() {
    pigeon()
        .args(["email", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("authenticate"))
        .stdout(predicate::str::contains("list-identities"))
        .stdout(predicate::str::contains("sink"))
        .stdout(predicate::str::contains("transform"));
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

#[test]
fn sink_help_shows_optional_alias_and_directory() {
    pigeon()
        .args(["email", "sink", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("[ALIAS]"))
        .stdout(predicate::str::contains("--directory"));
}

#[test]
fn sink_without_alias_on_empty_store_says_to_authenticate() {
    let config_dir = TempDir::new().unwrap();
    let output_dir = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .args([
            "email",
            "sink",
            "--directory",
            output_dir.path().to_str().unwrap(),
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("authenticate"));
}

#[test]
fn sink_with_unknown_alias_fails_fast() {
    let config_dir = TempDir::new().unwrap();
    let output_dir = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .args([
            "email",
            "sink",
            "no-such-alias",
            "--directory",
            output_dir.path().to_str().unwrap(),
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "no identity with alias 'no-such-alias'",
        ));
}

/// Writes a fake `identities.toml` directly (no `authenticate`/keychain
/// needed -- `transform` never reads the secret), registering one identity.
fn write_identity(config_dir: &TempDir, alias: &str, email: &str) {
    let toml = format!(
        "[[identities]]\nalias = \"{alias}\"\nemail = \"{email}\"\nprovider = \"gmail\"\nhost = \"imap.gmail.com\"\nport = 993\n"
    );
    fs::write(config_dir.path().join("identities.toml"), toml).unwrap();
}

#[test]
fn transform_help_shows_new_shape() {
    pigeon()
        .args(["email", "transform", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("[ALIAS]"))
        .stdout(predicate::str::contains("--input"))
        .stdout(predicate::str::contains("--output"))
        .stdout(predicate::str::contains("--normalize").not())
        .stdout(predicate::str::contains("--mbox-to-markdown").not());
}

#[test]
fn transform_without_alias_on_empty_store_says_to_authenticate() {
    let config_dir = TempDir::new().unwrap();
    let input_dir = TempDir::new().unwrap();
    let output_dir = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .args([
            "email",
            "transform",
            "--input",
            input_dir.path().to_str().unwrap(),
            "--output",
            output_dir.path().to_str().unwrap(),
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("authenticate"));
}

#[test]
fn transform_converts_a_plain_text_message() {
    let config_dir = TempDir::new().unwrap();
    let input_dir = TempDir::new().unwrap();
    let output_dir = TempDir::new().unwrap();

    write_identity(&config_dir, "first-last", "first.last@example.com");

    let inbox_dir = input_dir.path().join("inbox");
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
            "transform",
            "first-last",
            "--input",
            input_dir.path().to_str().unwrap(),
            "--output",
            output_dir.path().to_str().unwrap(),
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
    assert!(contents.contains("Hello there!"));
}

#[test]
fn transform_converts_an_html_only_message() {
    let config_dir = TempDir::new().unwrap();
    let input_dir = TempDir::new().unwrap();
    let output_dir = TempDir::new().unwrap();

    write_identity(&config_dir, "first-last", "first.last@example.com");

    let inbox_dir = input_dir.path().join("inbox");
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
            "transform",
            "first-last",
            "--input",
            input_dir.path().to_str().unwrap(),
            "--output",
            output_dir.path().to_str().unwrap(),
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
}
