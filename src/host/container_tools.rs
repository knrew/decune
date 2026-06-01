use std::{
    env, fs, io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
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

    fn display_name(self) -> &'static str {
        match self {
            Self::GitCredentialHelper => "Git credential helper",
            Self::ForwardAgent => "port forwarding agent",
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
    pub(crate) const ALL: [Self; 2] = [Self::LinuxAmd64, Self::LinuxArm64];

    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::LinuxAmd64 => "linux-amd64",
            Self::LinuxArm64 => "linux-arm64",
        }
    }
}

pub(crate) fn staged_tool_name(tool: ContainerTool, platform: ContainerToolPlatform) -> String {
    format!("{}-{}", tool.file_name(), platform.id())
}

pub(crate) fn stage_container_tool_variants(
    tool: ContainerTool,
    runtime_dir: &Path,
) -> Result<Vec<PathBuf>> {
    stage_container_tool_variants_from_dirs(tool, runtime_dir, container_tool_source_dirs())
}

pub(crate) fn stage_container_tool_variants_from_dirs(
    tool: ContainerTool,
    runtime_dir: &Path,
    source_dirs: Vec<PathBuf>,
) -> Result<Vec<PathBuf>> {
    let mut staged = Vec::new();
    for platform in ContainerToolPlatform::ALL {
        let target = runtime_dir.join(staged_tool_name(tool, platform));
        let Some(source) = resolve_container_tool(tool, platform, &source_dirs)? else {
            remove_stale_container_tool(&target, tool)?;
            continue;
        };
        fs::copy(&source, &target).with_context(|| {
            format!(
                "Failed to stage {} artifact: {} -> {}",
                tool.display_name(),
                source.display(),
                target.display()
            )
        })?;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).with_context(|| {
            format!(
                "Failed to set {} artifact permissions: {}",
                tool.display_name(),
                target.display()
            )
        })?;
        staged.push(target);
    }

    Ok(staged)
}

fn remove_stale_container_tool(path: &Path, tool: ContainerTool) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| {
            format!(
                "Failed to remove stale {} artifact: {}",
                tool.display_name(),
                path.display()
            )
        }),
    }
}

fn resolve_container_tool(
    tool: ContainerTool,
    platform: ContainerToolPlatform,
    source_dirs: &[PathBuf],
) -> Result<Option<PathBuf>> {
    for source_dir in source_dirs {
        match resolve_container_tool_from_manifest(tool, platform, source_dir)? {
            ManifestLookup::Found(path) => return Ok(Some(path)),
            ManifestLookup::MissingEntry => continue,
            ManifestLookup::NoManifest => {}
        }
        let candidate = source_dir.join(platform.id()).join(tool.file_name());
        if candidate.is_file() {
            return Ok(Some(candidate));
        }
    }

    Ok(None)
}

fn resolve_container_tool_from_manifest(
    tool: ContainerTool,
    platform: ContainerToolPlatform,
    source_dir: &Path,
) -> Result<ManifestLookup> {
    let manifest_path = source_dir.join("manifest.json");
    let manifest = match fs::read_to_string(&manifest_path) {
        Ok(manifest) => manifest,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
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
    let Some(entry) = manifest
        .tools
        .iter()
        .find(|entry| entry.name == tool.file_name() && entry.platform == platform)
    else {
        return Ok(ManifestLookup::MissingEntry);
    };
    let path = source_dir.join(&entry.path);
    if !path.is_file() {
        bail!(
            "Container tools manifest entry does not exist: {}",
            path.display()
        );
    }
    verify_sha256(&path, &entry.sha256)?;
    Ok(ManifestLookup::Found(path))
}

#[derive(Debug)]
enum ManifestLookup {
    NoManifest,
    MissingEntry,
    Found(PathBuf),
}

fn verify_sha256(path: &Path, expected: &str) -> Result<()> {
    let bytes = fs::read(path).with_context(|| {
        format!(
            "Failed to read container tool artifact for checksum: {}",
            path.display()
        )
    })?;
    let actual = hex_lower(Sha256::digest(&bytes).as_slice());
    if actual != expected {
        bail!(
            "Container tool artifact checksum mismatch: {}",
            path.display()
        );
    }
    Ok(())
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn container_tool_source_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(path) = env::var_os(CONTAINER_TOOLS_ENV).filter(|value| !value.is_empty()) {
        dirs.push(PathBuf::from(path));
    }
    if let Some(path) = installed_container_tools_dir() {
        dirs.push(path);
    }
    dirs.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("target/container-tools"));
    dedupe_paths(dirs)
}

fn installed_container_tools_dir() -> Option<PathBuf> {
    let exe = env::current_exe().ok()?;
    let bin_dir = exe.parent()?;
    if bin_dir.file_name().and_then(|name| name.to_str()) != Some("bin") {
        return None;
    }
    Some(
        bin_dir
            .parent()?
            .join("libexec")
            .join("decune")
            .join("container-tools"),
    )
}

fn dedupe_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut deduped = Vec::new();
    for path in paths {
        if !deduped.iter().any(|existing| existing == &path) {
            deduped.push(path);
        }
    }
    deduped
}

#[derive(Debug, Deserialize)]
struct ContainerToolsManifest {
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
        ContainerTool, ContainerToolPlatform, stage_container_tool_variants_from_dirs,
        staged_tool_name,
    };

    #[test]
    fn stages_available_container_tool_variants_from_manifest() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let runtime = temp.path().join("runtime");
        fs::create_dir_all(source.join("linux-amd64")).unwrap();
        fs::create_dir_all(&runtime).unwrap();
        fs::write(source.join("linux-amd64/git-credential-decune"), b"helper").unwrap();
        fs::write(
            source.join("manifest.json"),
            r#"{"tools":[{"name":"git-credential-decune","platform":"linux-amd64","path":"linux-amd64/git-credential-decune","sha256":"e81d3b0e9d82feaaf5f6e55bdff24731d7eee08632ffa63801e6397290c5d20a"}]}"#,
        )
        .unwrap();

        let staged = stage_container_tool_variants_from_dirs(
            ContainerTool::GitCredentialHelper,
            &runtime,
            vec![source],
        )
        .unwrap();

        assert_eq!(
            staged,
            vec![runtime.join("git-credential-decune-linux-amd64")]
        );
        assert_eq!(
            fs::read(runtime.join("git-credential-decune-linux-amd64")).unwrap(),
            b"helper"
        );
    }

    #[test]
    fn stage_name_uses_stable_platform_suffix() {
        assert_eq!(
            staged_tool_name(
                ContainerTool::ForwardAgent,
                ContainerToolPlatform::LinuxArm64
            ),
            "decune-forward-agent-linux-arm64"
        );
    }

    #[test]
    fn removes_stale_container_tool_variant_when_source_is_missing() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let runtime = temp.path().join("runtime");
        fs::create_dir_all(source.join("linux-amd64")).unwrap();
        fs::create_dir_all(&runtime).unwrap();
        fs::write(source.join("linux-amd64/decune-forward-agent"), b"agent").unwrap();
        fs::write(
            runtime.join("decune-forward-agent-linux-arm64"),
            b"stale agent",
        )
        .unwrap();
        fs::write(runtime.join("unrelated"), b"keep").unwrap();

        let staged = stage_container_tool_variants_from_dirs(
            ContainerTool::ForwardAgent,
            &runtime,
            vec![source],
        )
        .unwrap();

        assert_eq!(
            staged,
            vec![runtime.join("decune-forward-agent-linux-amd64")]
        );
        assert_eq!(
            fs::read(runtime.join("decune-forward-agent-linux-amd64")).unwrap(),
            b"agent"
        );
        assert!(!runtime.join("decune-forward-agent-linux-arm64").exists());
        assert_eq!(fs::read(runtime.join("unrelated")).unwrap(), b"keep");
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
            r#"{"tools":[{"name":"decune-forward-agent","platform":"linux-amd64","path":"linux-amd64/decune-forward-agent","sha256":"0000000000000000000000000000000000000000000000000000000000000000"}]}"#,
        )
        .unwrap();

        let error = stage_container_tool_variants_from_dirs(
            ContainerTool::ForwardAgent,
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
}
