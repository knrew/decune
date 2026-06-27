use std::{
    fs,
    io::Read,
    os::unix::fs::PermissionsExt,
    path::{Component, Path},
};

use anyhow::{Context, Result, bail};

use super::context::{ResolvedBuildContext, build_context_entries};

const TAR_BLOCK_SIZE: usize = 512;

pub(crate) fn create_build_context_tar(context: &ResolvedBuildContext) -> Result<Vec<u8>> {
    let rules = DockerignoreRules::load(context.dockerignore_path.as_deref())?;
    let mut output = Vec::new();
    let entries = build_context_entries(context, &rules)?;
    for relative_path in entries {
        append_tar_entry(&mut output, &context.context_dir, &relative_path)?;
    }

    output.extend([0; TAR_BLOCK_SIZE * 2]);
    Ok(output)
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DockerignoreRules {
    rules: Vec<DockerignoreRule>,
}

impl DockerignoreRules {
    pub(super) fn load(path: Option<&Path>) -> Result<Self> {
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

    pub(super) fn is_ignored(&self, path: &Path) -> bool {
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
        u64::from(metadata.permissions().mode()) & 0o7777,
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
    let checksum = header.iter().map(|byte| u32::from(*byte)).sum::<u32>();
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
const fn padded_size(size: usize) -> usize {
    size.div_ceil(TAR_BLOCK_SIZE) * TAR_BLOCK_SIZE
}

pub(super) fn path_for_docker(path: &Path) -> String {
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
    use std::{collections::BTreeMap, fs};

    use crate::config::layer::LayerDevcontainerBuild;
    use tempfile::TempDir;

    use super::{create_build_context_tar, tar_contains_path};
    use crate::docker::build::context::resolve_build_context;

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
            args: BTreeMap::default(),
            options: Vec::new(),
            target: None,
            cache_from: Vec::new(),
        };
        let context = resolve_build_context(root, &devcontainer_file, &build).unwrap();

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
    fn dockerfile_specific_ignore_overrides_root_dockerignore_for_tar_context() {
        let temp = tempdir("dockerfile-specific-ignore-tar");
        let root = temp.path();
        let devcontainer_file = root.join(".devcontainer/devcontainer.json");
        let context_dir = root.join(".devcontainer");
        fs::create_dir_all(&context_dir).unwrap();
        fs::write(context_dir.join("Dockerfile"), "FROM alpine\n").unwrap();
        fs::write(context_dir.join(".dockerignore"), "specific-kept.txt\n").unwrap();
        fs::write(
            context_dir.join("Dockerfile.dockerignore"),
            "specific-secret.env\n",
        )
        .unwrap();
        fs::write(context_dir.join("specific-kept.txt"), "included\n").unwrap();
        fs::write(context_dir.join("specific-secret.env"), "excluded\n").unwrap();
        let build = LayerDevcontainerBuild {
            dockerfile: "Dockerfile".to_owned(),
            context: None,
            args: BTreeMap::default(),
            options: Vec::new(),
            target: None,
            cache_from: Vec::new(),
        };
        let context = resolve_build_context(root, &devcontainer_file, &build).unwrap();

        let tar = create_build_context_tar(&context).unwrap();

        assert!(tar_contains_path(&tar, "specific-kept.txt"));
        assert!(!tar_contains_path(&tar, "specific-secret.env"));
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
            args: BTreeMap::default(),
            options: Vec::new(),
            target: None,
            cache_from: Vec::new(),
        };
        let context = resolve_build_context(root, &devcontainer_file, &build).unwrap();

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
            args: BTreeMap::default(),
            options: Vec::new(),
            target: None,
            cache_from: Vec::new(),
        };
        let context = resolve_build_context(root, &devcontainer_file, &build).unwrap();

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
            args: BTreeMap::default(),
            options: Vec::new(),
            target: None,
            cache_from: Vec::new(),
        };
        let context = resolve_build_context(root, &devcontainer_file, &build).unwrap();

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
            args: BTreeMap::default(),
            options: Vec::new(),
            target: None,
            cache_from: Vec::new(),
        };
        let context = resolve_build_context(root, &devcontainer_file, &build).unwrap();

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
            args: BTreeMap::default(),
            options: Vec::new(),
            target: None,
            cache_from: Vec::new(),
        };
        let context = resolve_build_context(root, &devcontainer_file, &build).unwrap();

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
            args: BTreeMap::default(),
            options: Vec::new(),
            target: None,
            cache_from: Vec::new(),
        };
        let context = resolve_build_context(root, &devcontainer_file, &build).unwrap();

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
