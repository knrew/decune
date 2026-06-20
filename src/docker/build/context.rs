use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

use super::tar::{DockerignoreRules, path_for_docker};
use crate::config::{
    canonical::{CanonicalWriter, sha256_hex},
    hash::BuildHashInput,
    layer::LayerDevcontainerBuild,
};

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
    let dockerfile_in_context = match dockerfile_path.strip_prefix(&context_dir) {
        Ok(path) => path.to_path_buf(),
        Err(_) => bail!(
            "Dockerfile outside build context is unsupported in decune v0.1: Dockerfile {} is outside build context {}. build.dockerfile must be under build.context because decune sends a generated tar context to docker build -. Workaround: set build.context to a parent directory that contains the Dockerfile, or move the Dockerfile into the context.",
            dockerfile_path.display(),
            context_dir.display()
        ),
    };
    let dockerignore_path = dockerignore_path(&context_dir, &dockerfile_path);

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
        dockerignore_path: context
            .dockerignore_path
            .as_ref()
            .map(|path| path.display().to_string()),
        dockerignore_content_hash,
        context_content_hash: Some(context_content_digest(context)?),
    })
}

pub(super) fn build_context_entries(
    context: &ResolvedBuildContext,
    rules: &DockerignoreRules,
) -> Result<Vec<PathBuf>> {
    let mut entries = Vec::new();
    collect_context_entries(
        &context.context_dir,
        &context.context_dir,
        rules,
        &mut entries,
    )?;
    entries.push(context.dockerfile_in_context.clone());
    if let Some(dockerignore_path) = &context.dockerignore_path {
        let dockerignore_in_context = dockerignore_path
            .strip_prefix(&context.context_dir)
            .with_context(|| {
                format!(
                    ".dockerignore must be inside build context: {} is outside {}",
                    dockerignore_path.display(),
                    context.context_dir.display()
                )
            })?
            .to_path_buf();
        entries.push(dockerignore_in_context);
    }

    entries.sort();
    entries.dedup();
    Ok(entries)
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

fn context_content_digest(context: &ResolvedBuildContext) -> Result<String> {
    let rules = DockerignoreRules::load(context.dockerignore_path.as_deref())?;
    let entries = build_context_entries(context, &rules)?;
    let mut digest_entries = Vec::new();
    for relative_path in entries {
        digest_entries.push(context_digest_entry(context, &relative_path)?);
    }

    let mut writer = CanonicalWriter::default();
    writer.object("BuildContextDigest", |writer| {
        writer.field("entries", |writer| {
            writer.seq(digest_entries.iter(), |writer, entry| {
                writer.object("Entry", |writer| {
                    writer.field("path", |writer| writer.string(&entry.path));
                    writer.field("kind", |writer| writer.string(&entry.kind));
                    writer.field("mode", |writer| writer.string(&entry.mode));
                    writer.field("content_hash", |writer| {
                        writer.option_string(entry.content_hash.as_deref());
                    });
                    writer.field("link_target", |writer| {
                        writer.option_string(entry.link_target.as_deref());
                    });
                });
            });
        });
    });

    Ok(sha256_hex(writer.finish().as_bytes()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ContextDigestEntry {
    path: String,
    kind: String,
    mode: String,
    content_hash: Option<String>,
    link_target: Option<String>,
}

fn context_digest_entry(
    context: &ResolvedBuildContext,
    relative_path: &Path,
) -> Result<ContextDigestEntry> {
    let path = context.context_dir.join(relative_path);
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("Failed to inspect build context path: {}", path.display()))?;
    let kind = if metadata.is_dir() {
        "directory"
    } else if metadata.file_type().is_symlink() {
        "symlink"
    } else if metadata.is_file() {
        "file"
    } else {
        "other"
    };
    let content_hash = if metadata.is_file() {
        let contents = fs::read(&path)
            .with_context(|| format!("Failed to read build context file: {}", path.display()))?;
        Some(sha256_hex(&contents))
    } else {
        None
    };
    let link_target = if metadata.file_type().is_symlink() {
        Some(
            fs::read_link(&path)
                .with_context(|| {
                    format!(
                        "Failed to read symlink in build context: {}",
                        path.display()
                    )
                })?
                .to_string_lossy()
                .to_string(),
        )
    } else {
        None
    };

    Ok(ContextDigestEntry {
        path: path_for_docker(relative_path),
        kind: kind.to_owned(),
        mode: format!("{:o}", file_mode(&metadata)),
        content_hash,
        link_target,
    })
}

#[cfg(unix)]
fn file_mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;

    metadata.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
fn file_mode(_metadata: &fs::Metadata) -> u32 {
    0
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

fn dockerignore_path(context_dir: &Path, dockerfile_path: &Path) -> Option<PathBuf> {
    let dockerfile_specific = dockerfile_path.with_file_name(dockerfile_specific_ignore_name(
        dockerfile_path.file_name()?,
    ));
    if dockerfile_specific.is_file() {
        return Some(dockerfile_specific);
    }

    let default = context_dir.join(".dockerignore");
    default.is_file().then_some(default)
}

fn dockerfile_specific_ignore_name(dockerfile_name: &std::ffi::OsStr) -> OsString {
    let mut name = OsString::from(dockerfile_name);
    name.push(".dockerignore");
    name
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
            options: Vec::new(),
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
            options: Vec::new(),
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
    fn dockerfile_specific_ignore_file_takes_precedence() {
        let temp = tempdir("dockerfile-specific-ignore");
        let root = temp.path();
        let devcontainer_file = root.join(".devcontainer/devcontainer.json");
        let context_dir = root.join(".devcontainer");
        fs::create_dir_all(&context_dir).unwrap();
        let dockerfile = context_dir.join("Dockerfile");
        let dockerfile_ignore = context_dir.join("Dockerfile.dockerignore");
        let root_ignore = context_dir.join(".dockerignore");
        fs::write(&dockerfile, "FROM alpine\n").unwrap();
        fs::write(&dockerfile_ignore, "specific-secret\n").unwrap();
        fs::write(root_ignore, "root-secret\n").unwrap();
        let build = LayerDevcontainerBuild {
            dockerfile: "Dockerfile".to_owned(),
            context: None,
            args: Default::default(),
            options: Vec::new(),
            target: None,
            cache_from: Vec::new(),
        };

        let context = resolve_build_context(root, &devcontainer_file, &build).unwrap();

        assert_eq!(context.dockerignore_path, Some(dockerfile_ignore));
    }

    #[test]
    fn build_hash_input_uses_effective_ignore_path_and_context_digest() {
        let temp = tempdir("hash-input-effective-ignore");
        let root = temp.path();
        let devcontainer_file = root.join(".devcontainer/devcontainer.json");
        let context_dir = root.join(".devcontainer");
        fs::create_dir_all(&context_dir).unwrap();
        let dockerfile = context_dir.join("Dockerfile");
        let dockerfile_ignore = context_dir.join("Dockerfile.dockerignore");
        fs::write(&dockerfile, "FROM alpine\n").unwrap();
        fs::write(&dockerfile_ignore, "ignored.txt\n").unwrap();
        fs::write(context_dir.join(".dockerignore"), "included.txt\n").unwrap();
        fs::write(context_dir.join("included.txt"), "included\n").unwrap();
        fs::write(context_dir.join("ignored.txt"), "ignored\n").unwrap();
        let build = LayerDevcontainerBuild {
            dockerfile: "Dockerfile".to_owned(),
            context: None,
            args: Default::default(),
            options: Vec::new(),
            target: None,
            cache_from: Vec::new(),
        };
        let context = resolve_build_context(root, &devcontainer_file, &build).unwrap();

        let hash_input = super::build_hash_input(&context).unwrap();

        assert_eq!(
            hash_input.dockerignore_path,
            Some(dockerfile_ignore.display().to_string())
        );
        assert!(hash_input.dockerignore_content_hash.is_some());
        assert!(hash_input.context_content_hash.is_some());
    }

    #[test]
    fn ignored_file_changes_do_not_change_context_digest() {
        let temp = tempdir("hash-input-ignored-file");
        let root = temp.path();
        let devcontainer_file = root.join(".devcontainer/devcontainer.json");
        let context_dir = root.join(".devcontainer");
        fs::create_dir_all(&context_dir).unwrap();
        fs::write(context_dir.join("Dockerfile"), "FROM alpine\n").unwrap();
        fs::write(context_dir.join("Dockerfile.dockerignore"), "ignored.txt\n").unwrap();
        fs::write(context_dir.join("included.txt"), "included\n").unwrap();
        fs::write(context_dir.join("ignored.txt"), "first ignored\n").unwrap();
        let build = LayerDevcontainerBuild {
            dockerfile: "Dockerfile".to_owned(),
            context: None,
            args: Default::default(),
            options: Vec::new(),
            target: None,
            cache_from: Vec::new(),
        };
        let context = resolve_build_context(root, &devcontainer_file, &build).unwrap();
        let first = super::build_hash_input(&context).unwrap();

        fs::write(context_dir.join("ignored.txt"), "second ignored\n").unwrap();
        let second = super::build_hash_input(&context).unwrap();

        assert_eq!(first.context_content_hash, second.context_content_hash);
    }

    #[test]
    fn included_file_changes_change_context_digest() {
        let temp = tempdir("hash-input-included-file");
        let root = temp.path();
        let devcontainer_file = root.join(".devcontainer/devcontainer.json");
        let context_dir = root.join(".devcontainer");
        fs::create_dir_all(&context_dir).unwrap();
        fs::write(context_dir.join("Dockerfile"), "FROM alpine\n").unwrap();
        fs::write(context_dir.join("Dockerfile.dockerignore"), "ignored.txt\n").unwrap();
        fs::write(context_dir.join("included.txt"), "first included\n").unwrap();
        fs::write(context_dir.join("ignored.txt"), "ignored\n").unwrap();
        let build = LayerDevcontainerBuild {
            dockerfile: "Dockerfile".to_owned(),
            context: None,
            args: Default::default(),
            options: Vec::new(),
            target: None,
            cache_from: Vec::new(),
        };
        let context = resolve_build_context(root, &devcontainer_file, &build).unwrap();
        let first = super::build_hash_input(&context).unwrap();

        fs::write(context_dir.join("included.txt"), "second included\n").unwrap();
        let second = super::build_hash_input(&context).unwrap();

        assert_ne!(first.context_content_hash, second.context_content_hash);
    }

    #[test]
    fn dockerfile_specific_ignore_file_changes_hash_input() {
        let temp = tempdir("hash-input-ignore-file-change");
        let root = temp.path();
        let devcontainer_file = root.join(".devcontainer/devcontainer.json");
        let context_dir = root.join(".devcontainer");
        fs::create_dir_all(&context_dir).unwrap();
        fs::write(context_dir.join("Dockerfile"), "FROM alpine\n").unwrap();
        fs::write(context_dir.join("Dockerfile.dockerignore"), "ignored.txt\n").unwrap();
        fs::write(context_dir.join("included.txt"), "included\n").unwrap();
        fs::write(context_dir.join("ignored.txt"), "ignored\n").unwrap();
        let build = LayerDevcontainerBuild {
            dockerfile: "Dockerfile".to_owned(),
            context: None,
            args: Default::default(),
            options: Vec::new(),
            target: None,
            cache_from: Vec::new(),
        };
        let context = resolve_build_context(root, &devcontainer_file, &build).unwrap();
        let first = super::build_hash_input(&context).unwrap();

        fs::write(context_dir.join("Dockerfile.dockerignore"), "other.txt\n").unwrap();
        let context = resolve_build_context(root, &devcontainer_file, &build).unwrap();
        let second = super::build_hash_input(&context).unwrap();

        assert_ne!(
            first.dockerignore_content_hash,
            second.dockerignore_content_hash
        );
        assert_ne!(first.context_content_hash, second.context_content_hash);
    }

    #[test]
    fn dockerfile_outside_context_is_rejected() {
        let temp = tempdir("outside-dockerfile");
        let root = temp.path();
        let devcontainer_file = root.join(".devcontainer/devcontainer.json");
        fs::create_dir_all(devcontainer_file.parent().unwrap()).unwrap();
        fs::write(root.join(".devcontainer/Dockerfile"), "FROM alpine\n").unwrap();
        fs::create_dir_all(root.join("app")).unwrap();
        let dockerfile_path = root.join(".devcontainer/Dockerfile");
        let context_path = root.join("app");
        let build = LayerDevcontainerBuild {
            dockerfile: "../.devcontainer/Dockerfile".to_owned(),
            context: Some("../app".to_owned()),
            args: Default::default(),
            options: Vec::new(),
            target: None,
            cache_from: Vec::new(),
        };

        let error = resolve_build_context(root, &devcontainer_file, &build).unwrap_err();
        let message = error.to_string();

        assert!(message.contains("Dockerfile outside build context is unsupported in decune v0.1"));
        assert!(message.contains(&dockerfile_path.display().to_string()));
        assert!(message.contains(&context_path.display().to_string()));
        assert!(message.contains("build.dockerfile must be under build.context"));
        assert!(
            message
                .contains("set build.context to a parent directory that contains the Dockerfile")
        );
        assert!(message.contains("move the Dockerfile into the context"));
    }

    fn tempdir(name: &str) -> TempDir {
        tempfile::Builder::new()
            .prefix(&format!("decune-docker-build-{name}-"))
            .tempdir()
            .unwrap()
    }
}
