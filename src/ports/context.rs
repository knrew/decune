use std::{
    fs, io,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

use crate::{
    state::{PublishedPortRuntimeState, WorkspaceState, load_state_file},
    workspace::{is_valid_workspace_id, runtime_dir_for_workspace_id},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WorkspacePortContext {
    pub(super) workspace_id: String,
    pub(super) workspace_path: Option<String>,
    pub(super) runtime_dir: PathBuf,
    pub(super) published_ports: Vec<PublishedPortRuntimeState>,
}

#[derive(Debug, Clone)]
pub(super) struct StatePortEntry {
    pub(super) workspace_id: String,
    pub(super) state: Result<WorkspaceState, String>,
}

pub(super) fn load_port_states(root: &Path) -> Result<Vec<StatePortEntry>> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to read decune state root: {}", root.display()));
        }
    };
    let mut states = Vec::new();

    for entry in entries {
        let entry = entry.with_context(|| {
            format!("Failed to read decune state root entry: {}", root.display())
        })?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(workspace_id) = path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
        else {
            continue;
        };
        if !is_valid_workspace_id(&workspace_id) {
            continue;
        }
        match load_state_file(&path) {
            Ok(Some(state)) => states.push(StatePortEntry {
                workspace_id,
                state: Ok(state),
            }),
            Ok(None) => {}
            Err(error) => states.push(StatePortEntry {
                workspace_id,
                state: Err(format!("{error:#}")),
            }),
        }
    }

    Ok(states)
}

pub(super) fn context_for_workspace_id(workspace_id: &str) -> WorkspacePortContext {
    WorkspacePortContext {
        workspace_id: workspace_id.to_owned(),
        workspace_path: None,
        runtime_dir: runtime_dir_for_workspace_id(workspace_id),
        published_ports: Vec::new(),
    }
}
