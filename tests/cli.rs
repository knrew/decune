use assert_cmd::Command;
use predicates::prelude::*;

fn decune() -> Command {
    Command::cargo_bin("decune").unwrap()
}

#[test]
fn root_help_is_displayed() {
    decune()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Run dev containers from the command line.",
        ))
        .stdout(predicate::str::contains("up"))
        .stdout(predicate::str::contains("down"))
        .stdout(predicate::str::contains("clean"))
        .stdout(predicate::str::contains("rebuild"))
        .stderr(predicate::str::is_empty());
}

#[test]
fn command_help_is_displayed() {
    for command in ["up", "down", "clean", "rebuild"] {
        decune()
            .args([command, "--help"])
            .assert()
            .success()
            .stdout(predicate::str::contains("Workspace directory"))
            .stdout(predicate::str::contains("WORKSPACE"))
            .stderr(predicate::str::is_empty());
    }
}

#[test]
fn commands_fail_with_not_implemented_error() {
    for command in ["up", "down", "clean", "rebuild"] {
        decune()
            .arg(command)
            .assert()
            .failure()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Error:"))
            .stderr(predicate::str::contains("not implemented"));
    }
}
