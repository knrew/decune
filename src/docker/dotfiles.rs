use std::{
    collections::BTreeMap,
    fs,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail};

use crate::{
    config::{
        path::{HostPathOptions, PathCreate, SymlinkResolution, resolve_host_path},
        resolved::{ResolvedConfig, ResolvedDotfile, ResolvedDotfileEntry},
        types::{DotfileConflict, MountType},
        variables::{VariableContext, expand_variables},
    },
    docker::{
        client::DockerClient,
        exec::{ExecCommandSpec, exec_capture},
        mounts::DockerMountSpec,
        user::ResolvedRemoteUser,
    },
};

const DOTFILES_MOUNT_ROOT: &str = "/opt/decune/dotfiles";
const DOTFILE_BACKINGS_MOUNT_ROOT: &str = "/opt/decune/dotfile-backings";
const DOTFILE_MOUNT_SKELETON_DIR: &str = "dotfile-mount-skeleton";
const MAX_DOTFILE_TREE_DEPTH: u32 = 32;
const MAX_DOTFILE_MOUNTS: usize = 1024;

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct DotfileMountPlan {
    pub(crate) mounts: Vec<DockerMountSpec>,
    pub(crate) skeletons: Vec<DotfileSkeletonPlan>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DotfileSkeletonPlan {
    pub(crate) root: PathBuf,
    pub(crate) read_only: bool,
    entries: BTreeMap<PathBuf, DotfileSkeletonEntryKind>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DotfileSkeletonEntryKind {
    Symlink { target: String },
    Directory,
}

#[cfg(test)]
pub(crate) fn dotfile_mount_specs(
    config: &ResolvedConfig,
    workspace_root: &Path,
    variables: &VariableContext,
    state_root: &Path,
) -> Result<Vec<DockerMountSpec>> {
    Ok(dotfile_mount_plan(config, workspace_root, variables, state_root)?.mounts)
}

pub(crate) fn dotfile_mount_plan(
    config: &ResolvedConfig,
    workspace_root: &Path,
    variables: &VariableContext,
    state_root: &Path,
) -> Result<DotfileMountPlan> {
    let mut plan = DotfileMountPlan::default();
    for dotfile in expanded_dotfiles(config, variables)? {
        let dotfile_plan = dotfile_mount_spec(&dotfile, workspace_root, variables, state_root)?;
        plan.mounts.extend(dotfile_plan.mounts);
        plan.skeletons.extend(dotfile_plan.skeletons);
    }

    Ok(plan)
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

pub(crate) async fn setup_dotfiles(
    client: &DockerClient,
    container: &str,
    config: &ResolvedConfig,
    remote_user: &ResolvedRemoteUser,
    variables: &VariableContext,
) -> Result<()> {
    if config.dotfiles.is_empty() {
        return Ok(());
    }

    let remote_home = remote_user.home()?;
    let script = dotfile_setup_script(config, remote_home, variables)?;
    if script.is_empty() {
        return Ok(());
    }

    fix_dotfiles_mount_root_ownership(client, container, remote_user).await;

    exec_capture(
        client,
        container,
        &ExecCommandSpec {
            command: vec!["/bin/sh".to_owned(), "-lc".to_owned(), script],
            user: Some(remote_user.user.clone()),
            working_dir: Some(remote_home.to_owned()),
            env: Default::default(),
            redactions: Vec::new(),
            tty: false,
        },
    )
    .await
    .with_context(|| format!("Failed to setup dotfiles in container: {container}"))?;

    Ok(())
}

async fn fix_dotfiles_mount_root_ownership(
    client: &DockerClient,
    container: &str,
    remote_user: &ResolvedRemoteUser,
) {
    let script = format!(
        "chown {}:{} '{}'",
        remote_user.uid, remote_user.gid, DOTFILES_MOUNT_ROOT,
    );
    let _ = exec_capture(
        client,
        container,
        &ExecCommandSpec {
            command: vec!["/bin/sh".to_owned(), "-c".to_owned(), script],
            user: Some("root".to_owned()),
            working_dir: None,
            env: Default::default(),
            redactions: Vec::new(),
            tty: false,
        },
    )
    .await;
}

fn dotfile_setup_script(
    config: &ResolvedConfig,
    remote_home: &str,
    variables: &VariableContext,
) -> Result<String> {
    let dotfiles = expanded_dotfiles(config, variables)?;
    let mut script = String::new();
    if !dotfiles.is_empty() {
        script.push_str("set -e\n");
    }

    for dotfile in dotfiles {
        let target = remote_home_target(remote_home, &dotfile.target)?;
        let source = dotfile_mount_target(&dotfile.target)?;
        let parent = container_parent(&target)?;
        script.push_str(&dotfile_setup_script_entry(
            &source,
            &target,
            &parent,
            dotfile.dotfile.on_conflict,
        ));
    }

    Ok(script)
}

fn dotfile_mount_spec(
    dotfile: &ExpandedDotfile<'_>,
    workspace_root: &Path,
    variables: &VariableContext,
    state_root: &Path,
) -> Result<DotfileMountPlan> {
    let target = dotfile_mount_target(&dotfile.target)?;
    let source = resolve_host_path(
        &dotfile.dotfile.source,
        &HostPathOptions::new(dotfile.dotfile.origin, workspace_root, variables)
            .with_create(PathCreate::None)
            .with_symlink_resolution(symlink_resolution(dotfile.dotfile.resolve_symlink)),
    )
    .with_context(|| {
        format!(
            "Failed to resolve dotfile source for target: {}",
            dotfile.dotfile.target
        )
    })?;

    if !dotfile.dotfile.resolve_symlink || source.is_file() {
        return Ok(DotfileMountPlan {
            mounts: vec![dotfile_bind_mount(
                &source,
                target,
                dotfile.dotfile.read_only,
            )],
            skeletons: Vec::new(),
        });
    }
    if !source.is_dir() {
        bail!(
            "Dotfile source must be a file or directory: {}",
            source.display()
        );
    }

    if !directory_contains_any_symlink(&source)? {
        return Ok(DotfileMountPlan {
            mounts: vec![dotfile_bind_mount(
                &source,
                target,
                dotfile.dotfile.read_only,
            )],
            skeletons: Vec::new(),
        });
    }

    let tree = collect_dotfile_tree(&source)?;
    if let Some(backing_root) = backing_root_mount_source(&tree)? {
        return Ok(DotfileMountPlan {
            mounts: vec![dotfile_bind_mount(
                &backing_root,
                target,
                dotfile.dotfile.read_only,
            )],
            skeletons: Vec::new(),
        });
    }

    let plan = skeleton_dotfile_mount_plan(
        &source,
        &dotfile.target,
        target,
        state_root,
        dotfile.dotfile.read_only,
    )?;
    if plan.mounts.len() > MAX_DOTFILE_MOUNTS {
        bail!(
            "Dotfile target generates too many bind mounts ({} > {}): {}",
            plan.mounts.len(),
            MAX_DOTFILE_MOUNTS,
            dotfile.target
        );
    }

    Ok(plan)
}

fn dotfile_bind_mount(source: &Path, target: String, read_only: bool) -> DockerMountSpec {
    DockerMountSpec {
        source: Some(source.display().to_string()),
        target,
        mount_type: MountType::Bind,
        read_only,
        consistency: None,
        bind_options: None,
        volume_options: None,
    }
}

struct ExpandedDotfile<'a> {
    dotfile: &'a ResolvedDotfile,
    target: String,
}

enum DotfileEntryRef<'a> {
    Enabled(&'a ResolvedDotfile),
    Disabled(&'a str),
}

fn expanded_dotfiles<'a>(
    config: &'a ResolvedConfig,
    variables: &VariableContext,
) -> Result<Vec<ExpandedDotfile<'a>>> {
    let mut expanded = Vec::new();

    for entry in dotfile_entries(config) {
        match entry {
            DotfileEntryRef::Enabled(dotfile) => {
                let target = normalized_expanded_dotfile_target(&dotfile.target, variables)?;
                replace_dotfile_by_target(&mut expanded, ExpandedDotfile { dotfile, target });
            }
            DotfileEntryRef::Disabled(target) => {
                let target = normalized_expanded_dotfile_target(target, variables)?;
                remove_dotfile_by_target(&mut expanded, &target);
            }
        }
    }

    Ok(expanded)
}

fn dotfile_entries(config: &ResolvedConfig) -> Vec<DotfileEntryRef<'_>> {
    if config.dotfile_entries.is_empty() {
        return config
            .dotfiles
            .iter()
            .map(DotfileEntryRef::Enabled)
            .collect();
    }

    config
        .dotfile_entries
        .iter()
        .map(|entry| match entry {
            ResolvedDotfileEntry::Enabled(dotfile) => DotfileEntryRef::Enabled(dotfile),
            ResolvedDotfileEntry::Disabled(dotfile) => DotfileEntryRef::Disabled(&dotfile.target),
        })
        .collect()
}

fn replace_dotfile_by_target<'a>(
    dotfiles: &mut Vec<ExpandedDotfile<'a>>,
    dotfile: ExpandedDotfile<'a>,
) {
    match dotfiles
        .iter()
        .position(|existing| existing.target == dotfile.target)
    {
        Some(index) => dotfiles[index] = dotfile,
        None => dotfiles.push(dotfile),
    }
}

fn remove_dotfile_by_target(dotfiles: &mut Vec<ExpandedDotfile<'_>>, target: &str) {
    dotfiles.retain(|existing| existing.target != target);
}

fn normalized_expanded_dotfile_target(target: &str, variables: &VariableContext) -> Result<String> {
    let target = expanded_dotfile_target(target, variables)?;
    let components = relative_target_components(&target)?;

    Ok(components.join("/"))
}

fn expanded_dotfile_target(target: &str, variables: &VariableContext) -> Result<String> {
    expand_variables(target, variables)
        .with_context(|| format!("Failed to expand dotfile target: {target}"))
}

fn dotfile_mount_target(target: &str) -> Result<String> {
    let components = relative_target_components(target)?;

    Ok(format!("{DOTFILES_MOUNT_ROOT}/{}", components.join("/")))
}

fn remote_home_target(remote_home: &str, target: &str) -> Result<String> {
    let components = relative_target_components(target)?;
    let remote_home = normalized_remote_home(remote_home)?;
    if remote_home == "/" {
        return Ok(format!("/{}", components.join("/")));
    }

    Ok(format!("{remote_home}/{}", components.join("/")))
}

fn normalized_remote_home(remote_home: &str) -> Result<&str> {
    let trimmed = remote_home.trim_end_matches('/');
    if trimmed.is_empty() && remote_home.starts_with('/') {
        return Ok("/");
    }
    if trimmed.is_empty() {
        bail!("Remote user home must not be empty");
    }

    Ok(trimmed)
}

fn container_parent(target: &str) -> Result<String> {
    let (parent, _) = target
        .rsplit_once('/')
        .ok_or_else(|| anyhow::anyhow!("Dotfile target must be absolute: {target}"))?;
    if parent.is_empty() {
        return Ok("/".to_owned());
    }

    Ok(parent.to_owned())
}

fn dotfile_setup_script_entry(
    source: &str,
    target: &str,
    parent: &str,
    on_conflict: DotfileConflict,
) -> String {
    let source = shell_quote(source);
    let target = shell_quote(target);
    let parent = shell_quote(parent);
    let conflict_body = match on_conflict {
        DotfileConflict::Fail => format!(
            "printf '%s\\n' {message} >&2\nexit 1\n",
            message = shell_quote("Dotfile target already exists")
        ),
        DotfileConflict::ReplaceSymlink => format!(
            "if [ -L \"$dest\" ]; then\n  rm \"$dest\"\nelse\n  printf '%s\\n' {message} >&2\n  exit 1\nfi\n",
            message = shell_quote("Dotfile target already exists and is not a symlink")
        ),
        DotfileConflict::Backup => {
            "backup=\"$dest.decune-backup-$(date +%s)\"\nindex=0\nwhile [ -e \"$backup\" ] || [ -L \"$backup\" ]; do\n  index=$((index + 1))\n  backup=\"$dest.decune-backup-$(date +%s)-$index\"\ndone\nmv \"$dest\" \"$backup\"\n".to_owned()
        }
    };

    format!(
        "src={source}\ndest={target}\nparent={parent}\nmkdir -p \"$parent\"\nif [ -L \"$dest\" ] && [ \"$(readlink \"$dest\")\" = \"$src\" ]; then\n  :\nelif [ -e \"$dest\" ] || [ -L \"$dest\" ]; then\n{conflict_body}  ln -s \"$src\" \"$dest\"\nelse\n  ln -s \"$src\" \"$dest\"\nfi\n"
    )
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn relative_target_components(target: &str) -> Result<Vec<String>> {
    let path = Path::new(target);
    let mut components = Vec::new();

    for component in path.components() {
        match component {
            Component::Normal(value) => {
                let value = value.to_str().ok_or_else(|| {
                    anyhow::anyhow!("Dotfile target is not valid Unicode: {target}")
                })?;
                components.push(value.to_owned());
            }
            Component::CurDir => {}
            Component::ParentDir => bail!("Dotfile target must not contain '..': {target}"),
            Component::RootDir | Component::Prefix(_) => {
                bail!("Dotfile target must be relative: {target}");
            }
        }
    }

    if components.is_empty() {
        bail!("Dotfile target must not be empty");
    }

    Ok(components)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DotfileTreeEntryKind {
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
struct DotfileTree {
    entries: BTreeMap<PathBuf, DotfileTreeEntry>,
    has_symlink: bool,
}

#[derive(Debug)]
struct DotfileDirectoryEntry {
    name: String,
    path: PathBuf,
}

impl DotfileTree {
    fn insert(
        &mut self,
        relative: PathBuf,
        kind: DotfileTreeEntryKind,
        real_path: PathBuf,
        from_symlink: bool,
    ) -> Result<()> {
        if self
            .entries
            .insert(
                relative.clone(),
                DotfileTreeEntry {
                    kind,
                    real_path,
                    from_symlink,
                },
            )
            .is_some()
        {
            bail!(
                "Dotfile source resolves the same target more than once: {}",
                relative.display()
            );
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

fn collect_dotfile_tree(source: &Path) -> Result<DotfileTree> {
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

fn read_dotfile_directory_entries(source: &Path) -> Result<Vec<DotfileDirectoryEntry>> {
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

fn directory_contains_any_symlink(source: &Path) -> Result<bool> {
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
            } else if !file_type.is_file() {
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
    } else if file_type.is_file() {
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

fn reject_circular_dotfile_directory(
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

fn backing_root_mount_source(tree: &DotfileTree) -> Result<Option<PathBuf>> {
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
    if file_type.is_file() {
        Some(DotfileTreeEntryKind::File)
    } else if file_type.is_dir() {
        Some(DotfileTreeEntryKind::Directory)
    } else {
        None
    }
}

fn skeleton_dotfile_mount_plan(
    source: &Path,
    dotfile_target: &str,
    container_target: String,
    state_root: &Path,
    read_only: bool,
) -> Result<DotfileMountPlan> {
    let components = relative_target_components(dotfile_target)?;
    let skeleton_root = state_root
        .join(DOTFILE_MOUNT_SKELETON_DIR)
        .join(components.join("/"));
    let mounts = vec![dotfile_bind_mount(
        &skeleton_root,
        container_target.clone(),
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
        container_root: &container_target,
        read_only,
        mounts,
        skeleton_entries: BTreeMap::new(),
        backing_files: BTreeMap::new(),
        backing_sources: BTreeMap::new(),
    };
    builder.build_directory(&source, Path::new(""), &mut ancestors, 0)?;
    builder.finalize_backing_mounts()?;
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
    backing_sources: BTreeMap<PathBuf, ()>,
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
        } else if file_type.is_file() {
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
        let existing = self.skeleton_entries.insert(relative.clone(), kind.clone());
        if let Some(existing) = existing
            && existing != kind
        {
            bail!(
                "Dotfile skeleton path generated with conflicting entry kinds: {}",
                relative.display()
            );
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
        self.backing_sources.insert(backing_file.source.clone(), ());
        self.backing_files.insert(relative, backing_file);

        Ok(())
    }

    fn finalize_backing_mounts(&mut self) -> Result<()> {
        let mut backing_targets = BTreeMap::new();
        for source in self.backing_sources.keys() {
            let target = format!("{DOTFILE_BACKINGS_MOUNT_ROOT}/{}", backing_targets.len());
            backing_targets.insert(source.clone(), target.clone());
            self.mounts
                .push(dotfile_bind_mount(source, target, self.read_only));
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
        } else if !file_type.is_file() {
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

fn symlink_resolution(resolve_symlink: bool) -> SymlinkResolution {
    if resolve_symlink {
        SymlinkResolution::Resolve
    } else {
        SymlinkResolution::Preserve
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path, process::Command};

    #[cfg(unix)]
    use std::os::unix::{fs as unix_fs, fs::MetadataExt};

    use super::*;
    use crate::config::{
        path::ConfigPathOrigin,
        resolved::{ResolvedConfig, ResolvedDotfile, ResolvedDotfileDisable, ResolvedDotfileEntry},
        types::{DotfileConflict, MountType},
        variables::{VariableContext, VariableContextInput},
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

    #[cfg(unix)]
    fn assert_skeleton_symlink(path: &Path, target: &str) {
        assert!(path.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(fs::read_link(path).unwrap(), Path::new(target));
    }

    #[test]
    fn converts_directory_dotfile_to_read_only_direct_mount() {
        let workspace = tempfile::tempdir().unwrap();
        let source = workspace.path().join(".decune/nvim");
        fs::create_dir_all(&source).unwrap();
        let config = ResolvedConfig {
            dotfiles: vec![ResolvedDotfile {
                source: ".decune/nvim".to_owned(),
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

        assert_eq!(
            mounts,
            vec![DockerMountSpec {
                source: Some(source.canonicalize().unwrap().display().to_string()),
                target: "/opt/decune/dotfiles/.config/nvim".to_owned(),
                mount_type: MountType::Bind,
                read_only: true,
                consistency: None,
                bind_options: None,
                volume_options: None,
            }]
        );
        assert!(!workspace.path().join(DOTFILE_MOUNT_SKELETON_DIR).exists());
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

        let mounts = materialized_dotfile_mount_specs(
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
    fn preserves_dotfile_symlink_source_when_requested() {
        let workspace = tempfile::tempdir().unwrap();
        let real_source = workspace.path().join("real-gitconfig");
        let link_source = workspace.path().join("linked-gitconfig");
        fs::write(&real_source, "[user]\nname = decune\n").unwrap();
        unix_fs::symlink(&real_source, &link_source).unwrap();
        let config = ResolvedConfig {
            dotfiles: vec![ResolvedDotfile {
                source: "linked-gitconfig".to_owned(),
                target: ".gitconfig".to_owned(),
                read_only: true,
                resolve_symlink: false,
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

        assert_eq!(
            mounts[0].source.as_deref(),
            Some(link_source.to_str().unwrap())
        );
    }

    #[test]
    fn rejects_absolute_dotfile_target() {
        let workspace = tempfile::tempdir().unwrap();
        let source = workspace.path().join("dotfile");
        fs::write(&source, "dotfile").unwrap();
        let config = ResolvedConfig {
            dotfiles: vec![ResolvedDotfile {
                source: "dotfile".to_owned(),
                target: "/root/.config".to_owned(),
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

        assert!(
            error
                .to_string()
                .contains("Dotfile target must be relative")
        );
    }

    #[test]
    fn expands_dotfile_target_variables_for_dotfile_mount() {
        let workspace = tempfile::tempdir().unwrap();
        let source = workspace.path().join("dotfile");
        fs::write(&source, "dotfile").unwrap();
        let config = ResolvedConfig {
            dotfiles: vec![ResolvedDotfile {
                source: "dotfile".to_owned(),
                target: ".config/${remoteUser}/nvim".to_owned(),
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

        assert_eq!(mounts[0].target, "/opt/decune/dotfiles/.config/vscode/nvim");
    }

    #[test]
    fn dotfile_mount_specs_replaces_duplicate_expanded_target() {
        let workspace = tempfile::tempdir().unwrap();
        let global_source = workspace.path().join("global-gitconfig");
        let project_source = workspace.path().join("project-gitconfig");
        fs::write(&global_source, "global").unwrap();
        fs::write(&project_source, "project").unwrap();
        let config = ResolvedConfig {
            dotfiles: vec![
                ResolvedDotfile {
                    source: global_source.display().to_string(),
                    target: ".config/${remoteUser}/gitconfig".to_owned(),
                    read_only: true,
                    resolve_symlink: true,
                    on_conflict: DotfileConflict::Fail,
                    origin: ConfigPathOrigin::Global,
                },
                ResolvedDotfile {
                    source: project_source.display().to_string(),
                    target: ".config/vscode/gitconfig".to_owned(),
                    read_only: false,
                    resolve_symlink: true,
                    on_conflict: DotfileConflict::Backup,
                    origin: ConfigPathOrigin::Project,
                },
            ],
            ..ResolvedConfig::default()
        };

        let mounts = materialized_dotfile_mount_specs(
            &config,
            workspace.path(),
            &variables(workspace.path()),
            workspace.path(),
        )
        .unwrap();

        assert_eq!(mounts.len(), 1);
        assert_eq!(
            mounts[0].source.as_deref(),
            Some(project_source.canonicalize().unwrap().to_str().unwrap())
        );
        assert_eq!(
            mounts[0].target,
            "/opt/decune/dotfiles/.config/vscode/gitconfig"
        );
        assert!(!mounts[0].read_only);
    }

    #[test]
    fn dotfile_mount_specs_disables_global_dotfile_by_expanded_target() {
        let workspace = tempfile::tempdir().unwrap();
        let global_source = workspace.path().join("global-gitconfig");
        fs::write(&global_source, "global").unwrap();
        let global_dotfile = ResolvedDotfile {
            source: global_source.display().to_string(),
            target: ".config/${remoteUser}/gitconfig".to_owned(),
            read_only: true,
            resolve_symlink: true,
            on_conflict: DotfileConflict::Fail,
            origin: ConfigPathOrigin::Global,
        };
        let config = ResolvedConfig {
            dotfile_entries: vec![
                ResolvedDotfileEntry::Enabled(global_dotfile.clone()),
                ResolvedDotfileEntry::Disabled(ResolvedDotfileDisable {
                    target: ".config/vscode/gitconfig".to_owned(),
                    origin: ConfigPathOrigin::Project,
                }),
            ],
            dotfiles: vec![global_dotfile],
            ..ResolvedConfig::default()
        };

        let mounts = materialized_dotfile_mount_specs(
            &config,
            workspace.path(),
            &variables(workspace.path()),
            workspace.path(),
        )
        .unwrap();

        assert!(mounts.is_empty());
    }

    #[test]
    fn rejects_dotfile_target_variables_that_expand_to_absolute_path() {
        let workspace = tempfile::tempdir().unwrap();
        let source = workspace.path().join("dotfile");
        fs::write(&source, "dotfile").unwrap();
        let config = ResolvedConfig {
            dotfiles: vec![ResolvedDotfile {
                source: "dotfile".to_owned(),
                target: "${remoteUserHome}/.gitconfig".to_owned(),
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

        assert!(
            error
                .to_string()
                .contains("Dotfile target must be relative")
        );
    }

    #[test]
    fn expands_dotfile_target_variables_for_setup_script() {
        let config = ResolvedConfig {
            dotfiles: vec![ResolvedDotfile {
                source: ".decune/nvim".to_owned(),
                target: ".config/${remoteUser}/nvim".to_owned(),
                read_only: true,
                resolve_symlink: true,
                on_conflict: DotfileConflict::Fail,
                origin: ConfigPathOrigin::Project,
            }],
            ..ResolvedConfig::default()
        };

        let script =
            dotfile_setup_script(&config, "/home/vscode", &variables(Path::new("/workspace")))
                .unwrap();

        assert!(script.contains("/opt/decune/dotfiles/.config/vscode/nvim"));
        assert!(script.contains("/home/vscode/.config/vscode/nvim"));
    }

    #[test]
    fn dotfile_setup_script_replaces_duplicate_expanded_target() {
        let config = ResolvedConfig {
            dotfiles: vec![
                ResolvedDotfile {
                    source: ".decune/global-gitconfig".to_owned(),
                    target: ".config/${remoteUser}/gitconfig".to_owned(),
                    read_only: true,
                    resolve_symlink: true,
                    on_conflict: DotfileConflict::Fail,
                    origin: ConfigPathOrigin::Global,
                },
                ResolvedDotfile {
                    source: ".decune/project-gitconfig".to_owned(),
                    target: ".config/vscode/gitconfig".to_owned(),
                    read_only: false,
                    resolve_symlink: true,
                    on_conflict: DotfileConflict::Backup,
                    origin: ConfigPathOrigin::Project,
                },
            ],
            ..ResolvedConfig::default()
        };

        let script =
            dotfile_setup_script(&config, "/home/vscode", &variables(Path::new("/workspace")))
                .unwrap();

        assert_eq!(
            script
                .matches("dest='/home/vscode/.config/vscode/gitconfig'")
                .count(),
            1
        );
        assert!(script.contains(".decune-backup-$(date +%s)"));
        assert!(!script.contains("Dotfile target already exists\n"));
    }

    #[test]
    fn dotfile_setup_script_disables_global_dotfile_by_expanded_target() {
        let global_dotfile = ResolvedDotfile {
            source: ".decune/global-gitconfig".to_owned(),
            target: ".config/${remoteUser}/gitconfig".to_owned(),
            read_only: true,
            resolve_symlink: true,
            on_conflict: DotfileConflict::Fail,
            origin: ConfigPathOrigin::Global,
        };
        let config = ResolvedConfig {
            dotfile_entries: vec![
                ResolvedDotfileEntry::Enabled(global_dotfile.clone()),
                ResolvedDotfileEntry::Disabled(ResolvedDotfileDisable {
                    target: ".config/vscode/gitconfig".to_owned(),
                    origin: ConfigPathOrigin::Project,
                }),
            ],
            dotfiles: vec![global_dotfile],
            ..ResolvedConfig::default()
        };

        let script =
            dotfile_setup_script(&config, "/home/vscode", &variables(Path::new("/workspace")))
                .unwrap();

        assert!(script.is_empty());
    }

    #[test]
    fn setup_script_is_idempotent_for_existing_expected_symlink() {
        let config = ResolvedConfig {
            dotfiles: vec![ResolvedDotfile {
                source: ".decune/nvim".to_owned(),
                target: ".config/nvim".to_owned(),
                read_only: true,
                resolve_symlink: true,
                on_conflict: DotfileConflict::Fail,
                origin: ConfigPathOrigin::Project,
            }],
            ..ResolvedConfig::default()
        };

        let script =
            dotfile_setup_script(&config, "/home/vscode", &variables(Path::new("/workspace")))
                .unwrap();

        assert!(script.contains("readlink \"$dest\""));
        assert!(script.contains("/opt/decune/dotfiles/.config/nvim"));
        assert!(script.contains("/home/vscode/.config/nvim"));
        assert!(script.contains("Dotfile target already exists"));
    }

    #[test]
    fn setup_script_fails_when_intermediate_dotfile_setup_fails() {
        let remote_home = tempfile::tempdir().unwrap();
        fs::write(remote_home.path().join(".config"), "not a directory").unwrap();
        let config = ResolvedConfig {
            dotfiles: vec![
                ResolvedDotfile {
                    source: ".decune/nvim".to_owned(),
                    target: ".config/nvim".to_owned(),
                    read_only: true,
                    resolve_symlink: true,
                    on_conflict: DotfileConflict::Fail,
                    origin: ConfigPathOrigin::Project,
                },
                ResolvedDotfile {
                    source: ".decune/gitconfig".to_owned(),
                    target: ".gitconfig".to_owned(),
                    read_only: true,
                    resolve_symlink: true,
                    on_conflict: DotfileConflict::Fail,
                    origin: ConfigPathOrigin::Project,
                },
            ],
            ..ResolvedConfig::default()
        };

        let script = dotfile_setup_script(
            &config,
            remote_home.path().to_str().unwrap(),
            &variables(Path::new("/workspace")),
        )
        .unwrap();
        let output = Command::new("/bin/sh")
            .args(["-lc", &script])
            .output()
            .unwrap();

        assert!(!output.status.success());
    }

    #[test]
    fn setup_script_allows_root_remote_home() {
        let config = ResolvedConfig {
            dotfiles: vec![ResolvedDotfile {
                source: ".decune/gitconfig".to_owned(),
                target: ".gitconfig".to_owned(),
                read_only: true,
                resolve_symlink: true,
                on_conflict: DotfileConflict::Fail,
                origin: ConfigPathOrigin::Project,
            }],
            ..ResolvedConfig::default()
        };

        let script =
            dotfile_setup_script(&config, "/", &variables(Path::new("/workspace"))).unwrap();

        assert!(script.contains("dest='/.gitconfig'"));
    }

    #[test]
    fn setup_script_replaces_only_existing_symlink_for_replace_symlink() {
        let config = ResolvedConfig {
            dotfiles: vec![ResolvedDotfile {
                source: ".decune/gitconfig".to_owned(),
                target: ".gitconfig".to_owned(),
                read_only: true,
                resolve_symlink: true,
                on_conflict: DotfileConflict::ReplaceSymlink,
                origin: ConfigPathOrigin::Project,
            }],
            ..ResolvedConfig::default()
        };

        let script =
            dotfile_setup_script(&config, "/home/vscode", &variables(Path::new("/workspace")))
                .unwrap();

        assert!(script.contains("if [ -L \"$dest\" ]; then"));
        assert!(script.contains("rm \"$dest\""));
        assert!(script.contains("is not a symlink"));
        assert!(script.contains("rm \"$dest\"\nelse"));
        assert!(script.contains("fi\n  ln -s \"$src\" \"$dest\""));
    }

    #[test]
    fn setup_script_moves_existing_target_for_backup() {
        let config = ResolvedConfig {
            dotfiles: vec![ResolvedDotfile {
                source: ".decune/gitconfig".to_owned(),
                target: ".gitconfig".to_owned(),
                read_only: true,
                resolve_symlink: true,
                on_conflict: DotfileConflict::Backup,
                origin: ConfigPathOrigin::Project,
            }],
            ..ResolvedConfig::default()
        };

        let script =
            dotfile_setup_script(&config, "/home/vscode", &variables(Path::new("/workspace")))
                .unwrap();

        assert!(script.contains(".decune-backup-$(date +%s)"));
        assert!(script.contains("mv \"$dest\" \"$backup\""));
        assert!(script.contains("mv \"$dest\" \"$backup\"\n  ln -s \"$src\" \"$dest\""));
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

        let mounts = materialized_dotfile_mount_specs(
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

    #[test]
    fn does_not_create_skeleton_for_file_dotfile_source() {
        let workspace = tempfile::tempdir().unwrap();
        let source = workspace.path().join("gitconfig");
        fs::write(&source, "[user]\nname = test\n").unwrap();
        let config = ResolvedConfig {
            dotfiles: vec![ResolvedDotfile {
                source: "gitconfig".to_owned(),
                target: ".gitconfig".to_owned(),
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

        assert_eq!(
            mounts[0].source.as_deref(),
            Some(source.canonicalize().unwrap().to_str().unwrap())
        );
        assert!(!workspace.path().join(DOTFILE_MOUNT_SKELETON_DIR).exists());
    }

    #[cfg(unix)]
    #[test]
    fn does_not_create_skeleton_when_resolve_symlink_is_false() {
        let workspace = tempfile::tempdir().unwrap();
        let source_dir = workspace.path().join("lazygit");
        fs::create_dir_all(&source_dir).unwrap();
        fs::write(source_dir.join("config.yml"), "content").unwrap();
        let config = ResolvedConfig {
            dotfiles: vec![ResolvedDotfile {
                source: "lazygit".to_owned(),
                target: ".config/lazygit".to_owned(),
                read_only: true,
                resolve_symlink: false,
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

        assert_eq!(
            mounts[0].source.as_deref(),
            Some(source_dir.to_str().unwrap())
        );
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

        let error = dotfile_mount_specs(
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
