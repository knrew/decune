use std::{
    fs,
    io::Read,
    os::unix::fs::PermissionsExt,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use bollard::{
    body_full,
    models::BuildInfo,
    query_parameters::{BuildImageOptions, BuildImageOptionsBuilder},
};
use futures_util::StreamExt;

use crate::{
    config::{canonical::sha256_hex, hash::BuildHashInput, layer::LayerDevcontainerBuild},
    docker::client::DockerClient,
    ui,
};

const TAR_BLOCK_SIZE: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DockerBuildInput {
    pub(crate) image_tag: String,
    pub(crate) labels: std::collections::HashMap<String, String>,
    pub(crate) context: ResolvedBuildContext,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedBuildContext {
    pub(crate) context_dir: PathBuf,
    pub(crate) dockerfile_path: PathBuf,
    pub(crate) dockerfile_in_context: PathBuf,
    pub(crate) dockerignore_path: Option<PathBuf>,
}

pub(crate) async fn build_image(client: &DockerClient, input: DockerBuildInput) -> Result<()> {
    ui::info(&format!("Building Docker image: {}", input.image_tag));

    let tar = create_build_context_tar(&input.context)?;
    let options = build_image_options(&input);
    let mut stream = client
        .raw()
        .build_image(options, None, Some(body_full(tar.into())));

    while let Some(item) = stream.next().await {
        let info =
            item.with_context(|| format!("Failed to build Docker image: {}", input.image_tag))?;
        handle_build_info(&input.image_tag, info)?;
    }

    ui::done(&format!("Built Docker image: {}", input.image_tag));
    Ok(())
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

pub(crate) fn create_build_context_tar(context: &ResolvedBuildContext) -> Result<Vec<u8>> {
    let rules = DockerignoreRules::load(context.dockerignore_path.as_deref())?;
    let mut output = Vec::new();
    let mut entries = Vec::new();
    collect_context_entries(
        &context.context_dir,
        &context.context_dir,
        &rules,
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
    for relative_path in entries {
        append_tar_entry(&mut output, &context.context_dir, &relative_path)?;
    }

    output.extend([0; TAR_BLOCK_SIZE * 2]);
    Ok(output)
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

#[cfg(test)]
pub(crate) fn tar_contains_path(tar: &[u8], path: &str) -> bool {
    let mut offset = 0;
    while offset + TAR_BLOCK_SIZE <= tar.len() {
        let header = &tar[offset..offset + TAR_BLOCK_SIZE];
        if header.iter().all(|byte| *byte == 0) {
            return false;
        }

        if tar_header_path(header).as_deref() == Some(path) {
            return true;
        }

        let size = parse_tar_octal(&header[124..136]).unwrap_or(0);
        offset += TAR_BLOCK_SIZE + padded_size(size);
    }

    false
}

fn build_image_options(input: &DockerBuildInput) -> BuildImageOptions {
    let labels = input.labels.clone();
    BuildImageOptionsBuilder::default()
        .dockerfile(&path_for_docker(&input.context.dockerfile_in_context))
        .t(&input.image_tag)
        .labels(&labels)
        .rm(true)
        .forcerm(true)
        .build()
}

fn handle_build_info(image_tag: &str, info: BuildInfo) -> Result<()> {
    if let Some(error) = info.error_detail.and_then(|detail| detail.message) {
        bail!("Failed to build Docker image: {image_tag}: {error}");
    }

    if let Some(stream) = info.stream {
        let line = stream.trim();
        if !line.is_empty() {
            ui::info(line);
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

fn collect_context_entries(
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct DockerignoreRules {
    rules: Vec<DockerignoreRule>,
}

impl DockerignoreRules {
    fn load(path: Option<&Path>) -> Result<Self> {
        let Some(path) = path else {
            return Ok(Self { rules: Vec::new() });
        };
        let contents = fs::read_to_string(path)
            .with_context(|| format!("Failed to read .dockerignore: {}", path.display()))?;
        Ok(Self::parse(&contents))
    }

    fn parse(contents: &str) -> Self {
        let rules = contents
            .lines()
            .filter_map(DockerignoreRule::parse)
            .collect();

        Self { rules }
    }

    fn is_ignored(&self, path: &Path) -> bool {
        let path = path_for_docker(path);
        self.rules
            .iter()
            .filter(|rule| rule.matches(&path))
            .fold(false, |_, rule| !rule.negated)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DockerignoreRule {
    pattern: String,
    negated: bool,
}

impl DockerignoreRule {
    fn parse(line: &str) -> Option<Self> {
        let line = line.trim_end_matches('\r');
        if line.starts_with('#') {
            return None;
        }

        let mut line = line.trim();
        if line.is_empty() {
            return None;
        }

        let negated = line.starts_with('!');
        if negated {
            line = line[1..].trim_start();
        }

        let line = line.trim_start_matches('/');
        let pattern = line.trim_end_matches('/').to_owned();
        if pattern.is_empty() || pattern == "." {
            return None;
        }

        Some(Self { pattern, negated })
    }

    fn matches(&self, path: &str) -> bool {
        if glob_match(&self.pattern, path) {
            return true;
        }

        let mut parent = path;
        while let Some((next_parent, _)) = parent.rsplit_once('/') {
            if glob_match(&self.pattern, next_parent) {
                return true;
            }
            parent = next_parent;
        }

        false
    }
}

fn append_tar_entry(output: &mut Vec<u8>, context_dir: &Path, relative_path: &Path) -> Result<()> {
    let path = context_dir.join(relative_path);
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("Failed to inspect build context path: {}", path.display()))?;
    let name = path_for_docker(relative_path);

    if metadata.is_dir() {
        append_tar_header(output, &name, &metadata, 0, b'5', None)?;
        return Ok(());
    }

    if metadata.file_type().is_symlink() {
        let target = fs::read_link(&path).with_context(|| {
            format!(
                "Failed to read symlink in build context: {}",
                path.display()
            )
        })?;
        append_tar_header(
            output,
            &name,
            &metadata,
            0,
            b'2',
            Some(target.to_string_lossy().as_ref()),
        )?;
        return Ok(());
    }

    if metadata.is_file() {
        append_tar_header(output, &name, &metadata, metadata.len(), b'0', None)?;
        let mut file = fs::File::open(&path)
            .with_context(|| format!("Failed to read build context file: {}", path.display()))?;
        file.read_to_end(output)
            .with_context(|| format!("Failed to archive build context file: {}", path.display()))?;
        pad_tar(output);
    }

    Ok(())
}

fn append_tar_header(
    output: &mut Vec<u8>,
    name: &str,
    metadata: &fs::Metadata,
    size: u64,
    entry_type: u8,
    link_name: Option<&str>,
) -> Result<()> {
    let mut header = [0u8; TAR_BLOCK_SIZE];
    let (name, prefix) = split_tar_name(name)?;
    write_tar_bytes(&mut header[0..100], name.as_bytes());
    write_tar_octal(
        &mut header[100..108],
        metadata.permissions().mode() as u64 & 0o7777,
    );
    write_tar_octal(&mut header[108..116], 0);
    write_tar_octal(&mut header[116..124], 0);
    write_tar_octal(&mut header[124..136], size);
    write_tar_octal(
        &mut header[136..148],
        metadata
            .modified()
            .ok()
            .and_then(|time| {
                time.duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .map(|duration| duration.as_secs())
            })
            .unwrap_or(0),
    );
    for byte in &mut header[148..156] {
        *byte = b' ';
    }
    header[156] = entry_type;
    if let Some(link_name) = link_name {
        write_tar_bytes(&mut header[157..257], link_name.as_bytes());
    }
    write_tar_bytes(&mut header[257..263], b"ustar\0");
    write_tar_bytes(&mut header[263..265], b"00");
    if let Some(prefix) = prefix {
        write_tar_bytes(&mut header[345..500], prefix.as_bytes());
    }
    let checksum = header.iter().map(|byte| *byte as u32).sum::<u32>();
    write_tar_checksum(&mut header[148..156], checksum);
    output.extend(header);

    Ok(())
}

fn split_tar_name(path: &str) -> Result<(&str, Option<&str>)> {
    if path.len() <= 100 {
        return Ok((path, None));
    }

    for index in path.match_indices('/').map(|(index, _)| index).rev() {
        let prefix = &path[..index];
        let name = &path[index + 1..];
        if prefix.len() <= 155 && name.len() <= 100 {
            return Ok((name, Some(prefix)));
        }
    }

    bail!("Build context path is too long for tar header: {path}");
}

fn write_tar_bytes(target: &mut [u8], value: &[u8]) {
    let len = target.len().min(value.len());
    target[..len].copy_from_slice(&value[..len]);
}

fn write_tar_octal(target: &mut [u8], value: u64) {
    let width = target.len() - 1;
    let text = format!("{value:0width$o}");
    write_tar_bytes(&mut target[..width], text.as_bytes());
    target[width] = 0;
}

fn write_tar_checksum(target: &mut [u8], value: u32) {
    let text = format!("{value:06o}\0 ");
    write_tar_bytes(target, text.as_bytes());
}

fn pad_tar(output: &mut Vec<u8>) {
    let remainder = output.len() % TAR_BLOCK_SIZE;
    if remainder != 0 {
        output.extend(vec![0; TAR_BLOCK_SIZE - remainder]);
    }
}

#[cfg(test)]
fn tar_header_path(header: &[u8]) -> Option<String> {
    let name = tar_string(&header[0..100])?;
    let prefix = tar_string(&header[345..500]);
    Some(match prefix {
        Some(prefix) => format!("{prefix}/{name}"),
        None => name,
    })
}

#[cfg(test)]
fn tar_string(bytes: &[u8]) -> Option<String> {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    if end == 0 {
        None
    } else {
        Some(String::from_utf8_lossy(&bytes[..end]).to_string())
    }
}

#[cfg(test)]
fn parse_tar_octal(bytes: &[u8]) -> Option<usize> {
    let text = String::from_utf8_lossy(bytes);
    usize::from_str_radix(text.trim_matches(char::from(0)).trim(), 8).ok()
}

#[cfg(test)]
fn padded_size(size: usize) -> usize {
    size.div_ceil(TAR_BLOCK_SIZE) * TAR_BLOCK_SIZE
}

fn path_for_docker(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn glob_match(pattern: &str, text: &str) -> bool {
    glob_match_bytes(pattern.as_bytes(), text.as_bytes())
}

fn glob_match_bytes(pattern: &[u8], text: &[u8]) -> bool {
    match pattern.split_first() {
        None => text.is_empty(),
        Some((&b'*', rest)) => {
            if let Some((&b'*', rest)) = rest.split_first() {
                let matches_zero_directories = rest
                    .strip_prefix(b"/")
                    .is_some_and(|rest| glob_match_bytes(rest, text));

                if matches_zero_directories {
                    return true;
                }

                glob_match_bytes(rest, text)
                    || (!text.is_empty() && glob_match_bytes(pattern, &text[1..]))
            } else {
                glob_match_bytes(rest, text)
                    || (!text.is_empty()
                        && text[0] != b'/'
                        && glob_match_bytes(pattern, &text[1..]))
            }
        }
        Some((&b'?', rest)) => {
            !text.is_empty() && text[0] != b'/' && glob_match_bytes(rest, &text[1..])
        }
        Some((&expected, rest)) => {
            !text.is_empty() && text[0] == expected && glob_match_bytes(rest, &text[1..])
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use crate::config::layer::LayerDevcontainerBuild;
    use tempfile::TempDir;

    use super::{create_build_context_tar, resolve_build_context, tar_contains_path};

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

        let context = resolve_build_context(&root, &devcontainer_file, &build).unwrap();

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

        let context = resolve_build_context(&root, &devcontainer_file, &build).unwrap();

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

        let error = resolve_build_context(&root, &devcontainer_file, &build).unwrap_err();

        assert!(error.to_string().contains("inside build context"));
    }

    #[test]
    fn dockerignore_excludes_files_from_tar_context() {
        let temp = tempdir("dockerignore-tar");
        let root = temp.path();
        let devcontainer_file = root.join(".devcontainer/devcontainer.json");
        let context_dir = root.join(".devcontainer");
        fs::create_dir_all(context_dir.join("tmp")).unwrap();
        fs::write(context_dir.join("Dockerfile"), "FROM alpine\n").unwrap();
        fs::write(context_dir.join("app.txt"), "included-content\n").unwrap();
        fs::write(context_dir.join("secret.env"), "excluded-secret\n").unwrap();
        fs::write(context_dir.join("tmp/cache.txt"), "excluded-cache\n").unwrap();
        fs::write(context_dir.join(".dockerignore"), "*.env\ntmp/\n").unwrap();
        let build = LayerDevcontainerBuild {
            dockerfile: "Dockerfile".to_owned(),
            context: None,
            args: Default::default(),
            target: None,
            cache_from: Vec::new(),
        };
        let context = resolve_build_context(&root, &devcontainer_file, &build).unwrap();

        let tar = create_build_context_tar(&context).unwrap();

        assert!(tar_contains_path(&tar, "Dockerfile"));
        assert!(tar_contains_path(&tar, "app.txt"));
        assert!(tar_contains_path(&tar, ".dockerignore"));
        assert!(!tar_contains_path(&tar, "secret.env"));
        assert!(!tar_contains_path(&tar, "tmp/cache.txt"));
        let text = String::from_utf8_lossy(&tar);
        assert!(text.contains("included-content"));
        assert!(!text.contains("excluded-secret"));
        assert!(!text.contains("excluded-cache"));
    }

    #[test]
    fn dockerignore_negation_reincludes_later_matches() {
        let temp = tempdir("dockerignore-negation");
        let root = temp.path();
        let devcontainer_file = root.join(".devcontainer/devcontainer.json");
        let context_dir = root.join(".devcontainer");
        fs::create_dir_all(context_dir.join("tmp")).unwrap();
        fs::write(context_dir.join("Dockerfile"), "FROM alpine\n").unwrap();
        fs::write(context_dir.join("tmp/cache.txt"), "excluded-cache\n").unwrap();
        fs::write(context_dir.join("tmp/keep.txt"), "included-keep\n").unwrap();
        fs::write(context_dir.join(".dockerignore"), "tmp/*\n!tmp/keep.txt\n").unwrap();
        let build = LayerDevcontainerBuild {
            dockerfile: "Dockerfile".to_owned(),
            context: None,
            args: Default::default(),
            target: None,
            cache_from: Vec::new(),
        };
        let context = resolve_build_context(&root, &devcontainer_file, &build).unwrap();

        let tar = create_build_context_tar(&context).unwrap();

        assert!(!tar_contains_path(&tar, "tmp/cache.txt"));
        assert!(tar_contains_path(&tar, "tmp/keep.txt"));
    }

    #[test]
    fn dockerignore_keeps_build_metadata_when_ignore_rule_matches_everything() {
        let temp = tempdir("dockerignore-build-metadata");
        let root = temp.path();
        let devcontainer_file = root.join(".devcontainer/devcontainer.json");
        let context_dir = root.join(".devcontainer");
        fs::create_dir_all(&context_dir).unwrap();
        fs::write(context_dir.join("Dockerfile"), "FROM alpine\n").unwrap();
        fs::write(context_dir.join("app.txt"), "excluded-content\n").unwrap();
        fs::write(context_dir.join(".dockerignore"), "*\n").unwrap();
        let build = LayerDevcontainerBuild {
            dockerfile: "Dockerfile".to_owned(),
            context: None,
            args: Default::default(),
            target: None,
            cache_from: Vec::new(),
        };
        let context = resolve_build_context(&root, &devcontainer_file, &build).unwrap();

        let tar = create_build_context_tar(&context).unwrap();

        assert!(tar_contains_path(&tar, "Dockerfile"));
        assert!(tar_contains_path(&tar, ".dockerignore"));
        assert!(!tar_contains_path(&tar, "app.txt"));
    }

    #[test]
    fn dockerignore_glob_star_does_not_cross_path_separator() {
        let temp = tempdir("dockerignore-glob-star");
        let root = temp.path();
        let devcontainer_file = root.join(".devcontainer/devcontainer.json");
        let context_dir = root.join(".devcontainer");
        fs::create_dir_all(context_dir.join("foo/bar")).unwrap();
        fs::write(context_dir.join("Dockerfile"), "FROM alpine\n").unwrap();
        fs::write(context_dir.join("foo/root.txt"), "excluded-root\n").unwrap();
        fs::write(context_dir.join("foo/bar/baz.txt"), "included-nested\n").unwrap();
        fs::write(context_dir.join(".dockerignore"), "foo/*.txt\n").unwrap();
        let build = LayerDevcontainerBuild {
            dockerfile: "Dockerfile".to_owned(),
            context: None,
            args: Default::default(),
            target: None,
            cache_from: Vec::new(),
        };
        let context = resolve_build_context(&root, &devcontainer_file, &build).unwrap();

        let tar = create_build_context_tar(&context).unwrap();

        assert!(!tar_contains_path(&tar, "foo/root.txt"));
        assert!(tar_contains_path(&tar, "foo/bar/baz.txt"));
    }

    #[test]
    fn dockerignore_double_star_slash_matches_root_files() {
        let temp = tempdir("dockerignore-double-star-root");
        let root = temp.path();
        let devcontainer_file = root.join(".devcontainer/devcontainer.json");
        let context_dir = root.join(".devcontainer");
        fs::create_dir_all(context_dir.join("config")).unwrap();
        fs::write(context_dir.join("Dockerfile"), "FROM alpine\n").unwrap();
        fs::write(context_dir.join("secret.env"), "excluded-root-secret\n").unwrap();
        fs::write(
            context_dir.join("config/secret.env"),
            "excluded-nested-secret\n",
        )
        .unwrap();
        fs::write(context_dir.join("app.txt"), "included-content\n").unwrap();
        fs::write(context_dir.join(".dockerignore"), "**/*.env\n").unwrap();
        let build = LayerDevcontainerBuild {
            dockerfile: "Dockerfile".to_owned(),
            context: None,
            args: Default::default(),
            target: None,
            cache_from: Vec::new(),
        };
        let context = resolve_build_context(&root, &devcontainer_file, &build).unwrap();

        let tar = create_build_context_tar(&context).unwrap();

        assert!(!tar_contains_path(&tar, "secret.env"));
        assert!(!tar_contains_path(&tar, "config/secret.env"));
        assert!(tar_contains_path(&tar, "app.txt"));
        let text = String::from_utf8_lossy(&tar);
        assert!(!text.contains("excluded-root-secret"));
        assert!(!text.contains("excluded-nested-secret"));
        assert!(text.contains("included-content"));
    }

    #[test]
    fn dockerignore_trailing_slash_matches_like_docker() {
        let temp = tempdir("dockerignore-trailing-slash");
        let root = temp.path();
        let devcontainer_file = root.join(".devcontainer/devcontainer.json");
        let context_dir = root.join(".devcontainer");
        fs::create_dir_all(context_dir.join("node_modules")).unwrap();
        fs::create_dir_all(context_dir.join("app/node_modules")).unwrap();
        fs::write(context_dir.join("Dockerfile"), "FROM alpine\n").unwrap();
        fs::write(context_dir.join("node_modules/pkg.json"), "excluded-root\n").unwrap();
        fs::write(
            context_dir.join("app/node_modules/pkg.json"),
            "included-nested\n",
        )
        .unwrap();
        fs::write(context_dir.join(".dockerignore"), "node_modules/\n").unwrap();
        let build = LayerDevcontainerBuild {
            dockerfile: "Dockerfile".to_owned(),
            context: None,
            args: Default::default(),
            target: None,
            cache_from: Vec::new(),
        };
        let context = resolve_build_context(&root, &devcontainer_file, &build).unwrap();

        let tar = create_build_context_tar(&context).unwrap();

        assert!(!tar_contains_path(&tar, "node_modules/pkg.json"));
        assert!(tar_contains_path(&tar, "app/node_modules"));
        assert!(tar_contains_path(&tar, "app/node_modules/pkg.json"));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_archived_without_following_targets() {
        use std::os::unix::fs as unix_fs;

        let temp = tempdir("symlink-context");
        let outside_temp = tempdir("symlink-outside");
        let root = temp.path();
        let outside = outside_temp.path();
        let devcontainer_file = root.join(".devcontainer/devcontainer.json");
        let context_dir = root.join(".devcontainer");
        fs::create_dir_all(&context_dir).unwrap();
        fs::write(context_dir.join("Dockerfile"), "FROM alpine\n").unwrap();
        fs::write(outside.join("outside.txt"), "outside-content\n").unwrap();
        unix_fs::symlink(outside.join("outside.txt"), context_dir.join("linked.txt")).unwrap();
        let build = LayerDevcontainerBuild {
            dockerfile: "Dockerfile".to_owned(),
            context: None,
            args: Default::default(),
            target: None,
            cache_from: Vec::new(),
        };
        let context = resolve_build_context(&root, &devcontainer_file, &build).unwrap();

        let tar = create_build_context_tar(&context).unwrap();

        assert!(tar_contains_path(&tar, "linked.txt"));
        assert!(!String::from_utf8_lossy(&tar).contains("outside-content"));
    }

    fn tempdir(name: &str) -> TempDir {
        tempfile::Builder::new()
            .prefix(&format!("decune-docker-build-{name}-"))
            .tempdir()
            .unwrap()
    }
}
