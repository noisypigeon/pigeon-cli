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
        .stdout(predicate::str::contains("list"))
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
/// needed -- `sync --debug transform` never reads the secret).
fn write_identity(config_dir: &TempDir, alias: &str, email: &str) {
    let toml = format!(
        "[[identities]]\nalias = \"{alias}\"\nemail = \"{email}\"\nprovider = \"gmail\"\nhost = \"imap.gmail.com\"\nport = 993\n"
    );
    fs::write(config_dir.path().join("identities.toml"), toml).unwrap();
}

#[test]
fn sync_help_shows_local_output_and_debug_flags() {
    pigeon()
        .args(["email", "sync", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("[ALIAS]"))
        .stdout(predicate::str::contains("--local-output"))
        .stdout(predicate::str::contains("--remote-output"))
        .stdout(predicate::str::contains("--debug"));
}

#[test]
fn sync_without_alias_on_empty_store_says_to_authenticate() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .args([
            "email",
            "sync",
            "--local-output",
            local_output.path().to_str().unwrap(),
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("authenticate"));
}

#[test]
fn sync_with_unknown_alias_fails_fast() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .args([
            "email",
            "sync",
            "no-such-alias",
            "--local-output",
            local_output.path().to_str().unwrap(),
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
    let local_output = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .args([
            "email",
            "sync",
            "--local-output",
            local_output.path().to_str().unwrap(),
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
    let local_output = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .args([
            "email",
            "sync",
            "no-such-alias",
            "--local-output",
            local_output.path().to_str().unwrap(),
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
fn sync_debug_sink_with_remote_output_is_rejected() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();

    write_identity(&config_dir, "first-last", "first.last@example.com");

    pigeon_in(&config_dir)
        .args([
            "email",
            "sync",
            "first-last",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--remote-output",
            "backup",
            "--debug",
            "sink",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "--remote-output cannot be combined with --debug",
        ));
}

#[test]
fn sync_debug_transform_with_remote_output_is_rejected() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();

    write_identity(&config_dir, "first-last", "first.last@example.com");

    pigeon_in(&config_dir)
        .args([
            "email",
            "sync",
            "first-last",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--remote-output",
            "backup",
            "--debug",
            "transform",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "--remote-output cannot be combined with --debug",
        ));
}

#[test]
fn sync_debug_sink_with_concurrency_is_rejected() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();

    write_identity(&config_dir, "first-last", "first.last@example.com");

    pigeon_in(&config_dir)
        .args([
            "email",
            "sync",
            "first-last",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--concurrency",
            "8",
            "--debug",
            "sink",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "--concurrency cannot be combined with --debug",
        ));
}

#[test]
fn sync_default_flow_with_unknown_remote_output_fails_fast() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();

    write_identity(&config_dir, "first-last", "first.last@example.com");

    pigeon_in(&config_dir)
        .args([
            "email",
            "sync",
            "first-last",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--remote-output",
            "no-such-remote",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "no bucket-config named 'no-such-remote'",
        ));
}

#[test]
fn sync_debug_transform_without_alias_on_empty_store_says_to_authenticate() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();

    pigeon_in(&config_dir)
        .args([
            "email",
            "sync",
            "--local-output",
            local_output.path().to_str().unwrap(),
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
    let local_output = TempDir::new().unwrap();

    write_identity(&config_dir, "first-last", "first.last@example.com");

    let inbox_dir = local_output.path().join("staging").join("inbox");
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
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--debug",
            "transform",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("1 message"));

    let md_path = local_output
        .path()
        .join("result")
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
    let local_output = TempDir::new().unwrap();

    write_identity(&config_dir, "first-last", "first.last@example.com");

    let inbox_dir = local_output.path().join("staging").join("inbox");
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
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--debug",
            "transform",
        ])
        .assert()
        .success();

    let md_path = local_output
        .path()
        .join("result")
        .join("first-last-example-com")
        .join("2024-02-05-weekly-update.md");
    let contents = fs::read_to_string(&md_path).unwrap();
    assert!(contents.contains("# Weekly Update"));
    assert!(contents.contains("world"));
    assert!(contents.contains("uid: 2"));
}

#[test]
fn sync_debug_transform_dedupes_identical_attachment_across_two_messages() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();

    write_identity(&config_dir, "first-last", "first.last@example.com");

    let inbox_dir = local_output.path().join("staging").join("inbox");
    fs::create_dir_all(&inbox_dir).unwrap();

    let eml = |subject: &str| {
        format!(
            "From: Jane Doe <jane.doe@example.com>\r\n\
             To: first.last@example.com\r\n\
             Subject: {subject}\r\n\
             Date: Fri, 26 Jan 2024 09:15:00 +0000\r\n\
             MIME-Version: 1.0\r\n\
             Content-Type: multipart/mixed; boundary=\"BOUNDARY\"\r\n\
             \r\n\
             --BOUNDARY\r\n\
             Content-Type: text/plain; charset=utf-8\r\n\
             \r\n\
             Hello there!\r\n\
             --BOUNDARY\r\n\
             Content-Type: application/pdf\r\n\
             Content-Disposition: attachment; filename=\"a.pdf\"\r\n\
             Content-Transfer-Encoding: base64\r\n\
             \r\n\
             JVBERi0xLjQK\r\n\
             --BOUNDARY--\r\n"
        )
    };
    fs::write(inbox_dir.join("1.eml"), eml("First")).unwrap();
    fs::write(inbox_dir.join("2.eml"), eml("Second")).unwrap();

    pigeon_in(&config_dir)
        .args([
            "email",
            "sync",
            "first-last",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--debug",
            "transform",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("1 attachment(s) deduped"));

    let attachments_dir = local_output
        .path()
        .join("result")
        .join("first-last-example-com")
        .join("attachments");
    assert_eq!(fs::read_dir(&attachments_dir).unwrap().count(), 1);
}

#[test]
fn sync_debug_transform_merges_duplicate_whole_message() {
    let config_dir = TempDir::new().unwrap();
    let local_output = TempDir::new().unwrap();

    write_identity(&config_dir, "first-last", "first.last@example.com");

    let inbox_dir = local_output.path().join("staging").join("inbox");
    let archive_dir = local_output.path().join("staging").join("archive");
    fs::create_dir_all(&inbox_dir).unwrap();
    fs::create_dir_all(&archive_dir).unwrap();

    let raw = "From: Jane Doe <jane.doe@example.com>\r\n\
        To: first.last@example.com\r\n\
        Subject: Hello, World!\r\n\
        Date: Fri, 26 Jan 2024 09:15:00 +0000\r\n\
        Content-Type: text/plain; charset=utf-8\r\n\
        \r\n\
        Hello there!\r\n";
    fs::write(inbox_dir.join("1.eml"), raw).unwrap();
    fs::write(archive_dir.join("2.eml"), raw).unwrap();

    pigeon_in(&config_dir)
        .args([
            "email",
            "sync",
            "first-last",
            "--local-output",
            local_output.path().to_str().unwrap(),
            "--debug",
            "transform",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("1 message(s) merged"));

    let identity_dir = local_output
        .path()
        .join("result")
        .join("first-last-example-com");
    let md_files: Vec<_> = fs::read_dir(&identity_dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("md"))
        .collect();
    assert_eq!(md_files.len(), 1);

    // `transform::run` visits `.eml` files in sorted-path order, so
    // "archive/2.eml" (a < i) is processed first and becomes canonical;
    // "inbox/1.eml" is the duplicate merged into it.
    let contents = fs::read_to_string(md_files[0].path()).unwrap();
    assert!(contents.contains("mailbox/inbox"));
    assert!(contents.contains("also-in:"));
    assert!(contents.contains("mailbox/inbox#1"));
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
