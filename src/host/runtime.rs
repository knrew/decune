use std::{fs, os::unix::fs::PermissionsExt, path::Path};

use anyhow::{Context, Result, bail};

const PRIVATE_RUNTIME_DIR_MODE: u32 = 0o700;

pub(crate) fn prepare_private_runtime_dir(runtime_dir: &Path, purpose: &str) -> Result<()> {
    create_runtime_dir(runtime_dir, purpose)?;
    set_private_runtime_parent(runtime_dir)?;
    set_runtime_dir_mode(runtime_dir, PRIVATE_RUNTIME_DIR_MODE, purpose)?;
    validate_runtime_dir_mode(runtime_dir, PRIVATE_RUNTIME_DIR_MODE, purpose)
}

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

pub(crate) fn create_runtime_dir(runtime_dir: &Path, purpose: &str) -> Result<()> {
    if runtime_dir
        .symlink_metadata()
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        bail!(
            "{} runtime directory must not be a symlink: {}",
            purpose,
            runtime_dir.display()
        );
    }

    fs::create_dir_all(runtime_dir).with_context(|| {
        format!(
            "Failed to create {} runtime directory: {}",
            purpose,
            runtime_dir.display()
        )
    })
}

pub(crate) fn set_runtime_dir_mode(runtime_dir: &Path, mode: u32, purpose: &str) -> Result<()> {
    fs::set_permissions(runtime_dir, fs::Permissions::from_mode(mode)).with_context(|| {
        format!(
            "Failed to set {} runtime directory permissions: {}",
            purpose,
            runtime_dir.display()
        )
    })
}

pub(crate) fn validate_runtime_dir_mode(
    runtime_dir: &Path,
    expected_mode: u32,
    purpose: &str,
) -> Result<()> {
    let metadata = fs::symlink_metadata(runtime_dir).with_context(|| {
        format!(
            "Failed to inspect {} runtime directory permissions: {}",
            purpose,
            runtime_dir.display()
        )
    })?;

    if metadata.file_type().is_symlink() {
        bail!(
            "{} runtime directory must not be a symlink: {}",
            purpose,
            runtime_dir.display()
        );
    }
    if !metadata.is_dir() {
        bail!(
            "{} runtime path is not a directory: {}",
            purpose,
            runtime_dir.display()
        );
    }

    let mode = metadata.permissions().mode() & 0o777;
    if mode != expected_mode {
        bail!(
            "{} runtime directory has insecure permissions: {} has {:03o}, expected {:03o}",
            purpose,
            runtime_dir.display(),
            mode,
            expected_mode
        );
    }

    Ok(())
}

fn is_decune_runtime_parent(path: &Path) -> bool {
    path.file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|name| name == "decune" || name.starts_with("decune-"))
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::{fs::PermissionsExt, fs::symlink},
    };

    use tempfile::TempDir;

    use super::prepare_private_runtime_dir;

    #[test]
    fn private_runtime_dir_is_created_with_private_permissions() {
        let temp = TempDir::new().unwrap();
        let runtime_dir = temp.path().join("runtime");

        prepare_private_runtime_dir(&runtime_dir, "test").unwrap();

        assert_eq!(
            fs::metadata(&runtime_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn private_runtime_dir_rejects_symlink_path() {
        let temp = TempDir::new().unwrap();
        let target = temp.path().join("target");
        let runtime_dir = temp.path().join("runtime");
        fs::create_dir_all(&target).unwrap();
        symlink(&target, &runtime_dir).unwrap();

        let error = prepare_private_runtime_dir(&runtime_dir, "test").unwrap_err();

        assert!(
            error
                .to_string()
                .contains("runtime directory must not be a symlink")
        );
    }
}
