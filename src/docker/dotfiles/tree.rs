use std::{
    collections::{BTreeMap, btree_map::Entry},
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

use super::MAX_DOTFILE_TREE_DEPTH;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DotfileTreeEntryKind {
    File,
    Directory,
}

#[derive(Debug, Clone)]
struct DotfileTreeEntry {
    kind: DotfileTreeEntryKind,
    real_path: PathBuf,
    from_symlink: bool,
}

#[derive(Debug, Default)]
pub(super) struct DotfileTree {
    entries: BTreeMap<PathBuf, DotfileTreeEntry>,
    has_symlink: bool,
}

#[derive(Debug)]
pub(super) struct DotfileDirectoryEntry {
    pub(super) name: String,
    pub(super) path: PathBuf,
}

impl DotfileTree {
    fn insert(
        &mut self,
        relative: PathBuf,
        kind: DotfileTreeEntryKind,
        real_path: PathBuf,
        from_symlink: bool,
    ) -> Result<()> {
        match self.entries.entry(relative) {
            Entry::Vacant(entry) => {
                entry.insert(DotfileTreeEntry {
                    kind,
                    real_path,
                    from_symlink,
                });
            }
            Entry::Occupied(entry) => {
                bail!(
                    "Dotfile source resolves the same target more than once: {}",
                    entry.key().display()
                );
            }
        }

        Ok(())
    }

    fn kinds(&self) -> BTreeMap<PathBuf, DotfileTreeEntryKind> {
        self.entries
            .iter()
            .map(|(relative, entry)| (relative.clone(), entry.kind))
            .collect()
    }
}

pub(super) fn collect_dotfile_tree(source: &Path) -> Result<DotfileTree> {
    let source = source.canonicalize().with_context(|| {
        format!(
            "Failed to canonicalize dotfile source: {}",
            source.display()
        )
    })?;
    let mut tree = DotfileTree::default();
    let mut ancestors = vec![source.clone()];

    collect_logical_directory(&source, Path::new(""), false, &mut ancestors, 0, &mut tree)?;

    Ok(tree)
}

pub(super) fn read_dotfile_directory_entries(source: &Path) -> Result<Vec<DotfileDirectoryEntry>> {
    let read_dir = fs::read_dir(source).with_context(|| {
        format!(
            "Failed to read dotfile source directory: {}",
            source.display()
        )
    })?;
    let mut entries = Vec::new();
    for entry in read_dir {
        let entry = entry
            .with_context(|| format!("Failed to read directory entry in: {}", source.display()))?;
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| {
            anyhow::anyhow!(
                "Dotfile source entry is not valid Unicode: {}",
                entry.path().display()
            )
        })?;
        entries.push(DotfileDirectoryEntry {
            name: name.to_owned(),
            path: entry.path(),
        });
    }
    entries.sort_by(|left, right| left.name.cmp(&right.name));

    Ok(entries)
}

pub(super) fn directory_contains_any_symlink(source: &Path) -> Result<bool> {
    let mut pending = vec![source.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in read_dotfile_directory_entries(&directory)? {
            let metadata = fs::symlink_metadata(&entry.path).with_context(|| {
                format!(
                    "Failed to read dotfile source metadata: {}",
                    entry.path.display()
                )
            })?;
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                return Ok(true);
            }
            if file_type.is_dir() {
                pending.push(entry.path);
            } else if !metadata.is_file() {
                bail!(
                    "Dotfile source entry must be a file, directory, or symlink: {}",
                    entry.path.display()
                );
            }
        }
    }

    Ok(false)
}

fn collect_logical_directory(
    source: &Path,
    relative_parent: &Path,
    from_symlink: bool,
    ancestors: &mut Vec<PathBuf>,
    depth: u32,
    tree: &mut DotfileTree,
) -> Result<()> {
    if depth > MAX_DOTFILE_TREE_DEPTH {
        bail!(
            "Maximum dotfile directory depth exceeded (possible circular symlinks): {}",
            source.display()
        );
    }

    for entry in read_dotfile_directory_entries(source)? {
        let relative = relative_parent.join(&entry.name);
        collect_logical_entry(
            &entry.path,
            relative,
            from_symlink,
            ancestors,
            depth + 1,
            tree,
        )?;
    }

    Ok(())
}

fn collect_logical_entry(
    path: &Path,
    relative: PathBuf,
    from_symlink: bool,
    ancestors: &mut Vec<PathBuf>,
    depth: u32,
    tree: &mut DotfileTree,
) -> Result<()> {
    if depth > MAX_DOTFILE_TREE_DEPTH {
        bail!(
            "Maximum dotfile directory depth exceeded (possible circular symlinks): {}",
            path.display()
        );
    }

    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("Failed to read dotfile source metadata: {}", path.display()))?;
    let file_type = metadata.file_type();

    if file_type.is_symlink() {
        tree.has_symlink = true;
        let metadata = fs::metadata(path).with_context(|| {
            format!(
                "Failed to read metadata for dotfile symlink (broken symlink?): {}",
                path.display()
            )
        })?;
        let real_path = path.canonicalize().with_context(|| {
            format!(
                "Failed to resolve dotfile symlink (broken symlink?): {}",
                path.display()
            )
        })?;
        if metadata.is_file() {
            tree.insert(relative, DotfileTreeEntryKind::File, real_path, true)?;
        } else if metadata.is_dir() {
            reject_circular_dotfile_directory(&real_path, path, ancestors)?;
            tree.insert(
                relative.clone(),
                DotfileTreeEntryKind::Directory,
                real_path.clone(),
                true,
            )?;
            ancestors.push(real_path.clone());
            collect_logical_directory(&real_path, &relative, true, ancestors, depth, tree)?;
            ancestors.pop();
        } else {
            bail!(
                "Dotfile symlink target must be a file or directory: {}",
                path.display()
            );
        }
    } else if metadata.is_file() {
        let real_path = path
            .canonicalize()
            .with_context(|| format!("Failed to canonicalize dotfile file: {}", path.display()))?;
        tree.insert(
            relative,
            DotfileTreeEntryKind::File,
            real_path,
            from_symlink,
        )?;
    } else if file_type.is_dir() {
        let real_path = path.canonicalize().with_context(|| {
            format!(
                "Failed to canonicalize dotfile directory: {}",
                path.display()
            )
        })?;
        reject_circular_dotfile_directory(&real_path, path, ancestors)?;
        tree.insert(
            relative.clone(),
            DotfileTreeEntryKind::Directory,
            real_path.clone(),
            from_symlink,
        )?;
        ancestors.push(real_path);
        collect_logical_directory(path, &relative, from_symlink, ancestors, depth, tree)?;
        ancestors.pop();
    } else {
        bail!(
            "Dotfile source entry must be a file, directory, or symlink: {}",
            path.display()
        );
    }

    Ok(())
}

pub(super) fn reject_circular_dotfile_directory(
    real_path: &Path,
    display_path: &Path,
    ancestors: &[PathBuf],
) -> Result<()> {
    if ancestors.iter().any(|ancestor| ancestor == real_path) {
        bail!(
            "Circular dotfile symlink detected while resolving: {}",
            display_path.display()
        );
    }

    Ok(())
}

pub(super) fn backing_root_mount_source(tree: &DotfileTree) -> Result<Option<PathBuf>> {
    let candidates = tree
        .entries
        .iter()
        .filter(|(_, entry)| entry.from_symlink)
        .filter_map(|(relative, entry)| backing_root_candidate(&entry.real_path, relative));
    for candidate in candidates {
        let Ok(candidate) = candidate.canonicalize() else {
            continue;
        };
        if backing_root_matches_tree(&candidate, tree)? {
            return Ok(Some(candidate));
        }
    }

    Ok(None)
}

fn backing_root_matches_tree(candidate: &Path, tree: &DotfileTree) -> Result<bool> {
    let Some(candidate_tree) = collect_physical_tree_without_symlinks(candidate)? else {
        return Ok(false);
    };
    if candidate_tree != tree.kinds() {
        return Ok(false);
    }

    for (relative, entry) in &tree.entries {
        let candidate_path = candidate.join(relative);
        let Ok(metadata) = fs::symlink_metadata(&candidate_path) else {
            return Ok(false);
        };
        if metadata.file_type().is_symlink() || kind_from_metadata(&metadata) != Some(entry.kind) {
            return Ok(false);
        }
        let Ok(real_path) = candidate_path.canonicalize() else {
            return Ok(false);
        };
        if real_path != entry.real_path {
            return Ok(false);
        }
    }

    Ok(true)
}

fn backing_root_candidate(real_path: &Path, relative: &Path) -> Option<PathBuf> {
    if relative.as_os_str().is_empty() || !real_path.ends_with(relative) {
        return None;
    }

    let mut candidate = real_path.to_path_buf();
    for _ in relative.components() {
        candidate.pop();
    }

    Some(candidate)
}

fn collect_physical_tree_without_symlinks(
    root: &Path,
) -> Result<Option<BTreeMap<PathBuf, DotfileTreeEntryKind>>> {
    let mut entries = BTreeMap::new();
    if collect_physical_directory_without_symlinks(root, Path::new(""), 0, &mut entries)? {
        Ok(Some(entries))
    } else {
        Ok(None)
    }
}

fn collect_physical_directory_without_symlinks(
    source: &Path,
    relative_parent: &Path,
    depth: u32,
    entries: &mut BTreeMap<PathBuf, DotfileTreeEntryKind>,
) -> Result<bool> {
    if depth > MAX_DOTFILE_TREE_DEPTH {
        return Ok(false);
    }

    let Ok(read_dir) = fs::read_dir(source) else {
        return Ok(false);
    };
    let mut directory_entries = Vec::new();
    for entry in read_dir {
        let Ok(entry) = entry else {
            return Ok(false);
        };
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Ok(false);
        };
        directory_entries.push((name.to_owned(), entry.path()));
    }
    directory_entries.sort_by(|left, right| left.0.cmp(&right.0));

    for (name, path) in directory_entries {
        let relative = relative_parent.join(name);
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            return Ok(false);
        };
        let Some(kind) = kind_from_metadata(&metadata) else {
            return Ok(false);
        };
        entries.insert(relative.clone(), kind);
        if kind == DotfileTreeEntryKind::Directory
            && !collect_physical_directory_without_symlinks(&path, &relative, depth + 1, entries)?
        {
            return Ok(false);
        }
    }

    Ok(true)
}

fn kind_from_metadata(metadata: &fs::Metadata) -> Option<DotfileTreeEntryKind> {
    let file_type = metadata.file_type();
    if metadata.is_file() {
        Some(DotfileTreeEntryKind::File)
    } else if file_type.is_dir() {
        Some(DotfileTreeEntryKind::Directory)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    #[cfg(unix)]
    use std::os::unix::fs as unix_fs;

    use super::*;
    use crate::{
        config::{
            path::ConfigPathOrigin,
            resolved::{ResolvedConfig, ResolvedDotfile},
            types::DotfileConflict,
            variables::{VariableContext, VariableContextInput},
        },
        docker::dotfiles::{DOTFILE_MOUNT_SKELETON_DIR, dotfile_mount_specs},
    };

    fn variables(workspace_root: &Path) -> VariableContext {
        VariableContext::new(VariableContextInput {
            local_workspace_folder: workspace_root.to_path_buf(),
            local_workspace_folder_basename: "project".to_owned(),
            container_workspace_folder: "/workspaces/project".to_owned(),
            container_workspace_folder_basename: "project".to_owned(),
            devcontainer_id: "abc123def456".to_owned(),
            uid: 1000,
            gid: 1000,
            remote_user: "vscode".to_owned(),
            remote_user_home: Some("/home/vscode".to_owned()),
        })
    }

    #[test]
    fn mounts_deep_directory_without_symlinks_directly() {
        let workspace = tempfile::tempdir().unwrap();
        let source = workspace.path().join("deep-dotfiles");
        let mut current = source.clone();
        for index in 0..=MAX_DOTFILE_TREE_DEPTH + 1 {
            current = current.join(format!("level-{index}"));
            fs::create_dir_all(&current).unwrap();
        }
        fs::write(current.join("config.txt"), "content").unwrap();
        let config = ResolvedConfig {
            dotfiles: vec![ResolvedDotfile {
                source: "deep-dotfiles".to_owned(),
                target: ".config/deep".to_owned(),
                read_only: true,
                resolve_symlink: true,
                on_conflict: DotfileConflict::Fail,
                origin: ConfigPathOrigin::Project,
            }],
            ..ResolvedConfig::default()
        };

        let mounts = dotfile_mount_specs(
            &config,
            workspace.path(),
            &variables(workspace.path()),
            workspace.path(),
        )
        .unwrap();

        assert_eq!(mounts.len(), 1);
        assert_eq!(
            mounts[0].source.as_deref(),
            Some(source.canonicalize().unwrap().to_str().unwrap())
        );
        assert_eq!(mounts[0].target, "/opt/decune/dotfiles/.config/deep");
        assert!(!workspace.path().join(DOTFILE_MOUNT_SKELETON_DIR).exists());
    }

    #[cfg(unix)]
    #[test]
    fn mounts_exact_symlink_farm_backing_root_directly() {
        let workspace = tempfile::tempdir().unwrap();
        let dotfiles_real = workspace.path().join("dotfiles-real");
        fs::create_dir_all(&dotfiles_real).unwrap();
        fs::write(dotfiles_real.join("config.yml"), "key: value\n").unwrap();

        let source_dir = workspace.path().join(".config/lazygit");
        fs::create_dir_all(&source_dir).unwrap();
        unix_fs::symlink(
            dotfiles_real.join("config.yml"),
            source_dir.join("config.yml"),
        )
        .unwrap();

        let config = ResolvedConfig {
            dotfiles: vec![ResolvedDotfile {
                source: ".config/lazygit".to_owned(),
                target: ".config/lazygit".to_owned(),
                read_only: true,
                resolve_symlink: true,
                on_conflict: DotfileConflict::Fail,
                origin: ConfigPathOrigin::Project,
            }],
            ..ResolvedConfig::default()
        };

        let mounts = dotfile_mount_specs(
            &config,
            workspace.path(),
            &variables(workspace.path()),
            workspace.path(),
        )
        .unwrap();

        assert_eq!(
            mounts[0].source.as_deref(),
            Some(dotfiles_real.canonicalize().unwrap().to_str().unwrap())
        );
        assert_eq!(mounts.len(), 1);
        assert!(!workspace.path().join(DOTFILE_MOUNT_SKELETON_DIR).exists());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_broken_symlink_in_dotfile_directory() {
        let workspace = tempfile::tempdir().unwrap();
        let source_dir = workspace.path().join("dotdir");
        fs::create_dir_all(&source_dir).unwrap();
        unix_fs::symlink(
            workspace.path().join("nonexistent"),
            source_dir.join("broken-link"),
        )
        .unwrap();
        let config = ResolvedConfig {
            dotfiles: vec![ResolvedDotfile {
                source: "dotdir".to_owned(),
                target: ".config/dotdir".to_owned(),
                read_only: true,
                resolve_symlink: true,
                on_conflict: DotfileConflict::Fail,
                origin: ConfigPathOrigin::Project,
            }],
            ..ResolvedConfig::default()
        };

        let error = dotfile_mount_specs(
            &config,
            workspace.path(),
            &variables(workspace.path()),
            workspace.path(),
        )
        .unwrap_err();
        let message = format!("{error:#}");

        assert!(
            message.contains("broken symlink"),
            "unexpected error: {message}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_circular_symlink_in_dotfile_directory() {
        let workspace = tempfile::tempdir().unwrap();
        let source_dir = workspace.path().join("dotdir");
        fs::create_dir_all(&source_dir).unwrap();
        unix_fs::symlink(&source_dir, source_dir.join("loop")).unwrap();
        let config = ResolvedConfig {
            dotfiles: vec![ResolvedDotfile {
                source: "dotdir".to_owned(),
                target: ".config/dotdir".to_owned(),
                read_only: true,
                resolve_symlink: true,
                on_conflict: DotfileConflict::Fail,
                origin: ConfigPathOrigin::Project,
            }],
            ..ResolvedConfig::default()
        };

        let error = dotfile_mount_specs(
            &config,
            workspace.path(),
            &variables(workspace.path()),
            workspace.path(),
        )
        .unwrap_err();
        let message = format!("{error:#}");

        assert!(
            message.contains("Circular dotfile symlink"),
            "unexpected error: {message}"
        );
    }
}
