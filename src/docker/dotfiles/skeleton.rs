use std::{
    collections::{BTreeMap, BTreeSet, btree_map::Entry},
    fs,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail};

use crate::docker::mounts::DockerMountSpec;

use super::{
    DOTFILE_BACKINGS_MOUNT_ROOT, DOTFILE_MOUNT_SKELETON_DIR, DotfileMountPlan, MAX_DOTFILE_MOUNTS,
    MAX_DOTFILE_TREE_DEPTH, dotfile_bind_mount,
    targets::relative_target_components,
    tree::{read_dotfile_directory_entries, reject_circular_dotfile_directory},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DotfileSkeletonPlan {
    pub(crate) root: PathBuf,
    pub(crate) read_only: bool,
    entries: BTreeMap<PathBuf, DotfileSkeletonEntryKind>,
}

/// Assigns backing mount targets uniquely across the whole dotfile mount plan.
/// A shared backing mount is emitted only into the plan of the first entry that
/// registers it, so per-entry plans are not self-contained: all entries' mounts
/// must be merged into a single container mount set.
#[derive(Debug, Default)]
pub(super) struct DotfileBackingMountRegistry {
    targets: BTreeMap<(PathBuf, bool), String>,
}

impl DotfileBackingMountRegistry {
    fn target_for(&mut self, source: &Path, read_only: bool) -> (String, bool) {
        let next_target = format!("{DOTFILE_BACKINGS_MOUNT_ROOT}/{}", self.targets.len());
        match self.targets.entry((source.to_path_buf(), read_only)) {
            Entry::Vacant(entry) => {
                entry.insert(next_target.clone());
                (next_target, true)
            }
            Entry::Occupied(entry) => (entry.get().clone(), false),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DotfileSkeletonEntryKind {
    Symlink { target: String },
    Directory,
}

pub(crate) fn materialize_dotfile_skeletons(skeletons: &[DotfileSkeletonPlan]) -> Result<()> {
    for skeleton in skeletons {
        materialize_dotfile_skeleton(skeleton).with_context(|| {
            format!(
                "Failed to prepare dotfile mount skeleton: {}",
                skeleton.root.display()
            )
        })?;
    }

    Ok(())
}

pub(super) fn skeleton_dotfile_mount_plan(
    source: &Path,
    dotfile_target: &str,
    container_target: &str,
    state_root: &Path,
    read_only: bool,
    backing_mounts: &mut DotfileBackingMountRegistry,
) -> Result<DotfileMountPlan> {
    let components = relative_target_components(dotfile_target)?;
    let skeleton_root = state_root
        .join(DOTFILE_MOUNT_SKELETON_DIR)
        .join(components.join("/"));
    let mounts = vec![dotfile_bind_mount(
        &skeleton_root,
        container_target.to_owned(),
        read_only,
    )];
    let source = source.canonicalize().with_context(|| {
        format!(
            "Failed to canonicalize dotfile source: {}",
            source.display()
        )
    })?;
    let mut ancestors = vec![source.clone()];
    let mut builder = DotfileSkeletonBuilder {
        container_root: container_target,
        read_only,
        mounts,
        skeleton_entries: BTreeMap::new(),
        backing_files: BTreeMap::new(),
        backing_sources: BTreeSet::new(),
    };
    builder.build_directory(&source, Path::new(""), &mut ancestors, 0)?;
    builder.finalize_backing_mounts(backing_mounts)?;
    builder.mounts[1..].sort_by(|left, right| {
        container_path_depth(&left.target)
            .cmp(&container_path_depth(&right.target))
            .then_with(|| left.target.cmp(&right.target))
    });

    Ok(DotfileMountPlan {
        mounts: builder.mounts,
        skeletons: vec![DotfileSkeletonPlan {
            root: skeleton_root,
            read_only,
            entries: builder.skeleton_entries,
        }],
    })
}

struct DotfileSkeletonBuilder<'a> {
    container_root: &'a str,
    read_only: bool,
    mounts: Vec<DockerMountSpec>,
    skeleton_entries: BTreeMap<PathBuf, DotfileSkeletonEntryKind>,
    backing_files: BTreeMap<PathBuf, DotfileBackingFile>,
    backing_sources: BTreeSet<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DotfileBackingFile {
    source: PathBuf,
    relative: PathBuf,
}

impl DotfileSkeletonBuilder<'_> {
    fn build_directory(
        &mut self,
        source: &Path,
        relative_parent: &Path,
        ancestors: &mut Vec<PathBuf>,
        depth: u32,
    ) -> Result<()> {
        if depth > MAX_DOTFILE_TREE_DEPTH {
            bail!(
                "Maximum dotfile directory depth exceeded (possible circular symlinks): {}",
                source.display()
            );
        }

        for entry in read_dotfile_directory_entries(source)? {
            let relative = relative_parent.join(&entry.name);
            self.build_entry(&entry.path, relative, ancestors, depth + 1)?;
        }

        Ok(())
    }

    fn build_entry(
        &mut self,
        path: &Path,
        relative: PathBuf,
        ancestors: &mut Vec<PathBuf>,
        depth: u32,
    ) -> Result<()> {
        if depth > MAX_DOTFILE_TREE_DEPTH {
            bail!(
                "Maximum dotfile directory depth exceeded (possible circular symlinks): {}",
                path.display()
            );
        }

        let metadata = fs::symlink_metadata(path).with_context(|| {
            format!("Failed to read dotfile source metadata: {}", path.display())
        })?;
        let file_type = metadata.file_type();

        if file_type.is_symlink() {
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
                self.push_backed_file(relative, &real_path)?;
            } else if metadata.is_dir() {
                self.build_directory_or_mount(&real_path, &relative, ancestors, depth)?;
            } else {
                bail!(
                    "Dotfile symlink target must be a file or directory: {}",
                    path.display()
                );
            }
        } else if metadata.is_file() {
            let real_path = path.canonicalize().with_context(|| {
                format!("Failed to canonicalize dotfile file: {}", path.display())
            })?;
            self.push_backed_file(relative, &real_path)?;
        } else if file_type.is_dir() {
            let real_path = path.canonicalize().with_context(|| {
                format!(
                    "Failed to canonicalize dotfile directory: {}",
                    path.display()
                )
            })?;
            self.build_directory_or_mount(&real_path, &relative, ancestors, depth)?;
        } else {
            bail!(
                "Dotfile source entry must be a file, directory, or symlink: {}",
                path.display()
            );
        }

        Ok(())
    }

    fn build_directory_or_mount(
        &mut self,
        real_path: &Path,
        relative: &Path,
        ancestors: &mut Vec<PathBuf>,
        depth: u32,
    ) -> Result<()> {
        reject_circular_dotfile_directory(real_path, real_path, ancestors)?;
        self.push_skeleton_entry(relative.to_path_buf(), DotfileSkeletonEntryKind::Directory)?;

        if !directory_contains_symlink(real_path, ancestors, depth)? {
            self.push_mount(
                real_path,
                container_child_target(self.container_root, relative)?,
            )?;
            return Ok(());
        }

        ancestors.push(real_path.to_path_buf());
        self.build_directory(real_path, relative, ancestors, depth)?;
        ancestors.pop();

        Ok(())
    }

    fn push_skeleton_entry(
        &mut self,
        relative: PathBuf,
        kind: DotfileSkeletonEntryKind,
    ) -> Result<()> {
        if self.backing_files.contains_key(&relative) {
            bail!(
                "Dotfile skeleton path generated with conflicting entry kinds: {}",
                relative.display()
            );
        }
        self.insert_skeleton_entry(relative, kind)
    }

    fn insert_skeleton_entry(
        &mut self,
        relative: PathBuf,
        kind: DotfileSkeletonEntryKind,
    ) -> Result<()> {
        match self.skeleton_entries.entry(relative) {
            Entry::Vacant(entry) => {
                entry.insert(kind);
            }
            Entry::Occupied(entry) if entry.get() == &kind => {}
            Entry::Occupied(entry) => {
                bail!(
                    "Dotfile skeleton path generated with conflicting entry kinds: {}",
                    entry.key().display()
                );
            }
        }

        Ok(())
    }

    fn push_backed_file(&mut self, relative: PathBuf, real_path: &Path) -> Result<()> {
        if self.skeleton_entries.contains_key(&relative) {
            bail!(
                "Dotfile skeleton path generated with conflicting entry kinds: {}",
                relative.display()
            );
        }

        let backing_file = backing_file(real_path)?;
        if let Some(existing) = self.backing_files.get(&relative)
            && existing != &backing_file
        {
            bail!(
                "Dotfile skeleton path generated with conflicting backing files: {}",
                relative.display()
            );
        }
        self.backing_sources.insert(backing_file.source.clone());
        self.backing_files.insert(relative, backing_file);

        Ok(())
    }

    fn finalize_backing_mounts(
        &mut self,
        backing_mounts: &mut DotfileBackingMountRegistry,
    ) -> Result<()> {
        let mut backing_targets = BTreeMap::new();
        for source in &self.backing_sources {
            let (target, is_new) = backing_mounts.target_for(source, self.read_only);
            backing_targets.insert(source.clone(), target.clone());
            if is_new {
                self.mounts
                    .push(dotfile_bind_mount(source, target, self.read_only));
            }
        }

        let backing_entries = self
            .backing_files
            .iter()
            .map(|(relative, backing_file)| (relative.clone(), backing_file.clone()))
            .collect::<Vec<_>>();
        for (relative, backing_file) in backing_entries {
            let backing_root = backing_targets.get(&backing_file.source).ok_or_else(|| {
                anyhow::anyhow!(
                    "Dotfile backing directory missing for skeleton entry: {}",
                    relative.display()
                )
            })?;
            self.insert_skeleton_entry(
                relative,
                DotfileSkeletonEntryKind::Symlink {
                    target: container_child_target(backing_root, &backing_file.relative)?,
                },
            )?;
        }

        Ok(())
    }

    fn push_mount(&mut self, source: &Path, target: String) -> Result<()> {
        if self.mounts.iter().any(|mount| mount.target == target) {
            bail!("Dotfile mount target generated more than once: {target}");
        }
        self.mounts
            .push(dotfile_bind_mount(source, target, self.read_only));
        if self.mounts.len() > MAX_DOTFILE_MOUNTS {
            bail!(
                "Dotfile target generates too many bind mounts ({} > {})",
                self.mounts.len(),
                MAX_DOTFILE_MOUNTS
            );
        }

        Ok(())
    }
}

fn backing_file(real_path: &Path) -> Result<DotfileBackingFile> {
    let source = real_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Dotfile file has no parent: {}", real_path.display()))?
        .to_path_buf();
    let file_name = real_path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("Dotfile file has no file name: {}", real_path.display()))?;

    Ok(DotfileBackingFile {
        source,
        relative: PathBuf::from(file_name),
    })
}

fn directory_contains_symlink(source: &Path, ancestors: &[PathBuf], depth: u32) -> Result<bool> {
    if depth > MAX_DOTFILE_TREE_DEPTH {
        bail!(
            "Maximum dotfile directory depth exceeded (possible circular symlinks): {}",
            source.display()
        );
    }
    let source = source.canonicalize().with_context(|| {
        format!(
            "Failed to canonicalize dotfile directory: {}",
            source.display()
        )
    })?;
    reject_circular_dotfile_directory(&source, &source, ancestors)?;

    let mut ancestors = ancestors.to_vec();
    ancestors.push(source.clone());
    for entry in read_dotfile_directory_entries(&source)? {
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
            if directory_contains_symlink(&entry.path, &ancestors, depth + 1)? {
                return Ok(true);
            }
        } else if !metadata.is_file() {
            bail!(
                "Dotfile source entry must be a file, directory, or symlink: {}",
                entry.path.display()
            );
        }
    }

    Ok(false)
}

fn materialize_dotfile_skeleton(skeleton: &DotfileSkeletonPlan) -> Result<()> {
    materialize_skeleton_root(&skeleton.root)?;

    for (relative, kind) in &skeleton.entries {
        let path = skeleton.root.join(relative);
        match kind {
            DotfileSkeletonEntryKind::Directory => {
                materialize_skeleton_directory(&path, skeleton.read_only)?;
            }
            DotfileSkeletonEntryKind::Symlink { target } => {
                materialize_skeleton_symlink(&path, target, skeleton.read_only)?;
            }
        }
    }

    if skeleton.read_only {
        remove_stale_skeleton_entries(&skeleton.root, &skeleton.entries)?;
    }

    Ok(())
}

fn materialize_skeleton_root(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(()),
        Ok(metadata) => {
            remove_skeleton_path(path, &metadata)?;
            fs::create_dir_all(path).with_context(|| {
                format!(
                    "Failed to create dotfile mount skeleton directory: {}",
                    path.display()
                )
            })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir_all(path)
            .with_context(|| {
                format!(
                    "Failed to create dotfile mount skeleton directory: {}",
                    path.display()
                )
            }),
        Err(error) => Err(error).with_context(|| {
            format!(
                "Failed to read dotfile mount skeleton path: {}",
                path.display()
            )
        }),
    }
}

fn materialize_skeleton_directory(path: &Path, replace_conflict: bool) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(()),
        Ok(metadata) if replace_conflict => {
            remove_skeleton_path(path, &metadata)?;
            fs::create_dir_all(path).with_context(|| {
                format!(
                    "Failed to create dotfile mount skeleton directory: {}",
                    path.display()
                )
            })
        }
        Ok(_) => bail!(
            "Dotfile mount skeleton path conflicts with desired directory: {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir_all(path)
            .with_context(|| {
                format!(
                    "Failed to create dotfile mount skeleton directory: {}",
                    path.display()
                )
            }),
        Err(error) => Err(error).with_context(|| {
            format!(
                "Failed to read dotfile mount skeleton path: {}",
                path.display()
            )
        }),
    }
}

fn materialize_skeleton_symlink(
    path: &Path,
    target: &str,
    replace_parent_conflict: bool,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        materialize_skeleton_directory(parent, replace_parent_conflict)?;
    }

    match fs::symlink_metadata(path) {
        Ok(metadata)
            if metadata.file_type().is_symlink()
                && fs::read_link(path).ok().as_deref() == Some(Path::new(target)) =>
        {
            Ok(())
        }
        Ok(metadata) => {
            remove_skeleton_path(path, &metadata)?;
            create_skeleton_symlink(path, target)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            create_skeleton_symlink(path, target)
        }
        Err(error) => Err(error).with_context(|| {
            format!(
                "Failed to read dotfile mount skeleton path: {}",
                path.display()
            )
        }),
    }
}

fn create_skeleton_symlink(path: &Path, target: &str) -> Result<()> {
    std::os::unix::fs::symlink(target, path).with_context(|| {
        format!(
            "Failed to create dotfile mount skeleton symlink: {} -> {}",
            path.display(),
            target
        )
    })?;

    Ok(())
}

fn remove_stale_skeleton_entries(
    root: &Path,
    desired: &BTreeMap<PathBuf, DotfileSkeletonEntryKind>,
) -> Result<()> {
    remove_stale_skeleton_entries_in_directory(root, Path::new(""), desired)
}

fn remove_stale_skeleton_entries_in_directory(
    directory: &Path,
    relative_parent: &Path,
    desired: &BTreeMap<PathBuf, DotfileSkeletonEntryKind>,
) -> Result<()> {
    for entry in fs::read_dir(directory).with_context(|| {
        format!(
            "Failed to read dotfile mount skeleton directory: {}",
            directory.display()
        )
    })? {
        let entry = entry.with_context(|| {
            format!(
                "Failed to read directory entry in dotfile mount skeleton: {}",
                directory.display()
            )
        })?;
        let relative = relative_parent.join(PathBuf::from(entry.file_name()));
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).with_context(|| {
            format!(
                "Failed to read dotfile mount skeleton path: {}",
                path.display()
            )
        })?;
        if !skeleton_relative_should_keep(&relative, desired) {
            remove_skeleton_path(&path, &metadata)?;
            continue;
        }
        if metadata.file_type().is_dir() {
            remove_stale_skeleton_entries_in_directory(&path, &relative, desired)?;
        }
    }

    Ok(())
}

fn skeleton_relative_should_keep(
    relative: &Path,
    desired: &BTreeMap<PathBuf, DotfileSkeletonEntryKind>,
) -> bool {
    desired.contains_key(relative) || desired.keys().any(|entry| entry.starts_with(relative))
}

fn remove_skeleton_path(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    if metadata.file_type().is_dir() {
        fs::remove_dir_all(path).with_context(|| {
            format!(
                "Failed to remove dotfile mount skeleton directory: {}",
                path.display()
            )
        })
    } else {
        fs::remove_file(path).with_context(|| {
            format!(
                "Failed to remove dotfile mount skeleton path: {}",
                path.display()
            )
        })
    }
}

fn container_child_target(root: &str, relative: &Path) -> Result<String> {
    let suffix = relative_path_suffix(relative)?;
    if suffix.is_empty() {
        Ok(root.to_owned())
    } else {
        Ok(format!("{root}/{suffix}"))
    }
}

fn relative_path_suffix(path: &Path) -> Result<String> {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => {
                let value = value.to_str().ok_or_else(|| {
                    anyhow::anyhow!(
                        "Dotfile source entry is not valid Unicode: {}",
                        path.display()
                    )
                })?;
                components.push(value.to_owned());
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!(
                    "Dotfile source entry resolved to an invalid relative path: {}",
                    path.display()
                );
            }
        }
    }

    Ok(components.join("/"))
}

fn container_path_depth(path: &str) -> usize {
    path.split('/')
        .filter(|component| !component.is_empty())
        .count()
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, fs, path::Path};

    #[cfg(unix)]
    use std::os::unix::{fs as unix_fs, fs::MetadataExt};

    use anyhow::Result;

    use super::*;
    use crate::{
        config::{
            path::ConfigPathOrigin,
            resolved::{ResolvedConfig, ResolvedDotfile},
            types::DotfileConflict,
            variables::{VariableContext, VariableContextInput},
        },
        docker::{
            dotfiles::{DOTFILE_MOUNT_SKELETON_DIR, dotfile_mount_plan},
            mounts::DockerMountSpec,
        },
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

    fn materialized_dotfile_mount_specs(
        config: &ResolvedConfig,
        workspace_root: &Path,
        variables: &VariableContext,
        state_root: &Path,
    ) -> Result<Vec<DockerMountSpec>> {
        let plan = dotfile_mount_plan(config, workspace_root, variables, state_root)?;
        materialize_dotfile_skeletons(&plan.skeletons)?;
        Ok(plan.mounts)
    }

    fn backing_mount_target_for_source(mounts: &[DockerMountSpec], source: &Path) -> String {
        let source = source.canonicalize().unwrap();
        mounts
            .iter()
            .find(|mount| {
                mount.source.as_deref() == source.to_str()
                    && mount.target.starts_with(DOTFILE_BACKINGS_MOUNT_ROOT)
            })
            .expect("expected dotfile backing mount")
            .target
            .clone()
    }

    fn backing_mount_target_for_source_and_mode(
        mounts: &[DockerMountSpec],
        source: &Path,
        read_only: bool,
    ) -> String {
        let source = source.canonicalize().unwrap();
        mounts
            .iter()
            .find(|mount| {
                mount.source.as_deref() == source.to_str()
                    && mount.target.starts_with(DOTFILE_BACKINGS_MOUNT_ROOT)
                    && mount.read_only == read_only
            })
            .expect("expected dotfile backing mount")
            .target
            .clone()
    }

    #[cfg(unix)]
    fn assert_skeleton_symlink(path: &Path, target: &str) {
        assert!(path.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(fs::read_link(path).unwrap(), Path::new(target));
    }

    #[cfg(unix)]
    #[test]
    fn dotfile_mount_plan_does_not_materialize_skeleton() {
        let workspace = tempfile::tempdir().unwrap();
        let dotfiles_real = workspace.path().join("dotfiles-real");
        fs::create_dir_all(&dotfiles_real).unwrap();
        fs::write(dotfiles_real.join("config.yml"), "key: value\n").unwrap();
        fs::write(dotfiles_real.join("extra.yml"), "extra\n").unwrap();

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

        let plan = dotfile_mount_plan(
            &config,
            workspace.path(),
            &variables(workspace.path()),
            workspace.path(),
        )
        .unwrap();
        let skeleton_path = workspace
            .path()
            .join(DOTFILE_MOUNT_SKELETON_DIR)
            .join(".config/lazygit");

        assert_eq!(plan.mounts.len(), 2);
        assert_eq!(plan.skeletons.len(), 1);
        assert_eq!(plan.skeletons[0].root, skeleton_path);
        assert!(!skeleton_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn uses_skeleton_and_backing_directory_mount_when_backing_root_has_extra_entries() {
        let workspace = tempfile::tempdir().unwrap();
        let dotfiles_real = workspace.path().join("dotfiles-real");
        fs::create_dir_all(&dotfiles_real).unwrap();
        fs::write(dotfiles_real.join("config.yml"), "key: value\n").unwrap();
        fs::write(dotfiles_real.join("extra.yml"), "extra\n").unwrap();

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

        let mounts = materialized_dotfile_mount_specs(
            &config,
            workspace.path(),
            &variables(workspace.path()),
            workspace.path(),
        )
        .unwrap();

        let skeleton_path = workspace
            .path()
            .join(DOTFILE_MOUNT_SKELETON_DIR)
            .join(".config/lazygit");
        assert_eq!(mounts.len(), 2);
        assert_eq!(
            mounts[0].source.as_deref(),
            Some(skeleton_path.to_str().unwrap())
        );
        assert_eq!(mounts[0].target, "/opt/decune/dotfiles/.config/lazygit");
        assert!(mounts[0].read_only);
        let backing_target = backing_mount_target_for_source(&mounts, &dotfiles_real);
        assert_eq!(
            mounts[1].source.as_deref(),
            Some(dotfiles_real.canonicalize().unwrap().to_str().unwrap())
        );
        assert_eq!(mounts[1].target, backing_target);
        assert!(mounts[1].read_only);

        let skeleton_file = skeleton_path.join("config.yml");
        assert_skeleton_symlink(&skeleton_file, &format!("{backing_target}/config.yml"));
        assert!(!skeleton_path.join("extra.yml").exists());
    }

    #[cfg(unix)]
    #[test]
    fn uses_backing_directory_mount_for_regular_file_when_skeleton_is_needed() {
        let workspace = tempfile::tempdir().unwrap();
        let dotfiles_real = workspace.path().join("dotfiles-real");
        let source_dir = workspace.path().join("lazygit-source");
        fs::create_dir_all(&dotfiles_real).unwrap();
        fs::create_dir_all(&source_dir).unwrap();
        fs::write(dotfiles_real.join("config.yml"), "key: value\n").unwrap();
        fs::write(source_dir.join("local.yml"), "local: true\n").unwrap();
        unix_fs::symlink(
            dotfiles_real.join("config.yml"),
            source_dir.join("config.yml"),
        )
        .unwrap();

        let config = ResolvedConfig {
            dotfiles: vec![ResolvedDotfile {
                source: "lazygit-source".to_owned(),
                target: ".config/lazygit".to_owned(),
                read_only: true,
                resolve_symlink: true,
                on_conflict: DotfileConflict::Fail,
                origin: ConfigPathOrigin::Project,
            }],
            ..ResolvedConfig::default()
        };

        let mounts = materialized_dotfile_mount_specs(
            &config,
            workspace.path(),
            &variables(workspace.path()),
            workspace.path(),
        )
        .unwrap();

        let skeleton_path = workspace
            .path()
            .join(DOTFILE_MOUNT_SKELETON_DIR)
            .join(".config/lazygit");
        let real_backing_target = backing_mount_target_for_source(&mounts, &dotfiles_real);
        let source_backing_target = backing_mount_target_for_source(&mounts, &source_dir);

        assert_eq!(mounts.len(), 3);
        assert!(
            !mounts
                .iter()
                .any(|mount| mount.target == "/opt/decune/dotfiles/.config/lazygit/config.yml")
        );
        assert_skeleton_symlink(
            &skeleton_path.join("config.yml"),
            &format!("{real_backing_target}/config.yml"),
        );
        assert_skeleton_symlink(
            &skeleton_path.join("local.yml"),
            &format!("{source_backing_target}/local.yml"),
        );
    }

    #[cfg(unix)]
    #[test]
    fn assigns_unique_backing_targets_across_skeleton_dotfile_entries() {
        let workspace = tempfile::tempdir().unwrap();
        let mut dotfiles = Vec::new();
        for tool in ["tool-a", "tool-b"] {
            let backing_dir = workspace.path().join("dotfiles-repo").join(tool);
            let source_dir = workspace.path().join("dotfiles-src").join(tool);
            fs::create_dir_all(&backing_dir).unwrap();
            fs::create_dir_all(&source_dir).unwrap();
            fs::write(
                backing_dir.join(format!("{tool}-config.yml")),
                format!("{tool}-config\n"),
            )
            .unwrap();
            fs::write(
                source_dir.join(format!("{tool}-local.yml")),
                format!("{tool}-local\n"),
            )
            .unwrap();
            unix_fs::symlink(
                backing_dir.join(format!("{tool}-config.yml")),
                source_dir.join(format!("{tool}-config.yml")),
            )
            .unwrap();
            dotfiles.push(ResolvedDotfile {
                source: format!("dotfiles-src/{tool}"),
                target: format!(".config/{tool}"),
                read_only: true,
                resolve_symlink: true,
                on_conflict: DotfileConflict::Fail,
                origin: ConfigPathOrigin::Project,
            });
        }
        let config = ResolvedConfig {
            dotfiles,
            ..ResolvedConfig::default()
        };

        let plan = dotfile_mount_plan(
            &config,
            workspace.path(),
            &variables(workspace.path()),
            workspace.path(),
        )
        .unwrap();
        materialize_dotfile_skeletons(&plan.skeletons).unwrap();

        let backing_targets = plan
            .mounts
            .iter()
            .filter(|mount| mount.target.starts_with(DOTFILE_BACKINGS_MOUNT_ROOT))
            .map(|mount| mount.target.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(backing_targets.len(), 4);
        assert_eq!(
            plan.mounts
                .iter()
                .filter(|mount| mount.target.starts_with(DOTFILE_BACKINGS_MOUNT_ROOT))
                .count(),
            backing_targets.len()
        );

        for tool in ["tool-a", "tool-b"] {
            let backing_dir = workspace.path().join("dotfiles-repo").join(tool);
            let source_dir = workspace.path().join("dotfiles-src").join(tool);
            let backing_target = backing_mount_target_for_source(&plan.mounts, &backing_dir);
            let source_target = backing_mount_target_for_source(&plan.mounts, &source_dir);
            let skeleton = workspace
                .path()
                .join(DOTFILE_MOUNT_SKELETON_DIR)
                .join(".config")
                .join(tool);
            assert_skeleton_symlink(
                &skeleton.join(format!("{tool}-config.yml")),
                &format!("{backing_target}/{tool}-config.yml"),
            );
            assert_skeleton_symlink(
                &skeleton.join(format!("{tool}-local.yml")),
                &format!("{source_target}/{tool}-local.yml"),
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn shares_backing_mounts_across_read_only_skeleton_dotfile_entries() {
        let workspace = tempfile::tempdir().unwrap();
        let backing_dir = workspace.path().join("dotfiles-repo/tool");
        let source_dir = workspace.path().join("dotfiles-src/tool");
        fs::create_dir_all(&backing_dir).unwrap();
        fs::create_dir_all(&source_dir).unwrap();
        fs::write(backing_dir.join("config.yml"), "config\n").unwrap();
        fs::write(source_dir.join("local.yml"), "local\n").unwrap();
        unix_fs::symlink(
            backing_dir.join("config.yml"),
            source_dir.join("config.yml"),
        )
        .unwrap();
        let config = ResolvedConfig {
            dotfiles: [".config/tool-a", ".config/tool-b"]
                .into_iter()
                .map(|target| ResolvedDotfile {
                    source: "dotfiles-src/tool".to_owned(),
                    target: target.to_owned(),
                    read_only: true,
                    resolve_symlink: true,
                    on_conflict: DotfileConflict::Fail,
                    origin: ConfigPathOrigin::Project,
                })
                .collect(),
            ..ResolvedConfig::default()
        };

        let plan = dotfile_mount_plan(
            &config,
            workspace.path(),
            &variables(workspace.path()),
            workspace.path(),
        )
        .unwrap();
        materialize_dotfile_skeletons(&plan.skeletons).unwrap();

        assert_eq!(
            plan.mounts
                .iter()
                .filter(|mount| mount.target.starts_with(DOTFILE_BACKINGS_MOUNT_ROOT))
                .count(),
            2
        );
        let backing_target = backing_mount_target_for_source(&plan.mounts, &backing_dir);
        let source_target = backing_mount_target_for_source(&plan.mounts, &source_dir);
        for tool in ["tool-a", "tool-b"] {
            let skeleton = workspace
                .path()
                .join(DOTFILE_MOUNT_SKELETON_DIR)
                .join(".config")
                .join(tool);
            assert_skeleton_symlink(
                &skeleton.join("config.yml"),
                &format!("{backing_target}/config.yml"),
            );
            assert_skeleton_symlink(
                &skeleton.join("local.yml"),
                &format!("{source_target}/local.yml"),
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn separates_backing_mounts_for_different_access_modes() {
        let workspace = tempfile::tempdir().unwrap();
        let backing_dir = workspace.path().join("dotfiles-repo/tool");
        let source_dir = workspace.path().join("dotfiles-src/tool");
        fs::create_dir_all(&backing_dir).unwrap();
        fs::create_dir_all(&source_dir).unwrap();
        fs::write(backing_dir.join("config.yml"), "config\n").unwrap();
        fs::write(source_dir.join("local.yml"), "local\n").unwrap();
        unix_fs::symlink(
            backing_dir.join("config.yml"),
            source_dir.join("config.yml"),
        )
        .unwrap();
        let config = ResolvedConfig {
            dotfiles: vec![
                ResolvedDotfile {
                    source: "dotfiles-src/tool".to_owned(),
                    target: ".config/read-only".to_owned(),
                    read_only: true,
                    resolve_symlink: true,
                    on_conflict: DotfileConflict::Fail,
                    origin: ConfigPathOrigin::Project,
                },
                ResolvedDotfile {
                    source: "dotfiles-src/tool".to_owned(),
                    target: ".config/writable".to_owned(),
                    read_only: false,
                    resolve_symlink: true,
                    on_conflict: DotfileConflict::Fail,
                    origin: ConfigPathOrigin::Project,
                },
            ],
            ..ResolvedConfig::default()
        };

        let plan = dotfile_mount_plan(
            &config,
            workspace.path(),
            &variables(workspace.path()),
            workspace.path(),
        )
        .unwrap();
        materialize_dotfile_skeletons(&plan.skeletons).unwrap();

        assert_eq!(
            plan.mounts
                .iter()
                .filter(|mount| mount.target.starts_with(DOTFILE_BACKINGS_MOUNT_ROOT))
                .count(),
            4
        );
        for source in [&backing_dir, &source_dir] {
            let read_only_target =
                backing_mount_target_for_source_and_mode(&plan.mounts, source, true);
            let writable_target =
                backing_mount_target_for_source_and_mode(&plan.mounts, source, false);
            assert_ne!(read_only_target, writable_target);
        }

        let read_only_backing =
            backing_mount_target_for_source_and_mode(&plan.mounts, &backing_dir, true);
        let writable_backing =
            backing_mount_target_for_source_and_mode(&plan.mounts, &backing_dir, false);
        assert_skeleton_symlink(
            &workspace
                .path()
                .join(DOTFILE_MOUNT_SKELETON_DIR)
                .join(".config/read-only/config.yml"),
            &format!("{read_only_backing}/config.yml"),
        );
        assert_skeleton_symlink(
            &workspace
                .path()
                .join(DOTFILE_MOUNT_SKELETON_DIR)
                .join(".config/writable/config.yml"),
            &format!("{writable_backing}/config.yml"),
        );
    }

    #[cfg(unix)]
    #[test]
    fn uses_skeleton_when_backing_root_directory_resolves_elsewhere() {
        let workspace = tempfile::tempdir().unwrap();
        let dotfiles_real = workspace.path().join("dotfiles-real");
        fs::create_dir_all(dotfiles_real.join("plugins")).unwrap();
        fs::write(dotfiles_real.join("config.yml"), "key: value\n").unwrap();

        let source_dir = workspace.path().join(".config/lazygit");
        fs::create_dir_all(source_dir.join("plugins")).unwrap();
        unix_fs::symlink(
            dotfiles_real.join("config.yml"),
            source_dir.join("config.yml"),
        )
        .unwrap();

        let config = ResolvedConfig {
            dotfiles: vec![ResolvedDotfile {
                source: ".config/lazygit".to_owned(),
                target: ".config/lazygit".to_owned(),
                read_only: false,
                resolve_symlink: true,
                on_conflict: DotfileConflict::Fail,
                origin: ConfigPathOrigin::Project,
            }],
            ..ResolvedConfig::default()
        };

        let mounts = materialized_dotfile_mount_specs(
            &config,
            workspace.path(),
            &variables(workspace.path()),
            workspace.path(),
        )
        .unwrap();

        let skeleton_path = workspace
            .path()
            .join(DOTFILE_MOUNT_SKELETON_DIR)
            .join(".config/lazygit");
        assert_eq!(mounts.len(), 3);
        assert_eq!(
            mounts[0].source.as_deref(),
            Some(skeleton_path.to_str().unwrap())
        );
        assert_eq!(mounts[0].target, "/opt/decune/dotfiles/.config/lazygit");
        assert!(!mounts[0].read_only);
        let backing_target = backing_mount_target_for_source(&mounts, &dotfiles_real);
        assert_eq!(
            mounts[1].source.as_deref(),
            Some(dotfiles_real.canonicalize().unwrap().to_str().unwrap())
        );
        assert_eq!(mounts[1].target, backing_target);
        assert_eq!(
            mounts[2].source.as_deref(),
            Some(source_dir.join("plugins").to_str().unwrap())
        );
        assert_eq!(
            mounts[2].target,
            "/opt/decune/dotfiles/.config/lazygit/plugins"
        );
        assert!(!mounts.iter().any(|mount| {
            mount.source.as_deref() == dotfiles_real.to_str()
                && mount.target == "/opt/decune/dotfiles/.config/lazygit"
        }));
        assert_skeleton_symlink(
            &skeleton_path.join("config.yml"),
            &format!("{backing_target}/config.yml"),
        );
    }

    #[cfg(unix)]
    #[test]
    fn uses_writable_skeleton_mount_when_dotfile_is_not_read_only() {
        let workspace = tempfile::tempdir().unwrap();
        let dotfiles_real = workspace.path().join("dotfiles-real");
        fs::create_dir_all(&dotfiles_real).unwrap();
        fs::write(dotfiles_real.join("config.yml"), "key: value\n").unwrap();
        fs::write(dotfiles_real.join("extra.yml"), "extra\n").unwrap();

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
                read_only: false,
                resolve_symlink: true,
                on_conflict: DotfileConflict::Fail,
                origin: ConfigPathOrigin::Project,
            }],
            ..ResolvedConfig::default()
        };

        let mounts = materialized_dotfile_mount_specs(
            &config,
            workspace.path(),
            &variables(workspace.path()),
            workspace.path(),
        )
        .unwrap();

        assert_eq!(mounts.len(), 2);
        assert_eq!(mounts[0].target, "/opt/decune/dotfiles/.config/lazygit");
        assert!(!mounts[0].read_only);
        let backing_target = backing_mount_target_for_source(&mounts, &dotfiles_real);
        assert_eq!(mounts[1].target, backing_target);
        assert!(!mounts[1].read_only);
    }

    #[cfg(unix)]
    #[test]
    fn uses_skeleton_and_direct_directory_mount_for_symlinked_directory() {
        let workspace = tempfile::tempdir().unwrap();
        let real_subdir = workspace.path().join("real-plugins");
        fs::create_dir_all(&real_subdir).unwrap();
        fs::write(real_subdir.join("plugin.lua"), "return {}").unwrap();

        let source_dir = workspace.path().join("nvim");
        fs::create_dir_all(&source_dir).unwrap();
        unix_fs::symlink(&real_subdir, source_dir.join("plugins")).unwrap();

        let config = ResolvedConfig {
            dotfiles: vec![ResolvedDotfile {
                source: "nvim".to_owned(),
                target: ".config/nvim".to_owned(),
                read_only: true,
                resolve_symlink: true,
                on_conflict: DotfileConflict::Fail,
                origin: ConfigPathOrigin::Project,
            }],
            ..ResolvedConfig::default()
        };

        let mounts = materialized_dotfile_mount_specs(
            &config,
            workspace.path(),
            &variables(workspace.path()),
            workspace.path(),
        )
        .unwrap();

        let skeleton_path = workspace
            .path()
            .join(DOTFILE_MOUNT_SKELETON_DIR)
            .join(".config/nvim");
        assert_eq!(mounts.len(), 2);
        assert_eq!(
            mounts[0].source.as_deref(),
            Some(skeleton_path.to_str().unwrap())
        );
        assert_eq!(
            mounts[1].source.as_deref(),
            Some(real_subdir.to_str().unwrap())
        );
        assert_eq!(
            mounts[1].target,
            "/opt/decune/dotfiles/.config/nvim/plugins"
        );
        assert!(skeleton_path.join("plugins").is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_dotfile_directory_that_generates_too_many_mounts() {
        let workspace = tempfile::tempdir().unwrap();
        let real_dir = workspace.path().join("real");
        let source_dir = workspace.path().join("dotdir");
        fs::create_dir_all(&real_dir).unwrap();
        fs::create_dir_all(&source_dir).unwrap();
        for index in 0..MAX_DOTFILE_MOUNTS {
            let name = format!("file-{index}.txt");
            let real_parent = real_dir.join(format!("parent-{index}"));
            fs::create_dir_all(&real_parent).unwrap();
            fs::write(real_parent.join("config.txt"), "content").unwrap();
            unix_fs::symlink(real_parent.join("config.txt"), source_dir.join(&name)).unwrap();
        }
        let config = ResolvedConfig {
            dotfiles: vec![ResolvedDotfile {
                source: "dotdir".to_owned(),
                target: ".config/app".to_owned(),
                read_only: true,
                resolve_symlink: true,
                on_conflict: DotfileConflict::Fail,
                origin: ConfigPathOrigin::Project,
            }],
            ..ResolvedConfig::default()
        };

        let error = dotfile_mount_plan(
            &config,
            workspace.path(),
            &variables(workspace.path()),
            workspace.path(),
        )
        .unwrap_err();
        let message = format!("{error:#}");

        assert!(
            message.contains("too many bind mounts"),
            "unexpected error: {message}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn materialize_preserves_matching_symlink_and_replaces_conflicting_desired_path() {
        let workspace = tempfile::tempdir().unwrap();
        let dotfiles_real = workspace.path().join("real");
        fs::create_dir_all(&dotfiles_real).unwrap();
        fs::write(dotfiles_real.join("config.yml"), "content").unwrap();
        fs::write(dotfiles_real.join("extra.yml"), "extra").unwrap();

        let source_dir = workspace.path().join("dotdir");
        fs::create_dir_all(&source_dir).unwrap();
        unix_fs::symlink(
            dotfiles_real.join("config.yml"),
            source_dir.join("config.yml"),
        )
        .unwrap();

        let config = ResolvedConfig {
            dotfiles: vec![ResolvedDotfile {
                source: "dotdir".to_owned(),
                target: ".config/app".to_owned(),
                read_only: true,
                resolve_symlink: true,
                on_conflict: DotfileConflict::Fail,
                origin: ConfigPathOrigin::Project,
            }],
            ..ResolvedConfig::default()
        };
        let skeleton_path = workspace
            .path()
            .join(DOTFILE_MOUNT_SKELETON_DIR)
            .join(".config/app");

        let plan = dotfile_mount_plan(
            &config,
            workspace.path(),
            &variables(workspace.path()),
            workspace.path(),
        )
        .unwrap();
        materialize_dotfile_skeletons(&plan.skeletons).unwrap();
        let file_path = skeleton_path.join("config.yml");
        let backing_target = backing_mount_target_for_source(&plan.mounts, &dotfiles_real);
        let expected_target = format!("{backing_target}/config.yml");
        let root_ino = skeleton_path.symlink_metadata().unwrap().ino();
        let file_ino = file_path.symlink_metadata().unwrap().ino();
        assert_skeleton_symlink(&file_path, &expected_target);

        materialize_dotfile_skeletons(&plan.skeletons).unwrap();

        assert_eq!(skeleton_path.symlink_metadata().unwrap().ino(), root_ino);
        assert_eq!(file_path.symlink_metadata().unwrap().ino(), file_ino);
        assert_skeleton_symlink(&file_path, &expected_target);

        fs::remove_file(&file_path).unwrap();
        fs::write(&file_path, "placeholder").unwrap();
        fs::write(skeleton_path.join("stale"), "stale").unwrap();

        materialize_dotfile_skeletons(&plan.skeletons).unwrap();

        assert_eq!(skeleton_path.symlink_metadata().unwrap().ino(), root_ino);
        assert_skeleton_symlink(&file_path, &expected_target);
        assert!(!skeleton_path.join("stale").exists());
    }

    #[cfg(unix)]
    #[test]
    fn writable_skeleton_materialization_preserves_unknown_entries() {
        let workspace = tempfile::tempdir().unwrap();
        let dotfiles_real = workspace.path().join("real");
        fs::create_dir_all(&dotfiles_real).unwrap();
        fs::write(dotfiles_real.join("config.yml"), "content").unwrap();
        fs::write(dotfiles_real.join("extra.yml"), "extra").unwrap();

        let source_dir = workspace.path().join("dotdir");
        fs::create_dir_all(&source_dir).unwrap();
        unix_fs::symlink(
            dotfiles_real.join("config.yml"),
            source_dir.join("config.yml"),
        )
        .unwrap();

        let config = ResolvedConfig {
            dotfiles: vec![ResolvedDotfile {
                source: "dotdir".to_owned(),
                target: ".config/app".to_owned(),
                read_only: false,
                resolve_symlink: true,
                on_conflict: DotfileConflict::Fail,
                origin: ConfigPathOrigin::Project,
            }],
            ..ResolvedConfig::default()
        };
        let skeleton_path = workspace
            .path()
            .join(DOTFILE_MOUNT_SKELETON_DIR)
            .join(".config/app");

        let plan = dotfile_mount_plan(
            &config,
            workspace.path(),
            &variables(workspace.path()),
            workspace.path(),
        )
        .unwrap();
        materialize_dotfile_skeletons(&plan.skeletons).unwrap();
        let backing_target = backing_mount_target_for_source(&plan.mounts, &dotfiles_real);
        let expected_target = format!("{backing_target}/config.yml");
        fs::remove_file(skeleton_path.join("config.yml")).unwrap();
        fs::write(skeleton_path.join("config.yml"), "local replacement").unwrap();
        fs::write(skeleton_path.join("new.json"), "new").unwrap();

        materialize_dotfile_skeletons(&plan.skeletons).unwrap();

        assert_skeleton_symlink(&skeleton_path.join("config.yml"), &expected_target);
        assert_eq!(
            fs::read_to_string(skeleton_path.join("new.json")).unwrap(),
            "new"
        );
    }

    #[cfg(unix)]
    #[test]
    fn skeleton_is_idempotent_without_copying_contents() {
        let workspace = tempfile::tempdir().unwrap();
        let dotfiles_real = workspace.path().join("real");
        fs::create_dir_all(&dotfiles_real).unwrap();
        fs::write(dotfiles_real.join("config.yml"), "v1").unwrap();
        fs::write(dotfiles_real.join("extra.yml"), "extra").unwrap();

        let source_dir = workspace.path().join("dotdir");
        fs::create_dir_all(&source_dir).unwrap();
        unix_fs::symlink(
            dotfiles_real.join("config.yml"),
            source_dir.join("config.yml"),
        )
        .unwrap();

        let config = ResolvedConfig {
            dotfiles: vec![ResolvedDotfile {
                source: "dotdir".to_owned(),
                target: ".config/app".to_owned(),
                read_only: true,
                resolve_symlink: true,
                on_conflict: DotfileConflict::Fail,
                origin: ConfigPathOrigin::Project,
            }],
            ..ResolvedConfig::default()
        };

        let mounts1 = materialized_dotfile_mount_specs(
            &config,
            workspace.path(),
            &variables(workspace.path()),
            workspace.path(),
        )
        .unwrap();

        let skeleton_path = workspace
            .path()
            .join(DOTFILE_MOUNT_SKELETON_DIR)
            .join(".config/app");
        let backing_target = backing_mount_target_for_source(&mounts1, &dotfiles_real);
        let expected_target = format!("{backing_target}/config.yml");
        fs::write(skeleton_path.join("stale"), "stale").unwrap();
        fs::write(dotfiles_real.join("config.yml"), "v2").unwrap();

        let mounts2 = materialized_dotfile_mount_specs(
            &config,
            workspace.path(),
            &variables(workspace.path()),
            workspace.path(),
        )
        .unwrap();

        assert_eq!(mounts1[0].source, mounts2[0].source);
        assert_skeleton_symlink(&skeleton_path.join("config.yml"), &expected_target);
        assert!(!skeleton_path.join("stale").exists());
        assert!(!skeleton_path.join("extra.yml").exists());
    }
}
