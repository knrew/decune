use std::path::{Path, PathBuf};

#[allow(dead_code)]
pub(crate) fn state_file_path(state_dir: impl AsRef<Path>) -> PathBuf {
    state_dir.as_ref().join("state.toml")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::state_file_path;

    #[test]
    fn state_file_lives_under_workspace_state_directory() {
        assert_eq!(
            state_file_path(Path::new("/tmp/decune-state")),
            Path::new("/tmp/decune-state/state.toml")
        );
    }
}
