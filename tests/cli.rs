use assert_cmd::Command;
use predicates::prelude::*;

fn pigeon() -> Command {
    Command::cargo_bin("pigeon").unwrap()
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
fn authenticate_is_not_yet_implemented() {
    pigeon()
        .args([
            "email",
            "authenticate",
            "first.last@example.com",
            "--alias",
            "first-last",
        ])
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::contains("Not Yet Implemented"));
}

#[test]
fn list_identities_is_not_yet_implemented() {
    pigeon()
        .args(["email", "list-identities"])
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::contains("Not Yet Implemented"));
}

#[test]
fn sink_is_not_yet_implemented() {
    pigeon()
        .args(["email", "sink", "first-last", "--directory", "/tmp/first"])
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::contains("Not Yet Implemented"));
}

#[test]
fn transform_is_not_yet_implemented() {
    pigeon()
        .args([
            "email",
            "transform",
            "--input",
            "/tmp/first",
            "--output",
            "/tmp/second",
            "--normalize",
            "--mbox-to-markdown",
        ])
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::contains("Not Yet Implemented"));
}
