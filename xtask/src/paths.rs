use std::{
    env,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

pub(crate) fn workspace_root() -> Result<PathBuf> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .map(Path::to_path_buf)
        .context("Failed to determine workspace root from xtask manifest directory")
}

pub(crate) fn workspace_relative(workspace: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.join(path)
    }
}

pub(crate) fn target_dir(workspace: &Path) -> PathBuf {
    target_dir_from_env_value(
        workspace,
        env::var_os("CARGO_TARGET_DIR").map(PathBuf::from),
    )
}

pub(crate) fn target_dir_from_env_value(workspace: &Path, value: Option<PathBuf>) -> PathBuf {
    match value {
        Some(path) if path.is_absolute() => path,
        Some(path) => workspace.join(path),
        None => workspace.join("target"),
    }
}

pub(crate) fn resolve_dist_dir(workspace: &Path, path: Option<&Path>) -> PathBuf {
    match path {
        Some(path) => workspace_relative(workspace, path),
        None => target_dir(workspace).join("dist"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_dir_resolves_relative_cargo_target_dir_against_workspace() {
        let workspace = Path::new("/workspace/decune");
        assert_eq!(
            target_dir_from_env_value(workspace, Some(PathBuf::from("target-custom"))),
            PathBuf::from("/workspace/decune/target-custom"),
        );
    }

    #[test]
    fn target_dir_preserves_absolute_cargo_target_dir() {
        let workspace = Path::new("/workspace/decune");
        assert_eq!(
            target_dir_from_env_value(workspace, Some(PathBuf::from("/tmp/target-custom"))),
            PathBuf::from("/tmp/target-custom"),
        );
    }

    #[test]
    fn target_dir_defaults_to_workspace_target() {
        let workspace = Path::new("/workspace/decune");
        assert_eq!(
            target_dir_from_env_value(workspace, None),
            PathBuf::from("/workspace/decune/target"),
        );
    }
}
