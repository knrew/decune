use std::process::Command;

fn decune() -> Command {
    Command::new(env!("CARGO_BIN_EXE_decune"))
}

#[test]
fn root_help_is_displayed() {
    let output = decune().arg("--help").output().unwrap();

    assert!(output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("Run dev containers from the command line.")
    );
}

#[test]
fn command_help_is_displayed() {
    for command in ["up", "down", "clean", "rebuild"] {
        let output = decune().args([command, "--help"]).output().unwrap();

        assert!(output.status.success(), "{command} --help should succeed");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("Workspace directory"),
            "{command} --help should describe the workspace argument"
        );
    }
}

#[test]
fn commands_fail_with_not_implemented_error() {
    for command in ["up", "down", "clean", "rebuild"] {
        let output = decune().arg(command).output().unwrap();

        assert!(!output.status.success(), "{command} should fail for now");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("Error:"),
            "{command} should format the top-level error"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("not implemented"),
            "{command} should report that it is not implemented"
        );
        assert!(
            output.stdout.is_empty(),
            "{command} should not write command-result values to stdout"
        );
    }
}
