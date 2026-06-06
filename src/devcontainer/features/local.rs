use std::{fs, os::unix::fs::PermissionsExt, path::Path};

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};

use super::hex_lower;

pub(super) fn ensure_feature_files(source_dir: &Path) -> Result<()> {
    for name in ["install.sh", "devcontainer-feature.json"] {
        let path = source_dir.join(name);
        if !path.is_file() {
            bail!(
                "Feature directory must contain {name}: {}",
                source_dir.display()
            );
        }
    }

    Ok(())
}

pub(super) fn validate_local_feature_directory_name(
    source_dir: &Path,
    metadata_id: &str,
) -> Result<()> {
    let directory_name = source_dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            anyhow!(
                "Failed to resolve local Feature directory name: {}",
                source_dir.display()
            )
        })?;
    if directory_name != metadata_id {
        bail!(
            "Local Feature directory name must match Feature metadata id: {} has directory name `{}` but metadata id `{}`",
            source_dir.display(),
            directory_name,
            metadata_id
        );
    }

    Ok(())
}

pub(super) fn local_feature_content_digest(source_dir: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    hash_local_feature_directory(source_dir, source_dir, &mut hasher)?;
    let digest = hasher.finalize();

    Ok(format!("sha256:{}", hex_lower(&digest)))
}

fn hash_local_feature_directory(root: &Path, directory: &Path, hasher: &mut Sha256) -> Result<()> {
    let mut entries = fs::read_dir(directory)
        .with_context(|| {
            format!(
                "Failed to read local Feature directory: {}",
                directory.display()
            )
        })?
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| {
            format!(
                "Failed to enumerate local Feature directory: {}",
                directory.display()
            )
        })?;
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        let path = entry.path();
        let relative_path = path.strip_prefix(root).with_context(|| {
            format!(
                "Failed to relativize local Feature path: {}",
                path.display()
            )
        })?;
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("Failed to inspect local Feature path: {}", path.display()))?;
        hash_local_feature_entry_header(
            hasher,
            relative_path,
            local_feature_entry_kind(&metadata),
            metadata.permissions().mode(),
        );

        if metadata.is_dir() {
            hash_local_feature_directory(root, &path, hasher)?;
        } else if metadata.file_type().is_symlink() {
            let target = fs::read_link(&path).with_context(|| {
                format!("Failed to read local Feature symlink: {}", path.display())
            })?;
            hasher.update(target.as_os_str().as_encoded_bytes());
            hasher.update([0]);
        } else if metadata.is_file() {
            let contents = fs::read(&path).with_context(|| {
                format!("Failed to read local Feature file: {}", path.display())
            })?;
            hasher.update(contents.len().to_be_bytes());
            hasher.update(contents);
        }
    }

    Ok(())
}

fn hash_local_feature_entry_header(
    hasher: &mut Sha256,
    relative_path: &Path,
    kind: &'static [u8],
    mode: u32,
) {
    hasher.update(kind);
    hasher.update([0]);
    hasher.update(relative_path.as_os_str().as_encoded_bytes());
    hasher.update([0]);
    hasher.update((mode & 0o7777).to_be_bytes());
}

fn local_feature_entry_kind(metadata: &fs::Metadata) -> &'static [u8] {
    if metadata.is_dir() {
        b"dir"
    } else if metadata.file_type().is_symlink() {
        b"symlink"
    } else if metadata.is_file() {
        b"file"
    } else {
        b"other"
    }
}
