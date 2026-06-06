use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

use super::tar::DockerignoreRules;
use crate::config::{canonical::sha256_hex, hash::BuildHashInput, layer::LayerDevcontainerBuild};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedBuildContext {
    pub(crate) context_dir: PathBuf,
    pub(crate) dockerfile_path: PathBuf,
    pub(crate) dockerfile_in_context: PathBuf,
    pub(crate) dockerignore_path: Option<PathBuf>,
}

pub(crate) fn resolve_build_context(
    _workspace_root: &Path,
    devcontainer_file: &Path,
    build: &LayerDevcontainerBuild,
) -> Result<ResolvedBuildContext> {
    let devcontainer_dir = devcontainer_file.parent().with_context(|| {
        format!(
            "Devcontainer path has no parent: {}",
            devcontainer_file.display()
        )
    })?;
    let context_dir = resolve_existing_dir(
        devcontainer_dir,
        build.context.as_deref().unwrap_or("."),
        "Docker build context",
    )?;
    let dockerfile_path = resolve_existing_file(devcontainer_dir, &build.dockerfile, "Dockerfile")?;
    let dockerfile_in_context = dockerfile_path
        .strip_prefix(&context_dir)
        .with_context(|| {
            format!(
                "Dockerfile must be inside build context: {} is outside {}",
                dockerfile_path.display(),
                context_dir.display()
            )
        })?
        .to_path_buf();
    let dockerignore = context_dir.join(".dockerignore");
    let dockerignore_path = if dockerignore.is_file() {
        Some(dockerignore)
    } else {
        None
    };

    Ok(ResolvedBuildContext {
        context_dir,
        dockerfile_path,
        dockerfile_in_context,
        dockerignore_path,
    })
}

pub(crate) fn build_hash_input(context: &ResolvedBuildContext) -> Result<BuildHashInput> {
    let dockerfile = fs::read(&context.dockerfile_path).with_context(|| {
        format!(
            "Failed to read Dockerfile for config hash: {}",
            context.dockerfile_path.display()
        )
    })?;
    let dockerignore_content_hash = match &context.dockerignore_path {
        Some(path) => {
            let contents = fs::read(path).with_context(|| {
                format!(
                    "Failed to read .dockerignore for config hash: {}",
                    path.display()
                )
            })?;
            Some(sha256_hex(&contents))
        }
        None => None,
    };

    Ok(BuildHashInput {
        dockerfile_path: Some(context.dockerfile_path.display().to_string()),
        dockerfile_content_hash: Some(sha256_hex(&dockerfile)),
        context_path: Some(context.context_dir.display().to_string()),
        dockerignore_content_hash,
    })
}

pub(super) fn collect_context_entries(
    context_dir: &Path,
    directory: &Path,
    rules: &DockerignoreRules,
    entries: &mut Vec<PathBuf>,
) -> Result<()> {
    let read_dir = fs::read_dir(directory).with_context(|| {
        format!(
            "Failed to read Docker build context: {}",
            directory.display()
        )
    })?;
    let mut children = read_dir
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| {
            format!(
                "Failed to enumerate Docker build context: {}",
                directory.display()
            )
        })?;
    children.sort_by_key(|entry| entry.path());

    for child in children {
        let path = child.path();
        let relative_path = path.strip_prefix(context_dir).with_context(|| {
            format!(
                "Failed to relativize Docker build context path: {}",
                path.display()
            )
        })?;
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("Failed to inspect build context path: {}", path.display()))?;
        let is_dir = metadata.is_dir();

        if !rules.is_ignored(relative_path) {
            entries.push(relative_path.to_path_buf());
        }

        if is_dir {
            collect_context_entries(context_dir, &path, rules, entries)?;
        }
    }

    Ok(())
}

fn resolve_existing_dir(base: &Path, value: &str, label: &str) -> Result<PathBuf> {
    let path = resolve_path(base, value);
    let canonical = path
        .canonicalize()
        .with_context(|| format!("Failed to resolve {label}: {}", path.display()))?;
    if !canonical.is_dir() {
        bail!("{label} must be a directory: {}", canonical.display());
    }

    Ok(canonical)
}

fn resolve_existing_file(base: &Path, value: &str, label: &str) -> Result<PathBuf> {
    let path = resolve_path(base, value);
    let canonical = path
        .canonicalize()
        .with_context(|| format!("Failed to resolve {label}: {}", path.display()))?;
    if !canonical.is_file() {
        bail!("{label} must be a file: {}", canonical.display());
    }

    Ok(canonical)
}

fn resolve_path(base: &Path, value: &str) -> PathBuf {
    let path = Path::new(value);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use crate::config::layer::LayerDevcontainerBuild;
    use tempfile::TempDir;

    use super::resolve_build_context;

    #[test]
    fn build_context_defaults_to_devcontainer_directory() {
        let temp = tempdir("default-context");
        let root = temp.path();
        let devcontainer_file = root.join(".devcontainer/devcontainer.json");
        fs::create_dir_all(devcontainer_file.parent().unwrap()).unwrap();
        fs::write(root.join(".devcontainer/Dockerfile"), "FROM alpine\n").unwrap();
        let build = LayerDevcontainerBuild {
            dockerfile: "Dockerfile".to_owned(),
            context: None,
            args: Default::default(),
            target: None,
            cache_from: Vec::new(),
        };

        let context = resolve_build_context(root, &devcontainer_file, &build).unwrap();

        assert_eq!(context.context_dir, root.join(".devcontainer"));
        assert_eq!(
            context.dockerfile_path,
            root.join(".devcontainer/Dockerfile")
        );
        assert_eq!(context.dockerfile_in_context, Path::new("Dockerfile"));
    }

    #[test]
    fn build_context_and_dockerfile_are_resolved_relative_to_devcontainer_file() {
        let temp = tempdir("relative-context");
        let root = temp.path();
        let devcontainer_file = root.join(".devcontainer/config/devcontainer.json");
        fs::create_dir_all(devcontainer_file.parent().unwrap()).unwrap();
        fs::create_dir_all(root.join(".devcontainer/docker")).unwrap();
        fs::write(
            root.join(".devcontainer/docker/Dockerfile"),
            "FROM alpine\n",
        )
        .unwrap();
        let build = LayerDevcontainerBuild {
            dockerfile: "../docker/Dockerfile".to_owned(),
            context: Some("..".to_owned()),
            args: Default::default(),
            target: None,
            cache_from: Vec::new(),
        };

        let context = resolve_build_context(root, &devcontainer_file, &build).unwrap();

        assert_eq!(context.context_dir, root.join(".devcontainer"));
        assert_eq!(
            context.dockerfile_path,
            root.join(".devcontainer/docker/Dockerfile")
        );
        assert_eq!(
            context.dockerfile_in_context,
            Path::new("docker/Dockerfile")
        );
    }

    #[test]
    fn dockerfile_outside_context_is_rejected() {
        let temp = tempdir("outside-dockerfile");
        let root = temp.path();
        let devcontainer_file = root.join(".devcontainer/devcontainer.json");
        fs::create_dir_all(devcontainer_file.parent().unwrap()).unwrap();
        fs::write(root.join(".devcontainer/Dockerfile"), "FROM alpine\n").unwrap();
        fs::create_dir_all(root.join("app")).unwrap();
        let build = LayerDevcontainerBuild {
            dockerfile: "../.devcontainer/Dockerfile".to_owned(),
            context: Some("../app".to_owned()),
            args: Default::default(),
            target: None,
            cache_from: Vec::new(),
        };

        let error = resolve_build_context(root, &devcontainer_file, &build).unwrap_err();

        assert!(error.to_string().contains("inside build context"));
    }

    fn tempdir(name: &str) -> TempDir {
        tempfile::Builder::new()
            .prefix(&format!("decune-docker-build-{name}-"))
            .tempdir()
            .unwrap()
    }
}
