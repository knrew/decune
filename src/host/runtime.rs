use std::{fs, os::unix::fs::PermissionsExt, path::Path};

use anyhow::{Context, Result};

pub(crate) fn set_private_runtime_parent(runtime_dir: &Path) -> Result<()> {
    let Some(parent) = runtime_dir
        .parent()
        .filter(|path| is_decune_runtime_parent(path))
    else {
        return Ok(());
    };

    fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).with_context(|| {
        format!(
            "Failed to set decune runtime parent directory permissions: {}",
            parent.display()
        )
    })
}

fn is_decune_runtime_parent(path: &Path) -> bool {
    path.file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|name| name == "decune" || name.starts_with("decune-"))
}
