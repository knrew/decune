#[path = "support/support.rs"]
mod support;

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;

    use super::support::{self, TempWorkspace};

    #[test]
    fn temp_workspace_creates_unique_directory() {
        let first = TempWorkspace::new().unwrap();
        let second = TempWorkspace::new().unwrap();

        assert!(first.path().is_dir());
        assert!(second.path().is_dir());
        assert_ne!(first.path(), second.path());
    }

    #[test]
    fn temp_workspace_writes_nested_files() {
        let workspace = TempWorkspace::new().unwrap();

        let file = workspace
            .write_file(".devcontainer/devcontainer.json", br#"{"image":"ubuntu"}"#)
            .unwrap();

        assert_eq!(fs::read(&file).unwrap(), br#"{"image":"ubuntu"}"#);
        assert!(workspace.path().join(".devcontainer").is_dir());
    }

    #[test]
    fn temp_workspace_creates_nested_directories() {
        let workspace = TempWorkspace::new().unwrap();

        let directory = workspace.create_dir(".decune/cache").unwrap();

        assert_eq!(directory, workspace.path().join(".decune/cache"));
        assert!(directory.is_dir());
    }

    #[test]
    fn temp_workspace_rejects_paths_outside_workspace() {
        let workspace = TempWorkspace::new().unwrap();

        let error = workspace.write_file("../outside", b"contents").unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn fixture_path_rejects_paths_outside_fixture_root() {
        let error = support::fixture_path("../outside").unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn temp_workspace_writes_fixture_files() {
        let workspace = TempWorkspace::new().unwrap();

        let file = workspace
            .write_fixture_file("copied/hello.sh", "cli/harness/hello.sh")
            .unwrap();

        assert_eq!(
            fs::read_to_string(file).unwrap(),
            "#!/bin/sh\nset -eu\nprintf 'hello\\n'\n"
        );
    }

    #[test]
    fn temp_workspace_writes_executable_fixture_files() {
        let workspace = TempWorkspace::new().unwrap();

        let file = workspace
            .write_executable_fixture("bin/hello", "cli/harness/hello.sh")
            .unwrap();

        assert_eq!(
            fs::metadata(file).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[test]
    fn temp_workspace_copies_fixture_directories() {
        let workspace = TempWorkspace::new().unwrap();

        workspace.copy_fixture_dir("cli/harness").unwrap();

        assert!(workspace.path().join("hello.sh").is_file());
    }

    #[test]
    fn temp_workspace_copies_fixture_directories_to_destination() {
        let workspace = TempWorkspace::new().unwrap();

        workspace
            .copy_fixture_dir_to("cli/harness", "copied-harness")
            .unwrap();

        assert!(workspace.path().join("copied-harness/hello.sh").is_file());
    }

    #[test]
    fn temp_workspace_writes_fixture_templates() {
        let workspace = TempWorkspace::new().unwrap();

        let file = workspace
            .write_fixture_template(
                "rendered.txt",
                "cli/harness/template.txt",
                &[("__NAME__", "workspace")],
            )
            .unwrap();

        assert_eq!(fs::read_to_string(file).unwrap(), "name=workspace\n");
    }

    #[test]
    fn temp_workspace_writes_executable_contents() {
        let workspace = TempWorkspace::new().unwrap();

        let file = workspace
            .write_executable("bin/generated", "#!/bin/sh\nexit 0\n")
            .unwrap();

        assert_eq!(
            fs::metadata(file).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[test]
    fn path_with_prepended_puts_directory_first() {
        let workspace = TempWorkspace::new().unwrap();
        let bin_dir = workspace.create_dir("bin").unwrap();
        let path = support::path_with_prepended(&bin_dir).unwrap();
        let mut entries = std::env::split_paths(&path);

        assert_eq!(entries.next().as_deref(), Some(bin_dir.as_path()));
    }

    #[test]
    fn render_fixture_template_replaces_placeholders() {
        let rendered =
            support::render_fixture_template("cli/harness/template.txt", &[("__NAME__", "decune")])
                .unwrap();

        assert_eq!(rendered, "name=decune\n");
    }

    #[test]
    fn render_fixture_template_rejects_missing_placeholders() {
        let error = support::render_fixture_template(
            "cli/harness/template.txt",
            &[("__MISSING__", "decune")],
        )
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }
}
