use std::{
    fs,
    fs::File,
    io::{self, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const STATE_VERSION: u32 = 1;
pub(crate) fn state_file_path(state_dir: impl AsRef<Path>) -> PathBuf {
    state_dir.as_ref().join("state.toml")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkspaceState {
    pub(crate) version: u32,
    pub(crate) workspace: String,
    pub(crate) container_id: String,
    pub(crate) image: String,
    pub(crate) config_hash: String,
    #[serde(default)]
    pub(crate) config_file: Option<String>,
    #[serde(default)]
    pub(crate) compose_project_name: Option<String>,
    pub(crate) created_at: String,
    pub(crate) last_started_at: String,
    #[serde(default)]
    pub(crate) lifecycle: LifecycleState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LifecycleState {
    #[serde(default)]
    pub(crate) on_create_completed: bool,
    #[serde(default)]
    pub(crate) after_on_create_completed: bool,
    #[serde(default)]
    pub(crate) update_content_completed: bool,
    #[serde(default)]
    pub(crate) after_update_content_completed: bool,
    #[serde(default)]
    pub(crate) post_create_completed: bool,
    #[serde(default)]
    pub(crate) after_post_create_completed: bool,
}

impl LifecycleState {
    #[cfg(test)]
    pub(crate) fn all_completed() -> Self {
        Self {
            on_create_completed: true,
            after_on_create_completed: true,
            update_content_completed: true,
            after_update_content_completed: true,
            post_create_completed: true,
            after_post_create_completed: true,
        }
    }

    pub(crate) fn is_command_completed(self, completion: LifecycleCompletion) -> bool {
        match completion {
            LifecycleCompletion::OnCreate => self.on_create_completed,
            LifecycleCompletion::UpdateContent => self.update_content_completed,
            LifecycleCompletion::PostCreate => self.post_create_completed,
        }
    }

    pub(crate) fn is_after_hook_completed(self, completion: LifecycleCompletion) -> bool {
        match completion {
            LifecycleCompletion::OnCreate => self.after_on_create_completed,
            LifecycleCompletion::UpdateContent => self.after_update_content_completed,
            LifecycleCompletion::PostCreate => self.after_post_create_completed,
        }
    }

    pub(crate) fn is_completed(self, completion: LifecycleCompletion) -> bool {
        self.is_command_completed(completion) && self.is_after_hook_completed(completion)
    }

    pub(crate) fn mark_command_completed(&mut self, completion: LifecycleCompletion) {
        match completion {
            LifecycleCompletion::OnCreate => self.on_create_completed = true,
            LifecycleCompletion::UpdateContent => self.update_content_completed = true,
            LifecycleCompletion::PostCreate => self.post_create_completed = true,
        }
    }

    pub(crate) fn mark_after_hook_completed(&mut self, completion: LifecycleCompletion) {
        match completion {
            LifecycleCompletion::OnCreate => self.after_on_create_completed = true,
            LifecycleCompletion::UpdateContent => self.after_update_content_completed = true,
            LifecycleCompletion::PostCreate => self.after_post_create_completed = true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LifecycleCompletion {
    OnCreate,
    UpdateContent,
    PostCreate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StateContainerSnapshot {
    pub(crate) container_id: String,
    pub(crate) image: String,
    pub(crate) config_hash: String,
    pub(crate) config_file: Option<String>,
}

pub(crate) fn load_state_file(state_dir: impl AsRef<Path>) -> Result<Option<WorkspaceState>> {
    let path = state_file_path(state_dir);
    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to read decune state file: {}", path.display()));
        }
    };

    let state = toml::from_str::<WorkspaceState>(&content)
        .with_context(|| format!("Invalid decune state file: {}", path.display()))?;
    if state.version != STATE_VERSION {
        anyhow::bail!(
            "Unsupported decune state version {} in state file: {}",
            state.version,
            path.display()
        );
    }

    Ok(Some(state))
}

#[cfg(test)]
pub(crate) fn sync_state_with_container(
    state_dir: impl AsRef<Path>,
    workspace_root: &Path,
    container: StateContainerSnapshot,
    default_lifecycle: LifecycleState,
) -> Result<WorkspaceState> {
    sync_state_with_container_and_compose_project(
        state_dir,
        workspace_root,
        container,
        None,
        default_lifecycle,
    )
}

pub(crate) fn sync_state_with_container_and_compose_project(
    state_dir: impl AsRef<Path>,
    workspace_root: &Path,
    container: StateContainerSnapshot,
    compose_project_name: Option<String>,
    default_lifecycle: LifecycleState,
) -> Result<WorkspaceState> {
    let state_dir = state_dir.as_ref();
    let existing = load_state_file(state_dir).ok().flatten();
    let lifecycle = existing
        .as_ref()
        .filter(|state| state_matches_container(state, &container))
        .map(|state| state.lifecycle)
        .unwrap_or(default_lifecycle);
    let created_at = existing
        .as_ref()
        .filter(|state| state_matches_container(state, &container))
        .map(|state| state.created_at.clone())
        .unwrap_or_else(current_timestamp);

    write_state_for_container(
        state_dir,
        workspace_root,
        container,
        compose_project_name,
        lifecycle,
        Some(created_at),
    )
}

pub(crate) fn write_state_for_container(
    state_dir: impl AsRef<Path>,
    workspace_root: &Path,
    container: StateContainerSnapshot,
    compose_project_name: Option<String>,
    lifecycle: LifecycleState,
    created_at: Option<String>,
) -> Result<WorkspaceState> {
    let state_dir = state_dir.as_ref();
    let now = current_timestamp();
    let state = WorkspaceState {
        version: STATE_VERSION,
        workspace: workspace_root.display().to_string(),
        container_id: container.container_id,
        image: container.image,
        config_hash: container.config_hash,
        config_file: container.config_file,
        compose_project_name,
        created_at: created_at.unwrap_or_else(|| now.clone()),
        last_started_at: now,
        lifecycle,
    };
    write_state_file(state_dir, &state)?;

    Ok(state)
}

pub(crate) fn reconcile_state_without_container(state_dir: impl AsRef<Path>) -> Result<()> {
    let state_dir = state_dir.as_ref();
    if load_state_file(state_dir)?.is_some() {
        let path = state_file_path(state_dir);
        fs::remove_file(&path).with_context(|| {
            format!(
                "Failed to remove stale decune state file without Docker container: {}",
                path.display()
            )
        })?;
    }

    Ok(())
}

pub(crate) fn write_state_file(state_dir: impl AsRef<Path>, state: &WorkspaceState) -> Result<()> {
    let state_dir = state_dir.as_ref();
    fs::create_dir_all(state_dir).with_context(|| {
        format!(
            "Failed to create decune state directory: {}",
            state_dir.display()
        )
    })?;

    let path = state_file_path(state_dir);
    let temp_path = temporary_state_file_path(state_dir);
    let content = toml::to_string_pretty(state).context("Failed to serialize decune state")?;
    let mut file = create_state_temp_file(&temp_path)?;
    file.write_all(content.as_bytes()).with_context(|| {
        format!(
            "Failed to write temporary decune state file: {}",
            temp_path.display()
        )
    })?;
    file.sync_all().with_context(|| {
        format!(
            "Failed to flush temporary decune state file: {}",
            temp_path.display()
        )
    })?;
    drop(file);

    fs::rename(&temp_path, &path).with_context(|| {
        format!(
            "Failed to replace decune state file atomically: {}",
            path.display()
        )
    })?;
    sync_directory(state_dir)?;

    Ok(())
}

fn state_matches_container(state: &WorkspaceState, container: &StateContainerSnapshot) -> bool {
    state.container_id == container.container_id && state.config_hash == container.config_hash
}

fn temporary_state_file_path(state_dir: &Path) -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    state_dir.join(format!(
        "state.toml.tmp.{}.{}",
        std::process::id(),
        timestamp
    ))
}

fn current_timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    format!("unix:{seconds}")
}

fn create_state_temp_file(path: &Path) -> Result<File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    options.open(path).with_context(|| {
        format!(
            "Failed to create temporary decune state file: {}",
            path.display()
        )
    })
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("Failed to flush decune state directory: {}", path.display()))
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

    use super::{
        LifecycleState, StateContainerSnapshot, load_state_file, reconcile_state_without_container,
        remove_state_runtime_dirs, state_file_path, sync_state_with_container,
        sync_state_with_container_and_compose_project, write_state_file,
    };

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

    #[test]
    fn state_write_is_atomic_toml_and_leaves_no_temporary_file() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().join("state");
        let state = sync_state_with_container(
            &state_dir,
            Path::new("/workspace/project"),
            StateContainerSnapshot {
                container_id: "container-a".to_owned(),
                image: "decune/project:hash-a".to_owned(),
                config_hash: "hash-a".to_owned(),
                config_file: Some("/workspace/custom/devcontainer.json".to_owned()),
            },
            LifecycleState::default(),
        )
        .unwrap();

        write_state_file(&state_dir, &state).unwrap();

        let state_file = state_file_path(&state_dir);
        let content = fs::read_to_string(&state_file).unwrap();
        assert!(content.contains("version = 1"));
        assert!(content.contains("container_id = \"container-a\""));
        assert!(content.contains("config_file = \"/workspace/custom/devcontainer.json\""));
        assert_eq!(load_state_file(&state_dir).unwrap(), Some(state));
        let temp_files = fs::read_dir(&state_dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
            .count();
        assert_eq!(temp_files, 0);
    }

    #[test]
    fn state_can_persist_compose_project_name_without_requiring_it_in_legacy_files() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().join("state");
        let state = sync_state_with_container_and_compose_project(
            &state_dir,
            Path::new("/workspace/project"),
            StateContainerSnapshot {
                container_id: "container-a".to_owned(),
                image: "decune/project:hash-a".to_owned(),
                config_hash: "hash-a".to_owned(),
                config_file: Some("/workspace/.devcontainer/devcontainer.json".to_owned()),
            },
            Some("decune-project-abc123".to_owned()),
            LifecycleState::default(),
        )
        .unwrap();

        let state_file = state_file_path(&state_dir);
        let content = fs::read_to_string(&state_file).unwrap();
        assert!(content.contains("compose_project_name = \"decune-project-abc123\""));
        assert_eq!(
            load_state_file(&state_dir)
                .unwrap()
                .and_then(|state| state.compose_project_name),
            Some("decune-project-abc123".to_owned())
        );

        let legacy = content
            .lines()
            .filter(|line| !line.starts_with("compose_project_name = "))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&state_file, legacy).unwrap();

        let legacy_state = load_state_file(&state_dir).unwrap().unwrap();
        assert_eq!(legacy_state.compose_project_name, None);
        assert_eq!(legacy_state.container_id, state.container_id);
    }

    #[test]
    fn missing_state_is_regenerated_from_docker_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().join("state");

        let state = sync_state_with_container(
            &state_dir,
            Path::new("/workspace/project"),
            StateContainerSnapshot {
                container_id: "container-a".to_owned(),
                image: "decune/project:hash-a".to_owned(),
                config_hash: "hash-a".to_owned(),
                config_file: None,
            },
            LifecycleState::all_completed(),
        )
        .unwrap();

        assert_eq!(state.container_id, "container-a");
        assert_eq!(state.config_hash, "hash-a");
        assert!(state.lifecycle.on_create_completed);
        assert_eq!(load_state_file(&state_dir).unwrap(), Some(state));
    }

    #[test]
    fn stale_state_is_repaired_with_docker_snapshot_and_preserves_matching_lifecycle() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().join("state");
        let mut existing = sync_state_with_container(
            &state_dir,
            Path::new("/workspace/project"),
            StateContainerSnapshot {
                container_id: "container-a".to_owned(),
                image: "decune/project:hash-a".to_owned(),
                config_hash: "hash-a".to_owned(),
                config_file: None,
            },
            LifecycleState {
                on_create_completed: true,
                after_on_create_completed: true,
                update_content_completed: true,
                after_update_content_completed: true,
                post_create_completed: false,
                after_post_create_completed: false,
            },
        )
        .unwrap();
        existing.container_id = "stale-container".to_owned();
        write_state_file(&state_dir, &existing).unwrap();

        let repaired = sync_state_with_container(
            &state_dir,
            Path::new("/workspace/project"),
            StateContainerSnapshot {
                container_id: "container-a".to_owned(),
                image: "decune/project:hash-a".to_owned(),
                config_hash: "hash-a".to_owned(),
                config_file: None,
            },
            LifecycleState::all_completed(),
        )
        .unwrap();

        assert_eq!(repaired.container_id, "container-a");
        assert_eq!(repaired.lifecycle, LifecycleState::all_completed());
        assert_eq!(load_state_file(&state_dir).unwrap(), Some(repaired));
    }

    #[test]
    fn corrupt_state_without_docker_repair_reports_state_path() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        let state_file = state_file_path(&state_dir);
        fs::write(&state_file, "version = [").unwrap();

        let error = reconcile_state_without_container(&state_dir).unwrap_err();

        assert!(
            error
                .to_string()
                .contains(&state_file.display().to_string())
        );
        assert!(state_file.exists());
    }

    #[test]
    fn corrupt_state_is_repaired_when_docker_snapshot_is_available() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        let state_file = state_file_path(&state_dir);
        fs::write(&state_file, "version = [").unwrap();

        let repaired = sync_state_with_container(
            &state_dir,
            Path::new("/workspace/project"),
            StateContainerSnapshot {
                container_id: "container-a".to_owned(),
                image: "decune/project:hash-a".to_owned(),
                config_hash: "hash-a".to_owned(),
                config_file: None,
            },
            LifecycleState::all_completed(),
        )
        .unwrap();

        assert_eq!(repaired.container_id, "container-a");
        assert_eq!(load_state_file(&state_dir).unwrap(), Some(repaired));
    }

    #[test]
    fn stale_state_without_docker_container_is_removed() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().join("state");
        let state = sync_state_with_container(
            &state_dir,
            Path::new("/workspace/project"),
            StateContainerSnapshot {
                container_id: "container-a".to_owned(),
                image: "decune/project:hash-a".to_owned(),
                config_hash: "hash-a".to_owned(),
                config_file: None,
            },
            LifecycleState::default(),
        )
        .unwrap();
        write_state_file(&state_dir, &state).unwrap();

        reconcile_state_without_container(&state_dir).unwrap();

        assert!(!state_file_path(&state_dir).exists());
    }
}
