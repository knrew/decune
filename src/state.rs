use std::{
    fs, io,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

#[allow(dead_code)]
pub(crate) fn state_file_path(state_dir: impl AsRef<Path>) -> PathBuf {
    state_dir.as_ref().join("state.toml")
}

pub(crate) fn remove_state_runtime_dirs(
    state_dir: impl AsRef<Path>,
    runtime_dir: impl AsRef<Path>,
) -> Result<()> {
    remove_dir_if_exists(state_dir.as_ref())?;
    remove_dir_if_exists(runtime_dir.as_ref())?;
    Ok(())
}

fn remove_dir_if_exists(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("Failed to remove decune directory: {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use super::{remove_state_runtime_dirs, state_file_path};

    #[test]
    fn state_file_lives_under_workspace_state_directory() {
        assert_eq!(
            state_file_path(Path::new("/tmp/decune-state")),
            Path::new("/tmp/decune-state/state.toml")
        );
    }

    #[test]
    fn remove_state_runtime_dirs_removes_existing_directories_and_ignores_missing_paths() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().join("state");
        let runtime_dir = temp.path().join("runtime");
        fs::create_dir_all(&state_dir).unwrap();
        fs::create_dir_all(&runtime_dir).unwrap();
        fs::write(state_dir.join("state.toml"), "version = 1\n").unwrap();
        fs::write(runtime_dir.join("socket"), "").unwrap();

        remove_state_runtime_dirs(&state_dir, &runtime_dir).unwrap();
        remove_state_runtime_dirs(&state_dir, &runtime_dir).unwrap();

        assert!(!state_dir.exists());
        assert!(!runtime_dir.exists());
    }
}
