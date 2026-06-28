use std::{fs, process::Command};

use crate::{harness::fake_path_with_commands, support, support::TempWorkspace};

#[test]
fn install_script_uses_portable_mktemp_template_on_darwin() {
    let workspace = TempWorkspace::new().unwrap();
    let install_dir = workspace.create_dir("install").unwrap();
    let tmp_base = workspace.create_dir("tmp").unwrap();
    let mktemp_args = workspace.path().join("mktemp-args");

    let test_path = fake_path_with_commands(
        &workspace,
        &[
            ("uname", "cli/install-script/uname-darwin.sh"),
            ("mktemp", "cli/install-script/mktemp-darwin.sh"),
            ("curl", "cli/install-script/curl-release.sh"),
            ("sha256sum", "cli/install-script/sha256sum-ok.sh"),
            ("tar", "cli/install-script/tar-decune-archive.sh"),
        ],
    );

    let output = Command::new("sh")
        .arg(support::repo_file("scripts/install.sh").unwrap())
        .arg("--version")
        .arg("1.2.3")
        .arg("--dir")
        .arg(&install_dir)
        .env("PATH", test_path)
        .env("TMPDIR", &tmp_base)
        .env("DECUNE_TEST_MKTEMP_ARGS", &mktemp_args)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "install script failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(install_dir.join("decune").is_file());
    assert_eq!(
        fs::read_to_string(&mktemp_args).unwrap(),
        format!("-d\n{}/decune.XXXXXXXXXX\n", tmp_base.display())
    );
}

#[test]
fn readme_documents_writable_install_directory_for_script_install() {
    let readme = fs::read_to_string(support::repo_file("README.md").unwrap()).unwrap();

    assert!(readme.contains("mkdir -p \"$HOME/.local/bin\""));
    assert!(readme.contains("--dir \"$HOME/.local/bin\""));
}
