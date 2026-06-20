use crate::harness::*;

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
fn version_is_displayed() {
    let output = decune()
        .arg("--version")
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .get_output()
        .stdout
        .clone();
    let output = String::from_utf8(output).unwrap();
    let base = format!("decune {}", env!("CARGO_PKG_VERSION"));

    assert!(
        output == format!("{base}\n") || output.starts_with(&format!("{base}+")),
        "unexpected version output: {output:?}"
    );
}

#[test]
fn short_version_is_displayed() {
    decune()
        .arg("-V")
        .assert()
        .success()
        .stdout(predicate::str::starts_with("decune "))
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
fn up_and_rebuild_help_list_no_global_config() {
    for command in ["up", "rebuild"] {
        decune()
            .args([command, "--help"])
            .assert()
            .success()
            .stdout(predicate::str::contains("--no-global-config"))
            .stderr(predicate::str::is_empty());
    }
}
