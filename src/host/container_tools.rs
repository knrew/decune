use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    io::{Read, Seek, Write},
    os::unix::fs::PermissionsExt,
    path::{Component, Path, PathBuf},
};

#[cfg(test)]
use std::fmt::Write as _;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tempfile::{Builder, NamedTempFile};

use crate::hex::hex_lower;

pub(crate) const CONTAINER_TOOLS_ENV: &str = "DECUNE_CONTAINER_TOOLS_DIR";
const REQUIRED_TOOLS: [ContainerTool; 3] = [
    ContainerTool::GitCredentialHelper,
    ContainerTool::ForwardAgent,
    ContainerTool::Decune,
];
const REQUIRED_PLATFORM_IDS: [&str; 2] = ["linux-amd64", "linux-arm64"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContainerTool {
    GitCredentialHelper,
    ForwardAgent,
    Decune,
}

impl ContainerTool {
    pub(crate) const fn file_name(self) -> &'static str {
        match self {
            Self::GitCredentialHelper => "git-credential-decune",
            Self::ForwardAgent => "decune-forward-agent",
            Self::Decune => "decune",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ContainerToolPlatform {
    LinuxAmd64,
    LinuxArm64,
}

impl ContainerToolPlatform {
    pub(crate) const fn id(self) -> &'static str {
        match self {
            Self::LinuxAmd64 => "linux-amd64",
            Self::LinuxArm64 => "linux-arm64",
        }
    }

    pub(crate) fn from_docker_os_arch(os: &str, arch: &str) -> Result<Self> {
        match (os, arch) {
            ("linux", "amd64" | "x86_64") => Ok(Self::LinuxAmd64),
            ("linux", "arm64" | "aarch64") => Ok(Self::LinuxArm64),
            _ => bail!("Unsupported container platform for decune container tools: {os}/{arch}"),
        }
    }
}

pub(crate) struct EmbeddedContainerToolArtifact {
    pub(crate) name: &'static str,
    pub(crate) platform: &'static str,
    pub(crate) sha256: &'static str,
    pub(crate) bytes: &'static [u8],
}

include!(concat!(env!("OUT_DIR"), "/container_tools_bundle.rs"));

pub(crate) fn stage_container_tool(
    tool: ContainerTool,
    platform: ContainerToolPlatform,
    runtime_dir: &Path,
) -> Result<PathBuf> {
    let private_parent = runtime_dir.parent().with_context(|| {
        format!(
            "Decune container tool runtime directory has no host-private parent: {}",
            runtime_dir.display()
        )
    })?;
    let source_dir = container_tool_override_dir();
    stage_container_tool_from_sources(
        tool,
        platform,
        runtime_dir,
        private_parent,
        source_dir.as_deref(),
        default_embedded_container_tools(),
    )
}

pub(crate) fn stage_container_tool_with_private_parent(
    tool: ContainerTool,
    platform: ContainerToolPlatform,
    runtime_dir: &Path,
    private_parent: &Path,
) -> Result<PathBuf> {
    let source_dir = container_tool_override_dir();
    stage_container_tool_from_sources(
        tool,
        platform,
        runtime_dir,
        private_parent,
        source_dir.as_deref(),
        default_embedded_container_tools(),
    )
}

fn default_embedded_container_tools() -> &'static [EmbeddedContainerToolArtifact] {
    #[cfg(test)]
    {
        if EMBEDDED_CONTAINER_TOOLS.is_empty() {
            return TEST_EMBEDDED_CONTAINER_TOOLS;
        }
    }

    EMBEDDED_CONTAINER_TOOLS
}

#[cfg(test)]
const TEST_EMBEDDED_CONTAINER_TOOLS: &[EmbeddedContainerToolArtifact] = &[
    EmbeddedContainerToolArtifact {
        name: "decune-forward-agent",
        platform: "linux-amd64",
        sha256: "e43fad88995343d19035dbc9b9a181c46454c4305d0b19ce54addbacb31723e2",
        bytes: b"test-forward-agent",
    },
    EmbeddedContainerToolArtifact {
        name: "git-credential-decune",
        platform: "linux-amd64",
        sha256: "8449cd1f1a78299ef5aa03f9f7b62f657e0c028dd841a06a96755685221044a4",
        bytes: b"test-git-credential-helper",
    },
    EmbeddedContainerToolArtifact {
        name: "decune",
        platform: "linux-amd64",
        sha256: "6b8a134ce0be473579f329a795a8552617131d17a49d75d4d9deebd8a4ddc2b2",
        bytes: b"test-decune",
    },
];

pub(crate) fn stage_container_tool_from_dir(
    tool: ContainerTool,
    platform: ContainerToolPlatform,
    runtime_dir: &Path,
    source_dir: &Path,
) -> Result<PathBuf> {
    let private_parent = runtime_dir.parent().with_context(|| {
        format!(
            "Decune container tool runtime directory has no host-private parent: {}",
            runtime_dir.display()
        )
    })?;
    stage_container_tool_from_sources(
        tool,
        platform,
        runtime_dir,
        private_parent,
        Some(source_dir),
        &[],
    )
}

#[cfg(test)]
fn stage_container_tool_with_embedded(
    tool: ContainerTool,
    platform: ContainerToolPlatform,
    runtime_dir: &Path,
    source_dir: Option<&Path>,
    embedded: &'static [EmbeddedContainerToolArtifact],
) -> Result<PathBuf> {
    let private_parent = runtime_dir.parent().with_context(|| {
        format!(
            "Decune container tool runtime directory has no host-private parent: {}",
            runtime_dir.display()
        )
    })?;
    stage_container_tool_from_sources(
        tool,
        platform,
        runtime_dir,
        private_parent,
        source_dir,
        embedded,
    )
}

#[cfg(test)]
pub(crate) struct TestContainerToolEntry<'a> {
    pub(crate) tool: ContainerTool,
    pub(crate) platform: ContainerToolPlatform,
    pub(crate) contents: &'a [u8],
}

#[cfg(test)]
pub(crate) fn write_test_container_tools_bundle(
    source_dir: &Path,
    entries: &[TestContainerToolEntry<'_>],
) -> Result<()> {
    fs::create_dir_all(source_dir).with_context(|| {
        format!(
            "Failed to create test container tools bundle dir: {}",
            source_dir.display()
        )
    })?;

    let overrides = entries
        .iter()
        .map(|entry| {
            (
                (entry.tool.file_name(), entry.platform.id()),
                entry.contents,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut manifest_entries =
        Vec::with_capacity(REQUIRED_TOOLS.len() * REQUIRED_PLATFORM_IDS.len());
    let mut sums = String::new();
    for platform in REQUIRED_PLATFORM_IDS {
        for tool in REQUIRED_TOOLS {
            let tool = tool.file_name();
            let default_contents = format!("test {tool} for {platform}");
            let contents = overrides
                .get(&(tool, platform))
                .copied()
                .unwrap_or(default_contents.as_bytes());
            let relative_path = PathBuf::from(platform).join(tool);
            let artifact_path = source_dir.join(&relative_path);
            if let Some(parent) = artifact_path.parent() {
                fs::create_dir_all(parent).with_context(|| {
                    format!(
                        "Failed to create test container tools artifact dir: {}",
                        parent.display()
                    )
                })?;
            }
            fs::write(&artifact_path, contents).with_context(|| {
                format!(
                    "Failed to write test container tools artifact: {}",
                    artifact_path.display()
                )
            })?;
            fs::set_permissions(&artifact_path, fs::Permissions::from_mode(0o755)).with_context(
                || {
                    format!(
                        "Failed to set test container tools artifact permissions: {}",
                        artifact_path.display()
                    )
                },
            )?;
            let sha256 = hex_lower(&Sha256::digest(contents));
            manifest_entries.push(serde_json::json!({
                "name": tool,
                "platform": platform,
                "path": relative_path.display().to_string(),
                "sha256": sha256,
            }));
            writeln!(sums, "{sha256}  {}", relative_path.display())
                .context("Failed to format test container tools SHA256SUMS")?;
        }
    }

    let manifest = serde_json::json!({
        "schemaVersion": 1,
        "protocolVersion": 1,
        "tools": manifest_entries,
    });
    let manifest_json = serde_json::to_string(&manifest)
        .context("Failed to serialize test container tools manifest")?;
    fs::write(
        source_dir.join("manifest.json"),
        format!("{manifest_json}\n"),
    )
    .with_context(|| {
        format!(
            "Failed to write test container tools manifest: {}",
            source_dir.join("manifest.json").display()
        )
    })?;
    fs::write(source_dir.join("SHA256SUMS"), sums).with_context(|| {
        format!(
            "Failed to write test container tools checksums: {}",
            source_dir.join("SHA256SUMS").display()
        )
    })
}

fn stage_container_tool_from_sources(
    tool: ContainerTool,
    platform: ContainerToolPlatform,
    runtime_dir: &Path,
    private_parent: &Path,
    source_dir: Option<&Path>,
    embedded: &'static [EmbeddedContainerToolArtifact],
) -> Result<PathBuf> {
    let target = runtime_dir.join(tool.file_name());
    let resolved = resolve_container_tool(tool, platform, source_dir, embedded)?;
    let expected_sha256 = resolved.sha256().to_owned();

    atomic_stage_container_tool(&target, private_parent, &expected_sha256, |staged| {
        match &resolved {
            ResolvedContainerTool::ExternalFile { path, .. } => {
                let mut source = fs::File::open(path).with_context(|| {
                    format!(
                        "Failed to open decune container tool artifact for staging: {}",
                        path.display()
                    )
                })?;
                std::io::copy(&mut source, staged).with_context(|| {
                    format!(
                        "Failed to stage decune container tool artifact from {}",
                        path.display()
                    )
                })?;
            }
            ResolvedContainerTool::Embedded(artifact) => {
                staged.write_all(artifact.bytes).with_context(|| {
                    format!(
                        "Failed to stage embedded decune container tool artifact: {} for {}",
                        artifact.name, artifact.platform
                    )
                })?;
            }
        }
        Ok(())
    })?;
    Ok(target)
}

fn atomic_stage_container_tool(
    target: &Path,
    private_parent: &Path,
    expected_sha256: &str,
    write_contents: impl FnOnce(&mut fs::File) -> Result<()>,
) -> Result<()> {
    let target_name = target.file_name().with_context(|| {
        format!(
            "Decune container tool target has no file name: {}",
            target.display()
        )
    })?;
    let prefix = format!(".{}.", target_name.to_string_lossy());
    let mut staged = create_private_staging_file(private_parent, &prefix, 12)?;

    write_contents(staged.as_file_mut())?;
    staged
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o755))
        .with_context(|| {
            format!(
                "Failed to set staged decune container tool artifact permissions for {}",
                target.display()
            )
        })?;
    verify_open_file_sha256(staged.as_file_mut(), expected_sha256, target)?;
    validate_replaceable_target(target)?;

    staged.persist(target).map_err(|error| error.error).with_context(|| {
        format!(
            "Failed to atomically replace decune container tool artifact; runtime path may be corrupt: {}",
            target.display()
        )
    })?;
    Ok(())
}

fn create_private_staging_file(
    private_parent: &Path,
    prefix: &str,
    random_bytes: usize,
) -> Result<NamedTempFile> {
    Builder::new()
        .prefix(prefix)
        .rand_bytes(random_bytes)
        .tempfile_in(private_parent)
        .with_context(|| {
            format!(
                "Failed to exclusively create private decune container tool staging file in {}",
                private_parent.display()
            )
        })
}

fn verify_open_file_sha256(file: &mut fs::File, expected: &str, target: &Path) -> Result<()> {
    let actual = sha256_open_file(file, target, "staged decune container tool artifact")?;
    if actual != expected {
        bail!(
            "Container tool artifact checksum mismatch after staging: {}",
            target.display()
        );
    }
    Ok(())
}

fn sha256_open_file(file: &mut fs::File, path: &Path, description: &str) -> Result<String> {
    file.rewind().with_context(|| {
        format!(
            "Failed to rewind {description} for checksum: {}",
            path.display()
        )
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = file.read(&mut buffer).with_context(|| {
            format!(
                "Failed to read {description} for checksum: {}",
                path.display()
            )
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex_lower(&hasher.finalize()))
}

fn validate_replaceable_target(target: &Path) -> Result<()> {
    match fs::symlink_metadata(target) {
        Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => bail!(
            "Decune container tool runtime path is not a regular file or symlink: {}",
            target.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| {
            format!(
                "Failed to inspect decune container tool runtime path: {}",
                target.display()
            )
        }),
    }
}

fn resolve_container_tool(
    tool: ContainerTool,
    platform: ContainerToolPlatform,
    source_dir: Option<&Path>,
    embedded: &'static [EmbeddedContainerToolArtifact],
) -> Result<ResolvedContainerTool<'static>> {
    if let Some(source_dir) = source_dir {
        return resolve_external_container_tool(tool, platform, source_dir);
    }

    if let Some(artifact) = embedded
        .iter()
        .find(|artifact| artifact.name == tool.file_name() && artifact.platform == platform.id())
    {
        verify_embedded_sha256(artifact)?;
        return Ok(ResolvedContainerTool::Embedded(artifact));
    }

    bail!(
        "Missing decune container tool artifact: {} for {}.\nThis decune binary was built without embedded container tools, or DECUNE_CONTAINER_TOOLS_DIR points to an incomplete bundle.\nInstall an official release binary or set DECUNE_CONTAINER_TOOLS_DIR to a valid bundle.",
        tool.file_name(),
        platform.id()
    )
}

fn resolve_external_container_tool(
    tool: ContainerTool,
    platform: ContainerToolPlatform,
    source_dir: &Path,
) -> Result<ResolvedContainerTool<'static>> {
    let artifacts = validate_external_container_tools_bundle(source_dir)?;
    let Some(artifact) = artifacts
        .into_iter()
        .find(|artifact| artifact.name == tool.file_name() && artifact.platform == platform.id())
    else {
        bail!(
            "DECUNE_CONTAINER_TOOLS_DIR bundle is missing required decune container tool artifact: {} for {}",
            tool.file_name(),
            platform.id()
        );
    };
    Ok(ResolvedContainerTool::ExternalFile {
        path: artifact.path,
        sha256: artifact.sha256,
    })
}

fn validate_external_container_tools_bundle(
    source_dir: &Path,
) -> Result<Vec<ValidatedExternalArtifact>> {
    let manifest_path = source_dir.join("manifest.json");
    let manifest = match fs::read_to_string(&manifest_path) {
        Ok(manifest) => manifest,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!(
                "DECUNE_CONTAINER_TOOLS_DIR must point to a decune container tools bundle with manifest.json: {}",
                source_dir.display()
            );
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "Failed to read container tools manifest: {}",
                    manifest_path.display()
                )
            });
        }
    };
    let manifest: ContainerToolsManifest = serde_json::from_str(&manifest).with_context(|| {
        format!(
            "Failed to parse container tools manifest: {}",
            manifest_path.display()
        )
    })?;
    if manifest.schema_version != 1 {
        bail!(
            "Unsupported container tools manifest schemaVersion: {}",
            manifest.schema_version
        );
    }
    if manifest.protocol_version != 1 {
        bail!(
            "Unsupported container tools protocolVersion: {}",
            manifest.protocol_version
        );
    }

    let expected = expected_container_tool_set();
    let mut seen = BTreeSet::new();
    let mut manifest_sums = BTreeMap::new();
    let mut artifacts = Vec::with_capacity(manifest.tools.len());
    for entry in manifest.tools {
        let key = (entry.name.clone(), entry.platform.clone());
        if !expected.contains(&key) {
            bail!(
                "Unexpected container tool artifact in manifest: {} for {}",
                entry.name,
                entry.platform
            );
        }
        if !seen.insert(key) {
            bail!(
                "Duplicate container tool artifact in manifest: {} for {}",
                entry.name,
                entry.platform
            );
        }
        validate_manifest_path(&entry.path)?;
        validate_sha256_string(&entry.sha256)?;
        if manifest_sums
            .insert(entry.path.clone(), entry.sha256.clone())
            .is_some()
        {
            bail!(
                "Duplicate container tool artifact path in manifest: {}",
                entry.path.display()
            );
        }
        let path = source_dir.join(&entry.path);
        if !path.is_file() {
            bail!(
                "Container tools manifest entry does not exist: {}",
                path.display()
            );
        }
        validate_executable(&path)?;
        verify_file_sha256(&path, &entry.sha256)?;
        artifacts.push(ValidatedExternalArtifact {
            name: entry.name,
            platform: entry.platform,
            path,
            sha256: entry.sha256,
        });
    }
    if seen != expected {
        let missing = expected.difference(&seen).cloned().collect::<Vec<_>>();
        bail!("Missing required container tool artifacts: {missing:?}");
    }
    check_sha256sums(source_dir, &manifest_sums)?;
    Ok(artifacts)
}

fn validate_manifest_path(path: &Path) -> Result<()> {
    if path.is_absolute() {
        bail!(
            "Container tools manifest path must be relative: {}",
            path.display()
        );
    }
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            Component::Prefix(_)
            | Component::RootDir
            | Component::CurDir
            | Component::ParentDir => bail!(
                "Container tools manifest path must not escape the bundle: {}",
                path.display()
            ),
        }
    }
    Ok(())
}

fn validate_sha256_string(value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .chars()
            .all(|ch| ch.is_ascii_hexdigit() && !ch.is_ascii_uppercase())
    {
        bail!("Invalid sha256 value in container tools manifest: {value}");
    }
    Ok(())
}

fn validate_executable(path: &Path) -> Result<()> {
    let mode = fs::metadata(path)
        .with_context(|| format!("Failed to stat container tool artifact: {}", path.display()))?
        .permissions()
        .mode()
        & 0o777;
    if mode & 0o111 == 0 {
        bail!(
            "Container tool artifact is not executable: {}",
            path.display()
        );
    }
    Ok(())
}

fn expected_container_tool_set() -> BTreeSet<(String, String)> {
    REQUIRED_PLATFORM_IDS
        .iter()
        .flat_map(|platform| {
            REQUIRED_TOOLS
                .iter()
                .map(move |tool| (tool.file_name().to_owned(), (*platform).to_owned()))
        })
        .collect()
}

fn check_sha256sums(dir: &Path, manifest_sums: &BTreeMap<PathBuf, String>) -> Result<()> {
    let sums_path = dir.join("SHA256SUMS");
    let sums = fs::read_to_string(&sums_path)
        .with_context(|| format!("Failed to read {}", sums_path.display()))?;
    let mut parsed = BTreeMap::new();
    for (index, line) in sums.lines().enumerate() {
        if line.is_empty() {
            bail!("Invalid SHA256SUMS line {}: empty line", index + 1);
        }
        let Some((sha256, path)) = line.split_once("  ") else {
            bail!(
                "Invalid SHA256SUMS line {}: expected '<sha256><two spaces><path>'",
                index + 1
            );
        };
        validate_sha256_string(sha256)?;
        let path = PathBuf::from(path);
        validate_manifest_path(&path)?;
        if parsed.insert(path.clone(), sha256.to_owned()).is_some() {
            bail!("Duplicate path in SHA256SUMS: {}", path.display());
        }
    }
    if &parsed != manifest_sums {
        bail!("SHA256SUMS does not match container tools manifest");
    }
    Ok(())
}

fn verify_file_sha256(path: &Path, expected: &str) -> Result<()> {
    let mut file = fs::File::open(path).with_context(|| {
        format!(
            "Failed to read container tool artifact for checksum: {}",
            path.display()
        )
    })?;
    let actual = sha256_open_file(&mut file, path, "container tool artifact")?;
    if actual != expected {
        bail!(
            "Container tool artifact checksum mismatch: {}",
            path.display()
        );
    }
    Ok(())
}

fn verify_embedded_sha256(artifact: &EmbeddedContainerToolArtifact) -> Result<()> {
    verify_sha256(artifact.bytes, artifact.sha256, || {
        format!("embedded {} for {}", artifact.name, artifact.platform)
    })
}

fn verify_sha256(bytes: &[u8], expected: &str, display: impl FnOnce() -> String) -> Result<()> {
    let actual = hex_lower(&Sha256::digest(bytes));
    if actual != expected {
        bail!("Container tool artifact checksum mismatch: {}", display());
    }
    Ok(())
}

fn container_tool_override_dir() -> Option<PathBuf> {
    env::var_os(CONTAINER_TOOLS_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

enum ResolvedContainerTool<'a> {
    ExternalFile { path: PathBuf, sha256: String },
    Embedded(&'a EmbeddedContainerToolArtifact),
}

impl ResolvedContainerTool<'_> {
    fn sha256(&self) -> &str {
        match self {
            Self::ExternalFile { sha256, .. } => sha256,
            Self::Embedded(artifact) => artifact.sha256,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContainerToolsManifest {
    schema_version: u32,
    protocol_version: u32,
    tools: Vec<ContainerToolsManifestEntry>,
}

#[derive(Debug, Deserialize)]
struct ContainerToolsManifestEntry {
    name: String,
    platform: String,
    path: PathBuf,
    sha256: String,
}

struct ValidatedExternalArtifact {
    name: String,
    platform: String,
    path: PathBuf,
    sha256: String,
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::Write as _,
        os::unix::fs::{PermissionsExt, symlink},
        path::Path,
    };

    use sha2::{Digest, Sha256};
    use tempfile::TempDir;

    use super::{
        ContainerTool, ContainerToolPlatform, EmbeddedContainerToolArtifact,
        TestContainerToolEntry, atomic_stage_container_tool, create_private_staging_file,
        stage_container_tool_from_dir, stage_container_tool_with_embedded,
        write_test_container_tools_bundle,
    };

    static EMBEDDED_FORWARD_AGENT_AMD64: &[EmbeddedContainerToolArtifact] =
        &[EmbeddedContainerToolArtifact {
            name: "decune-forward-agent",
            platform: "linux-amd64",
            sha256: "9289140b1ac28dbda1437b283e6ca608e33186654e7d3a995da268c35906cd4c",
            bytes: b"embedded",
        }];
    static EMBEDDED_FORWARD_AGENT_ARM64: &[EmbeddedContainerToolArtifact] =
        &[EmbeddedContainerToolArtifact {
            name: "decune-forward-agent",
            platform: "linux-arm64",
            sha256: "9289140b1ac28dbda1437b283e6ca608e33186654e7d3a995da268c35906cd4c",
            bytes: b"embedded",
        }];

    #[test]
    fn stages_all_required_container_tool_names_from_complete_manifest() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let runtime = temp.path().join("runtime");
        fs::create_dir_all(&runtime).unwrap();
        write_test_container_tools_bundle(
            &source,
            &[TestContainerToolEntry {
                tool: ContainerTool::Decune,
                platform: ContainerToolPlatform::LinuxAmd64,
                contents: b"container-cli",
            }],
        )
        .unwrap();

        let staged = stage_container_tool_from_dir(
            ContainerTool::Decune,
            ContainerToolPlatform::LinuxAmd64,
            &runtime,
            &source,
        )
        .unwrap();

        assert_eq!(staged, runtime.join("decune"));
        assert_eq!(fs::read(runtime.join("decune")).unwrap(), b"container-cli");
        assert_eq!(
            fs::metadata(runtime.join("decune"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert_eq!(fs::read_dir(&runtime).unwrap().count(), 1);

        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(temp.path().join("source/manifest.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["tools"].as_array().unwrap().len(), 6);
    }

    #[test]
    fn stages_external_artifact_larger_than_checksum_buffer() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let runtime = temp.path().join("runtime");
        let contents = vec![b'x'; 16 * 1024 + 1];
        fs::create_dir_all(&runtime).unwrap();
        write_test_container_tools_bundle(
            &source,
            &[TestContainerToolEntry {
                tool: ContainerTool::Decune,
                platform: ContainerToolPlatform::LinuxAmd64,
                contents: &contents,
            }],
        )
        .unwrap();

        let staged = stage_container_tool_from_dir(
            ContainerTool::Decune,
            ContainerToolPlatform::LinuxAmd64,
            &runtime,
            &source,
        )
        .unwrap();

        assert_eq!(fs::read(staged).unwrap(), contents);
    }

    #[test]
    fn external_manifest_override_precedes_embedded_bundle() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let runtime = temp.path().join("runtime");
        fs::create_dir_all(&runtime).unwrap();
        fs::write(runtime.join("decune-forward-agent"), b"old").unwrap();
        write_test_container_tools_bundle(
            &source,
            &[TestContainerToolEntry {
                tool: ContainerTool::ForwardAgent,
                platform: ContainerToolPlatform::LinuxAmd64,
                contents: b"external",
            }],
        )
        .unwrap();

        let staged = stage_container_tool_with_embedded(
            ContainerTool::ForwardAgent,
            ContainerToolPlatform::LinuxAmd64,
            &runtime,
            Some(source.as_path()),
            EMBEDDED_FORWARD_AGENT_AMD64,
        )
        .unwrap();

        assert_eq!(staged, runtime.join("decune-forward-agent"));
        assert_eq!(
            fs::read(runtime.join("decune-forward-agent")).unwrap(),
            b"external"
        );
    }

    #[test]
    fn rejects_external_override_without_manifest() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let runtime = temp.path().join("runtime");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&runtime).unwrap();

        let error = stage_container_tool_from_dir(
            ContainerTool::ForwardAgent,
            ContainerToolPlatform::LinuxAmd64,
            &runtime,
            &source,
        )
        .unwrap_err();

        assert!(
            error.to_string().contains(
                "DECUNE_CONTAINER_TOOLS_DIR must point to a decune container tools bundle"
            )
        );
    }

    #[test]
    fn rejects_external_override_missing_required_entry() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let runtime = temp.path().join("runtime");
        fs::create_dir_all(&runtime).unwrap();
        write_test_container_tools_bundle(&source, &[]).unwrap();
        mutate_manifest(&source, |tools| {
            tools.retain(|entry| {
                entry["name"] != "decune-forward-agent" || entry["platform"] != "linux-amd64"
            });
        });

        let error = stage_container_tool_from_dir(
            ContainerTool::ForwardAgent,
            ContainerToolPlatform::LinuxAmd64,
            &runtime,
            &source,
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Missing required container tool artifacts")
        );
    }

    #[test]
    fn rejects_unknown_and_duplicate_manifest_entries() {
        let temp = TempDir::new().unwrap();
        let unknown_source = temp.path().join("unknown");
        let duplicate_source = temp.path().join("duplicate");
        let runtime = temp.path().join("runtime");
        fs::create_dir_all(&runtime).unwrap();
        write_test_container_tools_bundle(&unknown_source, &[]).unwrap();
        write_test_container_tools_bundle(&duplicate_source, &[]).unwrap();
        mutate_manifest(&unknown_source, |tools| {
            tools[0]["name"] = serde_json::Value::String("unknown".to_owned());
        });
        mutate_manifest(&duplicate_source, |tools| {
            tools.push(tools[0].clone());
        });

        let unknown = stage_container_tool_from_dir(
            ContainerTool::ForwardAgent,
            ContainerToolPlatform::LinuxAmd64,
            &runtime,
            &unknown_source,
        )
        .unwrap_err();
        let duplicate = stage_container_tool_from_dir(
            ContainerTool::ForwardAgent,
            ContainerToolPlatform::LinuxAmd64,
            &runtime,
            &duplicate_source,
        )
        .unwrap_err();

        assert!(
            unknown
                .to_string()
                .contains("Unexpected container tool artifact in manifest")
        );
        assert!(
            duplicate
                .to_string()
                .contains("Duplicate container tool artifact in manifest")
        );
    }

    #[test]
    fn rejects_duplicate_and_unsafe_manifest_paths() {
        let temp = TempDir::new().unwrap();
        let duplicate_source = temp.path().join("duplicate");
        let unsafe_source = temp.path().join("unsafe");
        let runtime = temp.path().join("runtime");
        fs::create_dir_all(&runtime).unwrap();
        write_test_container_tools_bundle(&duplicate_source, &[]).unwrap();
        write_test_container_tools_bundle(&unsafe_source, &[]).unwrap();
        mutate_manifest(&duplicate_source, |tools| {
            tools[1]["path"] = tools[0]["path"].clone();
        });
        mutate_manifest(&unsafe_source, |tools| {
            tools[0]["path"] = serde_json::Value::String("../outside".to_owned());
        });

        let duplicate = stage_container_tool_from_dir(
            ContainerTool::ForwardAgent,
            ContainerToolPlatform::LinuxAmd64,
            &runtime,
            &duplicate_source,
        )
        .unwrap_err();
        let unsafe_path = stage_container_tool_from_dir(
            ContainerTool::ForwardAgent,
            ContainerToolPlatform::LinuxAmd64,
            &runtime,
            &unsafe_source,
        )
        .unwrap_err();

        assert!(
            duplicate
                .to_string()
                .contains("Duplicate container tool artifact path in manifest")
        );
        assert!(
            unsafe_path
                .to_string()
                .contains("manifest path must not escape the bundle")
        );
    }

    #[test]
    fn stages_selected_container_tool_from_embedded_bundle() {
        let temp = TempDir::new().unwrap();
        let runtime = temp.path().join("runtime");
        fs::create_dir_all(&runtime).unwrap();
        fs::write(runtime.join("decune-forward-agent"), b"old").unwrap();

        let staged = stage_container_tool_with_embedded(
            ContainerTool::ForwardAgent,
            ContainerToolPlatform::LinuxArm64,
            &runtime,
            None,
            EMBEDDED_FORWARD_AGENT_ARM64,
        )
        .unwrap();

        assert_eq!(staged, runtime.join("decune-forward-agent"));
        assert_eq!(
            fs::read(runtime.join("decune-forward-agent")).unwrap(),
            b"embedded"
        );
    }

    #[test]
    fn rejects_manifest_checksum_mismatch() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let runtime = temp.path().join("runtime");
        fs::create_dir_all(&runtime).unwrap();
        write_test_container_tools_bundle(&source, &[]).unwrap();
        fs::write(source.join("linux-amd64/decune-forward-agent"), b"tampered").unwrap();

        let error = stage_container_tool_from_dir(
            ContainerTool::ForwardAgent,
            ContainerToolPlatform::LinuxAmd64,
            &runtime,
            &source,
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Container tool artifact checksum mismatch")
        );
    }

    #[test]
    fn rejects_sha256sums_mismatch() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let runtime = temp.path().join("runtime");
        fs::create_dir_all(&runtime).unwrap();
        write_test_container_tools_bundle(&source, &[]).unwrap();
        fs::write(
            source.join("SHA256SUMS"),
            "0000000000000000000000000000000000000000000000000000000000000000  linux-amd64/decune\n",
        )
        .unwrap();

        let error = stage_container_tool_from_dir(
            ContainerTool::ForwardAgent,
            ContainerToolPlatform::LinuxAmd64,
            &runtime,
            &source,
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("SHA256SUMS does not match container tools manifest")
        );
    }

    #[test]
    fn rejects_non_executable_manifest_artifact() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let runtime = temp.path().join("runtime");
        fs::create_dir_all(&runtime).unwrap();
        write_test_container_tools_bundle(&source, &[]).unwrap();
        fs::set_permissions(
            source.join("linux-amd64/decune-forward-agent"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();

        let error = stage_container_tool_from_dir(
            ContainerTool::ForwardAgent,
            ContainerToolPlatform::LinuxAmd64,
            &runtime,
            &source,
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Container tool artifact is not executable")
        );
    }

    #[test]
    fn ignores_predictable_temp_symlink_inside_mounted_runtime() {
        let temp = TempDir::new().unwrap();
        let runtime = temp.path().join("runtime");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&runtime).unwrap();
        fs::write(&outside, b"outside").unwrap();
        symlink(
            &outside,
            runtime.join(format!("decune-forward-agent.tmp-{}", std::process::id())),
        )
        .unwrap();
        stage_container_tool_with_embedded(
            ContainerTool::ForwardAgent,
            ContainerToolPlatform::LinuxAmd64,
            &runtime,
            None,
            EMBEDDED_FORWARD_AGENT_AMD64,
        )
        .unwrap();

        assert_eq!(fs::read(&outside).unwrap(), b"outside");
        assert!(
            runtime
                .join(format!("decune-forward-agent.tmp-{}", std::process::id()))
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn exclusive_private_temp_create_does_not_follow_existing_entries() {
        for symlink_entry in [false, true] {
            let temp = TempDir::new().unwrap();
            let prefix = ".decune-forward-agent.";
            let collision = temp.path().join(prefix);
            let outside = temp.path().join("outside");
            fs::write(&outside, b"outside").unwrap();
            if symlink_entry {
                symlink(&outside, &collision).unwrap();
            } else {
                fs::write(&collision, b"existing").unwrap();
            }

            assert!(create_private_staging_file(temp.path(), prefix, 0).is_err());
            assert_eq!(fs::read(&outside).unwrap(), b"outside");
            if symlink_entry {
                assert!(
                    collision
                        .symlink_metadata()
                        .unwrap()
                        .file_type()
                        .is_symlink()
                );
            } else {
                assert_eq!(fs::read(&collision).unwrap(), b"existing");
            }
        }
    }

    #[test]
    fn final_target_symlink_is_replaced_without_following_it() {
        let temp = TempDir::new().unwrap();
        let runtime = temp.path().join("runtime");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&runtime).unwrap();
        fs::write(&outside, b"outside").unwrap();
        symlink(&outside, runtime.join("decune-forward-agent")).unwrap();
        stage_container_tool_with_embedded(
            ContainerTool::ForwardAgent,
            ContainerToolPlatform::LinuxAmd64,
            &runtime,
            None,
            EMBEDDED_FORWARD_AGENT_AMD64,
        )
        .unwrap();

        assert_eq!(fs::read(&outside).unwrap(), b"outside");
        assert!(!runtime.join("decune-forward-agent").is_symlink());
        assert_eq!(
            fs::read(runtime.join("decune-forward-agent")).unwrap(),
            b"embedded"
        );
    }

    #[test]
    fn target_directory_is_reported_as_runtime_corruption() {
        let temp = TempDir::new().unwrap();
        let runtime = temp.path().join("runtime");
        fs::create_dir_all(runtime.join("decune-forward-agent")).unwrap();
        let error = stage_container_tool_with_embedded(
            ContainerTool::ForwardAgent,
            ContainerToolPlatform::LinuxAmd64,
            &runtime,
            None,
            EMBEDDED_FORWARD_AGENT_AMD64,
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("runtime path is not a regular file or symlink")
        );
        assert_private_staging_is_empty(temp.path());
    }

    #[test]
    fn write_checksum_and_rename_failures_leave_no_temp_or_partial_target() {
        let cases = ["write", "checksum", "rename"];
        for case in cases {
            let temp = TempDir::new().unwrap();
            let runtime = temp.path().join("runtime");
            let target = runtime.join("decune-forward-agent");
            fs::create_dir_all(&runtime).unwrap();
            let expected = sha256(b"complete");

            let result = atomic_stage_container_tool(&target, temp.path(), &expected, |file| {
                match case {
                    "write" => {
                        file.write_all(b"partial")?;
                        return Err(anyhow::anyhow!("injected write failure"));
                    }
                    "checksum" => file.write_all(b"tampered")?,
                    "rename" => {
                        file.write_all(b"complete")?;
                        fs::remove_dir(&runtime)?;
                    }
                    _ => unreachable!(),
                }
                Ok(())
            });

            assert!(result.is_err());
            assert!(!target.exists());
            assert_private_staging_is_empty(temp.path());
        }
    }

    #[test]
    fn resolves_supported_docker_platforms() {
        assert_eq!(
            ContainerToolPlatform::from_docker_os_arch("linux", "amd64").unwrap(),
            ContainerToolPlatform::LinuxAmd64
        );
        assert_eq!(
            ContainerToolPlatform::from_docker_os_arch("linux", "aarch64").unwrap(),
            ContainerToolPlatform::LinuxArm64
        );
        assert!(ContainerToolPlatform::from_docker_os_arch("linux", "arm").is_err());
    }

    fn mutate_manifest(source: &Path, mutate: impl FnOnce(&mut Vec<serde_json::Value>)) {
        let manifest_path = source.join("manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        mutate(manifest["tools"].as_array_mut().unwrap());
        fs::write(
            manifest_path,
            format!("{}\n", serde_json::to_string_pretty(&manifest).unwrap()),
        )
        .unwrap();
    }

    fn sha256(bytes: &[u8]) -> String {
        crate::hex::hex_lower(&Sha256::digest(bytes))
    }

    fn assert_private_staging_is_empty(parent: &Path) {
        let staging = fs::read_dir(parent)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".decune-forward-agent.")
            })
            .collect::<Vec<_>>();
        assert!(staging.is_empty(), "leftover staging entries: {staging:?}");
    }
}
