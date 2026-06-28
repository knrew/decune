use std::{collections::BTreeMap, fs, path::PathBuf};

use crate::runtime::command::RuntimeOutput;
use crate::workspace::Workspace;

use super::{capabilities::ComposeCliCapabilities, command_plan::ComposeCommandPlan};

pub(super) fn fixture_workspace(name: &str) -> (tempfile::TempDir, Workspace) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join(name);
    fs::create_dir_all(&root).unwrap();
    (temp, Workspace::resolve(&root).unwrap())
}

pub(super) fn write_compose_file(path: impl AsRef<std::path::Path>, contents: &str) {
    fs::write(path, contents).unwrap();
}

pub(super) fn lifecycle_command_plan() -> ComposeCommandPlan {
    ComposeCommandPlan {
        project_name: "decune-project-abc123def456".to_owned(),
        project_directory: PathBuf::from("/workspace"),
        files: vec![PathBuf::from("/workspace/compose.yaml")],
        env: BTreeMap::new(),
        redactions: Vec::new(),
    }
}

pub(super) fn runtime_output(stdout: impl AsRef<[u8]>) -> RuntimeOutput {
    RuntimeOutput {
        stdout: stdout.as_ref().to_vec(),
        stderr: Vec::new(),
        exit_code: 0,
    }
}

pub(super) fn runtime_error_output(stderr: impl AsRef<[u8]>) -> RuntimeOutput {
    RuntimeOutput {
        stdout: Vec::new(),
        stderr: stderr.as_ref().to_vec(),
        exit_code: 1,
    }
}

pub(super) fn valid_compose_capabilities() -> ComposeCliCapabilities {
    ComposeCliCapabilities::from_help_outputs(
        Some("2.40.0".to_owned()),
        "Usage: docker compose config [OPTIONS]\n      --format string",
        "Usage: docker compose ps [OPTIONS]\n      --format string",
        "Usage: docker compose build [OPTIONS]\n      --with-dependencies --no-cache --pull",
        "Usage: docker compose pull [OPTIONS]\n      --policy string --ignore-buildable --include-deps",
        "Usage: docker compose up [OPTIONS]\n      --force-recreate --remove-orphans",
    )
}
