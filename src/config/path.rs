use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};

use crate::{
    config::variables::{VariableContext, expand_variables},
    error::ResultExt,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigPathOrigin {
    Global,
    Project,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PathCreate {
    None,
    Directory,
    DirectoryReadOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SymlinkResolution {
    Resolve,
    Preserve,
}

#[derive(Debug, Clone)]
pub(crate) struct HostPathOptions<'a> {
    origin: ConfigPathOrigin,
    workspace_root: &'a Path,
    variables: &'a VariableContext,
    home_dir: Option<PathBuf>,
    create: PathCreate,
    symlink_resolution: SymlinkResolution,
}

impl<'a> HostPathOptions<'a> {
    pub(crate) fn new(
        origin: ConfigPathOrigin,
        workspace_root: &'a Path,
        variables: &'a VariableContext,
    ) -> Self {
        Self {
            origin,
            workspace_root,
            variables,
            home_dir: env_home_dir(),
            create: PathCreate::None,
            symlink_resolution: SymlinkResolution::Resolve,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_home_dir(mut self, home_dir: Option<PathBuf>) -> Self {
        self.home_dir = home_dir;
        self
    }

    pub(crate) fn with_create(mut self, create: PathCreate) -> Self {
        self.create = create;
        self
    }

    pub(crate) fn with_symlink_resolution(mut self, symlink_resolution: SymlinkResolution) -> Self {
        self.symlink_resolution = symlink_resolution;
        self
    }
}

pub(crate) fn resolve_host_path(input: &str, options: &HostPathOptions<'_>) -> Result<PathBuf> {
    let expanded_home = expand_home(input, options.home_dir.as_deref())?;
    let expanded_variables = expand_variables(&expanded_home, options.variables)?;
    resolve_checked_host_path(expanded_variables, options)
}

pub(crate) fn resolve_expanded_host_path(
    input: &str,
    options: &HostPathOptions<'_>,
) -> Result<PathBuf> {
    let expanded_home = expand_home(input, options.home_dir.as_deref())?;
    resolve_checked_host_path(expanded_home, options)
}

fn resolve_checked_host_path(input: String, options: &HostPathOptions<'_>) -> Result<PathBuf> {
    let absolute_path =
        absolutize_config_path(PathBuf::from(input), options.origin, options.workspace_root)?;

    match options.create {
        PathCreate::None => resolve_existing_host_path(&absolute_path, options.symlink_resolution),
        PathCreate::Directory => {
            fs::create_dir_all(&absolute_path)
                .with_path_context("create host path directory", &absolute_path)?;
            resolve_existing_host_path(&absolute_path, options.symlink_resolution)
        }
        PathCreate::DirectoryReadOnly => {
            resolve_read_only_directory_path(&absolute_path, options.symlink_resolution)
        }
    }
}

fn resolve_existing_host_path(
    absolute_path: &Path,
    symlink_resolution: SymlinkResolution,
) -> Result<PathBuf> {
    match symlink_resolution {
        SymlinkResolution::Resolve => absolute_path
            .canonicalize()
            .with_path_context("canonicalize host path", absolute_path),
        SymlinkResolution::Preserve => {
            fs::metadata(absolute_path)
                .with_path_context("read host path metadata", absolute_path)?;
            Ok(absolute_path.to_path_buf())
        }
    }
}

fn resolve_read_only_directory_path(
    absolute_path: &Path,
    symlink_resolution: SymlinkResolution,
) -> Result<PathBuf> {
    let mut current = absolute_path;
    let mut missing_components = Vec::new();
    loop {
        match fs::metadata(current) {
            Ok(metadata) => {
                if !metadata.is_dir() {
                    if missing_components.is_empty() {
                        bail!("Host path is not a directory: {}", current.display());
                    } else {
                        bail!(
                            "Host path ancestor is not a directory: {}",
                            current.display()
                        );
                    }
                }
                if missing_components.is_empty() {
                    return resolve_existing_host_path(absolute_path, symlink_resolution);
                }
                let mut resolved = current
                    .canonicalize()
                    .with_path_context("canonicalize host path ancestor", current)?;
                for component in missing_components.iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                if fs::symlink_metadata(current).is_ok() {
                    return current
                        .canonicalize()
                        .with_path_context("canonicalize host path ancestor", current);
                }
                let Some(file_name) = current.file_name() else {
                    return current
                        .canonicalize()
                        .with_path_context("canonicalize host path ancestor", current);
                };
                missing_components.push(file_name.to_os_string());
                current = current.parent().ok_or_else(|| {
                    anyhow!(
                        "Host path has no existing ancestor: {}",
                        absolute_path.display()
                    )
                })?;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("Failed to read host path metadata: {}", current.display())
                });
            }
        }
    }
}

fn expand_home(input: &str, home_dir: Option<&Path>) -> Result<String> {
    if input == "~" {
        return home_dir
            .map(path_to_string)
            .transpose()?
            .ok_or_else(|| anyhow!("HOME is not set; cannot expand ~ in config path"));
    }

    if let Some(rest) = input.strip_prefix("~/") {
        let home_dir =
            home_dir.ok_or_else(|| anyhow!("HOME is not set; cannot expand ~ in config path"))?;
        return path_to_string(home_dir.join(rest));
    }

    Ok(input.to_owned())
}

fn absolutize_config_path(
    path: PathBuf,
    origin: ConfigPathOrigin,
    workspace_root: &Path,
) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path);
    }

    match origin {
        ConfigPathOrigin::Global => Err(anyhow!(
            "Relative host paths are not allowed in global config: {}",
            path.display()
        )),
        ConfigPathOrigin::Project => Ok(workspace_root.join(path)),
    }
}

fn path_to_string(path: impl AsRef<Path>) -> Result<String> {
    path.as_ref().to_str().map(str::to_owned).ok_or_else(|| {
        anyhow!(
            "Config path is not valid Unicode: {}",
            path.as_ref().display()
        )
    })
}

fn env_home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    #[cfg(unix)]
    use std::os::unix::fs as unix_fs;

    use super::*;
    use crate::config::variables::{VariableContext, VariableContextInput};

    fn fixture_root(name: &str) -> PathBuf {
        let root = env::temp_dir().join(format!("decune-path-tests-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn variables(workspace_root: &Path) -> VariableContext {
        VariableContext::new(VariableContextInput {
            local_workspace_folder: workspace_root.to_path_buf(),
            local_workspace_folder_basename: "project".to_owned(),
            container_workspace_folder: "/workspaces/project".to_owned(),
            container_workspace_folder_basename: "project".to_owned(),
            devcontainer_id: "abc123def456".to_owned(),
            uid: 1000,
            gid: 1001,
            remote_user: "vscode".to_owned(),
            remote_user_home: Some("/home/vscode".to_owned()),
        })
    }

    fn project_options<'a>(
        workspace_root: &'a Path,
        variables: &'a VariableContext,
    ) -> HostPathOptions<'a> {
        HostPathOptions::new(ConfigPathOrigin::Project, workspace_root, variables)
            .with_home_dir(Some(workspace_root.join("home")))
    }

    #[test]
    fn project_relative_path_is_workspace_relative() {
        let root = fixture_root("project-relative");
        let source = root.join("config/nvim");
        fs::create_dir_all(&source).unwrap();
        let variables = variables(&root);

        let path = resolve_host_path("config/nvim", &project_options(&root, &variables)).unwrap();

        assert_eq!(path, source.canonicalize().unwrap());
    }

    #[test]
    fn global_relative_path_is_rejected() {
        let root = fixture_root("global-relative");
        let variables = variables(&root);
        let options = HostPathOptions::new(ConfigPathOrigin::Global, &root, &variables)
            .with_home_dir(Some(root.join("home")));

        let error = resolve_host_path("config/nvim", &options).unwrap_err();

        assert_eq!(
            error.to_string(),
            "Relative host paths are not allowed in global config: config/nvim"
        );
    }

    #[test]
    fn tilde_expands_to_home_directory() {
        let root = fixture_root("tilde");
        let home = root.join("home");
        let source = home.join(".config/nvim");
        fs::create_dir_all(&source).unwrap();
        let variables = variables(&root);
        let options = project_options(&root, &variables).with_home_dir(Some(home));

        let path = resolve_host_path("~/.config/nvim", &options).unwrap();

        assert_eq!(path, source.canonicalize().unwrap());
    }

    #[test]
    fn tilde_requires_home_directory() {
        let root = fixture_root("tilde-no-home");
        let variables = variables(&root);
        let options = project_options(&root, &variables).with_home_dir(None);

        let error = resolve_host_path("~/missing-home", &options).unwrap_err();

        assert_eq!(
            error.to_string(),
            "HOME is not set; cannot expand ~ in config path"
        );
    }

    #[test]
    fn variables_are_expanded_before_relative_resolution() {
        let root = fixture_root("variables");
        let source = root.join("tools");
        fs::create_dir_all(&source).unwrap();
        let variables = variables(&root);

        let path = resolve_host_path(
            "${localWorkspaceFolder}/tools",
            &project_options(&root, &variables),
        )
        .unwrap();

        assert_eq!(path, source.canonicalize().unwrap());
    }

    #[test]
    fn create_directory_creates_only_requested_directory() {
        let root = fixture_root("create-directory");
        let variables = variables(&root);
        let source = root.join("generated/cache");
        let options = project_options(&root, &variables).with_create(PathCreate::Directory);

        let path = resolve_host_path("generated/cache", &options).unwrap();

        assert!(source.is_dir());
        assert_eq!(path, source.canonicalize().unwrap());
    }

    #[test]
    fn read_only_create_directory_resolves_missing_path_without_creating_it() {
        let root = fixture_root("read-only-create-directory");
        let variables = variables(&root);
        let source = root.join("generated/cache");
        let options = project_options(&root, &variables).with_create(PathCreate::DirectoryReadOnly);

        let path = resolve_host_path("generated/cache", &options).unwrap();

        assert!(!source.exists());
        assert_eq!(path, root.canonicalize().unwrap().join("generated/cache"));
    }

    #[test]
    fn read_only_create_directory_rejects_existing_file_ancestor() {
        let root = fixture_root("read-only-create-directory-file-ancestor");
        let variables = variables(&root);
        let source = root.join("generated");
        fs::write(&source, b"not a directory").unwrap();
        let options = project_options(&root, &variables).with_create(PathCreate::DirectoryReadOnly);

        let error = resolve_host_path("generated/cache", &options).unwrap_err();
        let message = format!("{error:#}");

        assert!(message.contains("Host path ancestor is not a directory"));
        assert!(message.contains(&source.display().to_string()));
    }

    #[test]
    fn create_directory_rejects_existing_file() {
        let root = fixture_root("create-directory-existing-file");
        let variables = variables(&root);
        let source = root.join("generated");
        fs::write(&source, b"not a directory").unwrap();
        let options = project_options(&root, &variables).with_create(PathCreate::Directory);

        let error = resolve_host_path("generated/cache", &options).unwrap_err();
        let message = format!("{error:#}");

        assert!(message.contains("Failed to create host path directory"));
        assert!(message.contains(&source.join("cache").display().to_string()));
    }

    #[test]
    fn missing_path_without_create_is_rejected() {
        let root = fixture_root("missing");
        let variables = variables(&root);
        let source = root.join("missing");

        let error = resolve_host_path("missing", &project_options(&root, &variables)).unwrap_err();
        let message = format!("{error:#}");

        assert!(message.contains("Failed to canonicalize host path"));
        assert!(message.contains(&source.display().to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_source_is_canonicalized_by_default() {
        let root = fixture_root("symlink-resolve");
        let target = root.join("actual");
        let link = root.join("linked");
        fs::create_dir_all(&target).unwrap();
        unix_fs::symlink(&target, &link).unwrap();
        let variables = variables(&root);

        let path = resolve_host_path("linked", &project_options(&root, &variables)).unwrap();

        assert_eq!(path, target.canonicalize().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_source_can_be_preserved() {
        let root = fixture_root("symlink-preserve");
        let target = root.join("actual");
        let link = root.join("linked");
        fs::create_dir_all(&target).unwrap();
        unix_fs::symlink(&target, &link).unwrap();
        let variables = variables(&root);
        let options =
            project_options(&root, &variables).with_symlink_resolution(SymlinkResolution::Preserve);

        let path = resolve_host_path("linked", &options).unwrap();

        assert_eq!(path, link);
    }

    #[cfg(unix)]
    #[test]
    fn broken_symlink_is_rejected_with_context() {
        let root = fixture_root("broken-symlink");
        let link = root.join("linked");
        unix_fs::symlink(root.join("missing"), &link).unwrap();
        let variables = variables(&root);

        let error = resolve_host_path("linked", &project_options(&root, &variables)).unwrap_err();
        let message = format!("{error:#}");

        assert!(message.contains("Failed to canonicalize host path"));
        assert!(message.contains(&link.display().to_string()));
    }
}
