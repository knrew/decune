use std::{
    env, fs,
    os::unix::fs::PermissionsExt,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};

pub(crate) const CONTAINER_TOOLS_ENV: &str = "DECUNE_CONTAINER_TOOLS_DIR";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContainerTool {
    GitCredentialHelper,
    ForwardAgent,
}

impl ContainerTool {
    pub(crate) fn file_name(self) -> &'static str {
        match self {
            Self::GitCredentialHelper => "git-credential-decune",
            Self::ForwardAgent => "decune-forward-agent",
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
    pub(crate) fn id(self) -> &'static str {
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
    stage_container_tool_from_sources(
        tool,
        platform,
        runtime_dir,
        &container_tool_override_dirs(),
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
];

pub(crate) fn stage_container_tool_from_dirs(
    tool: ContainerTool,
    platform: ContainerToolPlatform,
    runtime_dir: &Path,
    source_dirs: Vec<PathBuf>,
) -> Result<PathBuf> {
    stage_container_tool_from_sources(tool, platform, runtime_dir, &source_dirs, &[])
}

#[cfg(test)]
fn stage_container_tool_with_embedded(
    tool: ContainerTool,
    platform: ContainerToolPlatform,
    runtime_dir: &Path,
    source_dirs: &[PathBuf],
    embedded: &'static [EmbeddedContainerToolArtifact],
) -> Result<PathBuf> {
    stage_container_tool_from_sources(tool, platform, runtime_dir, source_dirs, embedded)
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

    let mut manifest_entries = Vec::with_capacity(entries.len());
    for entry in entries {
        let relative_path = PathBuf::from(entry.platform.id()).join(entry.tool.file_name());
        let artifact_path = source_dir.join(&relative_path);
        if let Some(parent) = artifact_path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "Failed to create test container tools artifact dir: {}",
                    parent.display()
                )
            })?;
        }
        fs::write(&artifact_path, entry.contents).with_context(|| {
            format!(
                "Failed to write test container tools artifact: {}",
                artifact_path.display()
            )
        })?;
        manifest_entries.push(serde_json::json!({
            "name": entry.tool.file_name(),
            "platform": entry.platform.id(),
            "path": relative_path.display().to_string(),
            "sha256": hex_lower(&Sha256::digest(entry.contents)),
        }));
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
    })
}

fn stage_container_tool_from_sources(
    tool: ContainerTool,
    platform: ContainerToolPlatform,
    runtime_dir: &Path,
    source_dirs: &[PathBuf],
    embedded: &'static [EmbeddedContainerToolArtifact],
) -> Result<PathBuf> {
    let target = runtime_dir.join(tool.file_name());
    match resolve_container_tool(tool, platform, source_dirs, embedded)? {
        ResolvedContainerTool::ExternalFile(source) => {
            fs::copy(&source, &target).with_context(|| {
                format!(
                    "Failed to stage decune container tool artifact: {} -> {}",
                    source.display(),
                    target.display()
                )
            })?;
        }
        ResolvedContainerTool::Embedded(artifact) => {
            fs::write(&target, artifact.bytes).with_context(|| {
                format!(
                    "Failed to stage embedded decune container tool artifact: {}",
                    target.display()
                )
            })?;
        }
    }
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).with_context(|| {
        format!(
            "Failed to set decune container tool artifact permissions: {}",
            target.display()
        )
    })?;
    Ok(target)
}

fn resolve_container_tool(
    tool: ContainerTool,
    platform: ContainerToolPlatform,
    source_dirs: &[PathBuf],
    embedded: &'static [EmbeddedContainerToolArtifact],
) -> Result<ResolvedContainerTool<'static>> {
    for source_dir in source_dirs {
        if let Some(path) = resolve_external_container_tool(tool, platform, source_dir)? {
            return Ok(ResolvedContainerTool::ExternalFile(path));
        }
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
) -> Result<Option<PathBuf>> {
    match resolve_container_tool_from_manifest(tool, platform, source_dir)? {
        ManifestLookup::Found(path) => Ok(Some(path)),
        ManifestLookup::MissingEntry => {
            bail!(
                "DECUNE_CONTAINER_TOOLS_DIR bundle is missing required decune container tool artifact: {} for {}",
                tool.file_name(),
                platform.id()
            );
        }
        ManifestLookup::NoManifest => {
            bail!(
                "DECUNE_CONTAINER_TOOLS_DIR must point to a decune container tools bundle with manifest.json: {}",
                source_dir.display()
            );
        }
    }
}

fn resolve_container_tool_from_manifest(
    tool: ContainerTool,
    platform: ContainerToolPlatform,
    source_dir: &Path,
) -> Result<ManifestLookup> {
    let manifest_path = source_dir.join("manifest.json");
    let manifest = match fs::read_to_string(&manifest_path) {
        Ok(manifest) => manifest,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ManifestLookup::NoManifest);
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
    let Some(entry) = manifest
        .tools
        .iter()
        .find(|entry| entry.name == tool.file_name() && entry.platform == platform)
    else {
        return Ok(ManifestLookup::MissingEntry);
    };
    validate_manifest_path(&entry.path)?;
    let path = source_dir.join(&entry.path);
    if !path.is_file() {
        bail!(
            "Container tools manifest entry does not exist: {}",
            path.display()
        );
    }
    verify_file_sha256(&path, &entry.sha256)?;
    Ok(ManifestLookup::Found(path))
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
            _ => bail!(
                "Container tools manifest path must not escape the bundle: {}",
                path.display()
            ),
        }
    }
    Ok(())
}

fn verify_file_sha256(path: &Path, expected: &str) -> Result<()> {
    let bytes = fs::read(path).with_context(|| {
        format!(
            "Failed to read container tool artifact for checksum: {}",
            path.display()
        )
    })?;
    verify_sha256(&bytes, expected, || path.display().to_string())
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

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn container_tool_override_dirs() -> Vec<PathBuf> {
    env::var_os(CONTAINER_TOOLS_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .into_iter()
        .collect()
}

enum ResolvedContainerTool<'a> {
    ExternalFile(PathBuf),
    Embedded(&'a EmbeddedContainerToolArtifact),
}

#[derive(Debug)]
enum ManifestLookup {
    NoManifest,
    MissingEntry,
    Found(PathBuf),
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
    platform: ContainerToolPlatform,
    path: PathBuf,
    sha256: String,
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::{
        ContainerTool, ContainerToolPlatform, EmbeddedContainerToolArtifact,
        stage_container_tool_from_dirs, stage_container_tool_with_embedded,
    };

    #[test]
    fn stages_selected_container_tool_from_manifest() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let runtime = temp.path().join("runtime");
        fs::create_dir_all(source.join("linux-amd64")).unwrap();
        fs::create_dir_all(source.join("linux-arm64")).unwrap();
        fs::create_dir_all(&runtime).unwrap();
        fs::write(source.join("linux-amd64/git-credential-decune"), b"helper").unwrap();
        fs::write(source.join("linux-arm64/git-credential-decune"), b"other").unwrap();
        fs::write(
            source.join("manifest.json"),
            r#"{"schemaVersion":1,"protocolVersion":1,"tools":[{"name":"git-credential-decune","platform":"linux-amd64","path":"linux-amd64/git-credential-decune","sha256":"e81d3b0e9d82feaaf5f6e55bdff24731d7eee08632ffa63801e6397290c5d20a"},{"name":"git-credential-decune","platform":"linux-arm64","path":"linux-arm64/git-credential-decune","sha256":"d9298a10d1b073fe878bf79259c4b97b86767c27e853ca856b9cdf34f1581d90"}]}"#,
        )
        .unwrap();

        let staged = stage_container_tool_from_dirs(
            ContainerTool::GitCredentialHelper,
            ContainerToolPlatform::LinuxAmd64,
            &runtime,
            vec![source],
        )
        .unwrap();

        assert_eq!(staged, runtime.join("git-credential-decune"));
        assert_eq!(
            fs::read(runtime.join("git-credential-decune")).unwrap(),
            b"helper"
        );
        assert_eq!(fs::read_dir(&runtime).unwrap().count(), 1);
    }

    #[test]
    fn external_manifest_override_precedes_embedded_bundle() {
        static EMBEDDED: &[EmbeddedContainerToolArtifact] = &[EmbeddedContainerToolArtifact {
            name: "decune-forward-agent",
            platform: "linux-amd64",
            sha256: "9289140b1ac28dbda1437b283e6ca608e33186654e7d3a995da268c35906cd4c",
            bytes: b"embedded",
        }];

        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let runtime = temp.path().join("runtime");
        fs::create_dir_all(source.join("linux-amd64")).unwrap();
        fs::create_dir_all(&runtime).unwrap();
        fs::write(source.join("linux-amd64/decune-forward-agent"), b"external").unwrap();
        fs::write(
            source.join("manifest.json"),
            r#"{"schemaVersion":1,"protocolVersion":1,"tools":[{"name":"decune-forward-agent","platform":"linux-amd64","path":"linux-amd64/decune-forward-agent","sha256":"3c4623849a49a53911c4a3e48d8cead8a1858960bccdea7a1b978d73ec2f06d7"}]}"#,
        )
        .unwrap();

        let staged = stage_container_tool_with_embedded(
            ContainerTool::ForwardAgent,
            ContainerToolPlatform::LinuxAmd64,
            &runtime,
            &[source],
            EMBEDDED,
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
        fs::create_dir_all(source.join("linux-amd64")).unwrap();
        fs::create_dir_all(&runtime).unwrap();
        fs::write(source.join("linux-amd64/decune-forward-agent"), b"external").unwrap();

        let error = stage_container_tool_from_dirs(
            ContainerTool::ForwardAgent,
            ContainerToolPlatform::LinuxAmd64,
            &runtime,
            vec![source],
        )
        .unwrap_err();

        assert!(
            error.to_string().contains(
                "DECUNE_CONTAINER_TOOLS_DIR must point to a decune container tools bundle"
            )
        );
    }

    #[test]
    fn rejects_external_override_missing_requested_entry() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let runtime = temp.path().join("runtime");
        fs::create_dir_all(source.join("linux-amd64")).unwrap();
        fs::create_dir_all(&runtime).unwrap();
        fs::write(source.join("linux-amd64/git-credential-decune"), b"helper").unwrap();
        fs::write(
            source.join("manifest.json"),
            r#"{"schemaVersion":1,"protocolVersion":1,"tools":[{"name":"git-credential-decune","platform":"linux-amd64","path":"linux-amd64/git-credential-decune","sha256":"e81d3b0e9d82feaaf5f6e55bdff24731d7eee08632ffa63801e6397290c5d20a"}]}"#,
        )
        .unwrap();

        let error = stage_container_tool_from_dirs(
            ContainerTool::ForwardAgent,
            ContainerToolPlatform::LinuxAmd64,
            &runtime,
            vec![source],
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("bundle is missing required decune container tool artifact")
        );
    }

    #[test]
    fn stages_selected_container_tool_from_embedded_bundle() {
        static EMBEDDED: &[EmbeddedContainerToolArtifact] = &[EmbeddedContainerToolArtifact {
            name: "decune-forward-agent",
            platform: "linux-arm64",
            sha256: "9289140b1ac28dbda1437b283e6ca608e33186654e7d3a995da268c35906cd4c",
            bytes: b"embedded",
        }];
        let temp = TempDir::new().unwrap();
        let runtime = temp.path().join("runtime");
        fs::create_dir_all(&runtime).unwrap();

        let staged = stage_container_tool_with_embedded(
            ContainerTool::ForwardAgent,
            ContainerToolPlatform::LinuxArm64,
            &runtime,
            &[],
            EMBEDDED,
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
        fs::create_dir_all(source.join("linux-amd64")).unwrap();
        fs::create_dir_all(&runtime).unwrap();
        fs::write(source.join("linux-amd64/decune-forward-agent"), b"agent").unwrap();
        fs::write(
            source.join("manifest.json"),
            r#"{"schemaVersion":1,"protocolVersion":1,"tools":[{"name":"decune-forward-agent","platform":"linux-amd64","path":"linux-amd64/decune-forward-agent","sha256":"0000000000000000000000000000000000000000000000000000000000000000"}]}"#,
        )
        .unwrap();

        let error = stage_container_tool_from_dirs(
            ContainerTool::ForwardAgent,
            ContainerToolPlatform::LinuxAmd64,
            &runtime,
            vec![source],
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Container tool artifact checksum mismatch")
        );
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
}
