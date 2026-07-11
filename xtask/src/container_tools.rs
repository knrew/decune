use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    fs, io,
    os::unix::fs::PermissionsExt,
    path::{Component, Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tempfile::TempDir;

use crate::{
    hash::sha256_file,
    paths::{target_dir, workspace_relative},
};

const SCHEMA_VERSION: u32 = 1;
const PROTOCOL_VERSION: u32 = 1;
const TOOLS: [ContainerTool; 2] = [
    ContainerTool {
        name: "git-credential-decune",
    },
    ContainerTool {
        name: "decune-forward-agent",
    },
];
const PLATFORMS: [ContainerToolPlatform; 2] = [
    ContainerToolPlatform {
        id: "linux-amd64",
        rust_target: "x86_64-unknown-linux-musl",
    },
    ContainerToolPlatform {
        id: "linux-arm64",
        rust_target: "aarch64-unknown-linux-musl",
    },
];

pub(crate) fn build_container_tools(workspace: &Path, out: &Path, locked: bool) -> Result<()> {
    let out = workspace_relative(workspace, out);
    let temp_parent = out
        .parent()
        .map_or_else(|| workspace.to_path_buf(), Path::to_path_buf);
    fs::create_dir_all(&temp_parent).with_context(|| {
        format!(
            "Failed to create container tools output parent directory: {}",
            temp_parent.display()
        )
    })?;
    let temp = tempfile::Builder::new()
        .prefix("container-tools.")
        .tempdir_in(&temp_parent)
        .with_context(|| {
            format!(
                "Failed to create temporary container tools directory: {}",
                temp_parent.display()
            )
        })?;

    let mut entries = Vec::new();
    for platform in PLATFORMS {
        build_platform(workspace, platform, locked)?;
        let platform_dir = temp.path().join(platform.id);
        fs::create_dir_all(&platform_dir).with_context(|| {
            format!(
                "Failed to create container tools platform directory: {}",
                platform_dir.display()
            )
        })?;
        for tool in TOOLS {
            let source = target_dir(workspace)
                .join(platform.rust_target)
                .join("dist")
                .join(tool.name);
            if !source.is_file() {
                bail!(
                    "Missing container tool build artifact: {}. Ensure Rust target {} is installed.",
                    source.display(),
                    platform.rust_target
                );
            }
            let relative_path = PathBuf::from(platform.id).join(tool.name);
            let target = temp.path().join(&relative_path);
            fs::copy(&source, &target).with_context(|| {
                format!(
                    "Failed to copy container tool artifact: {} -> {}",
                    source.display(),
                    target.display()
                )
            })?;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).with_context(|| {
                format!(
                    "Failed to set container tool artifact permissions: {}",
                    target.display()
                )
            })?;
            let sha256 = sha256_file(&target)?;
            entries.push(ManifestEntry {
                name: tool.name.to_owned(),
                platform: platform.id.to_owned(),
                path: relative_path.to_string_lossy().into_owned(),
                sha256,
            });
        }
    }

    entries.sort_by(|left, right| {
        left.platform
            .cmp(&right.platform)
            .then_with(|| left.name.cmp(&right.name))
    });
    write_manifest_and_sums(temp.path(), entries)?;
    replace_dir(temp, &out)?;
    Ok(())
}

fn build_platform(workspace: &Path, platform: ContainerToolPlatform, locked: bool) -> Result<()> {
    let mut command = Command::new("cargo");
    command
        .current_dir(workspace)
        .arg("build")
        .arg("--profile")
        .arg("dist")
        .arg("--target")
        .arg(platform.rust_target)
        .arg("-p")
        .arg("decune-container-tools")
        .arg("--bins");
    if locked {
        command.arg("--locked");
    }
    if platform.rust_target == "aarch64-unknown-linux-musl" {
        command.env("CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER", "rust-lld");
    }
    let output = command.output().with_context(|| {
        format!(
            "Failed to run cargo build for container tools target {}",
            platform.rust_target
        )
    })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("can't find crate for `std`")
        || stderr.contains("target may not be installed")
        || stderr.contains("is not installed")
    {
        bail!(
            "Missing Rust target required to build decune container tools:\n  {}\n\nInstall release-build prerequisites with:\n  rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl",
            platform.rust_target
        );
    }
    bail!(
        "Failed to build decune container tools for {}.\nstdout:\n{}\nstderr:\n{}",
        platform.rust_target,
        String::from_utf8_lossy(&output.stdout),
        stderr
    );
}

pub(crate) fn check_container_tools(dir: &Path) -> Result<Manifest> {
    let manifest = read_manifest(dir)?;
    if manifest.schema_version != SCHEMA_VERSION {
        bail!(
            "Unsupported container tools manifest schemaVersion: {}",
            manifest.schema_version
        );
    }
    if manifest.protocol_version != PROTOCOL_VERSION {
        bail!(
            "Unsupported container tools protocolVersion: {}",
            manifest.protocol_version
        );
    }

    let expected = expected_container_tool_set();
    let mut seen = BTreeSet::new();
    let mut manifest_sums = BTreeMap::new();
    for entry in &manifest.tools {
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
        validate_manifest_path(Path::new(&entry.path))?;
        validate_sha256_string(&entry.sha256)?;
        if manifest_sums
            .insert(entry.path.clone(), entry.sha256.clone())
            .is_some()
        {
            bail!(
                "Duplicate container tool artifact path in manifest: {}",
                entry.path
            );
        }
        let path = dir.join(&entry.path);
        if !path.is_file() {
            bail!(
                "Container tools manifest entry does not exist: {}",
                path.display()
            );
        }
        let actual = sha256_file(&path)?;
        if actual != entry.sha256 {
            bail!(
                "Container tool artifact checksum mismatch: {}",
                path.display()
            );
        }
        let mode = fs::metadata(&path)
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
    }

    if seen != expected {
        let missing = expected.difference(&seen).cloned().collect::<Vec<_>>();
        bail!("Missing required container tool artifacts: {missing:?}");
    }
    check_sha256sums(dir, &manifest_sums)?;
    Ok(manifest)
}

pub(crate) fn prepare_xtask_container_tools_bundle(
    workspace: &Path,
    locked: bool,
) -> Result<PathBuf> {
    let bundle_dir = default_xtask_container_tools_bundle_dir(workspace);
    prepare_container_tools_bundle(workspace, &bundle_dir, locked)?;
    Ok(bundle_dir)
}

pub(crate) fn prepare_container_tools_bundle(
    workspace: &Path,
    bundle_dir: &Path,
    locked: bool,
) -> Result<()> {
    build_container_tools(workspace, bundle_dir, locked)?;
    check_container_tools(bundle_dir)?;
    Ok(())
}

pub(crate) fn default_xtask_container_tools_bundle_dir(workspace: &Path) -> PathBuf {
    target_dir(workspace)
        .join("decune-xtask")
        .join("container-tools-bundle")
}

pub(crate) fn resolve_container_tools_bundle_arg(workspace: &Path, path: Option<&Path>) -> PathBuf {
    path.map_or_else(
        || default_xtask_container_tools_bundle_dir(workspace),
        |path| workspace_relative(workspace, path),
    )
}

fn write_manifest_and_sums(dir: &Path, entries: Vec<ManifestEntry>) -> Result<()> {
    let manifest = Manifest {
        schema_version: SCHEMA_VERSION,
        protocol_version: PROTOCOL_VERSION,
        tools: entries,
    };
    let json = serde_json::to_string_pretty(&manifest)?;
    fs::write(dir.join("manifest.json"), format!("{json}\n"))
        .with_context(|| format!("Failed to write {}", dir.join("manifest.json").display()))?;

    let mut sums = String::new();
    for entry in &manifest.tools {
        writeln!(sums, "{}  {}", entry.sha256, entry.path)?;
    }
    fs::write(dir.join("SHA256SUMS"), sums)
        .with_context(|| format!("Failed to write {}", dir.join("SHA256SUMS").display()))
}

fn read_manifest(dir: &Path) -> Result<Manifest> {
    let manifest_path = dir.join("manifest.json");
    let bytes = fs::read(&manifest_path).with_context(|| {
        format!(
            "Failed to read container tools manifest: {}",
            manifest_path.display()
        )
    })?;
    serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "Failed to parse container tools manifest: {}",
            manifest_path.display()
        )
    })
}

fn replace_dir(mut temp: TempDir, target: &Path) -> Result<()> {
    let staging_path = temp.path().to_path_buf();
    match fs::remove_dir_all(target) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "Failed to remove existing container tools directory: {}",
                    target.display()
                )
            });
        }
    }
    fs::rename(&staging_path, target).with_context(|| {
        format!(
            "Failed to replace container tools directory: {} -> {}",
            staging_path.display(),
            target.display()
        )
    })?;
    temp.disable_cleanup(true);
    Ok(())
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

fn expected_container_tool_set() -> BTreeSet<(String, String)> {
    PLATFORMS
        .iter()
        .flat_map(|platform| {
            TOOLS
                .iter()
                .map(move |tool| (tool.name.to_owned(), platform.id.to_owned()))
        })
        .collect()
}

fn check_sha256sums(dir: &Path, manifest_sums: &BTreeMap<String, String>) -> Result<()> {
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
        validate_manifest_path(Path::new(path))?;
        if parsed.insert(path.to_owned(), sha256.to_owned()).is_some() {
            bail!("Duplicate path in SHA256SUMS: {path}");
        }
    }
    if &parsed != manifest_sums {
        bail!("SHA256SUMS does not match container tools manifest");
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct ContainerTool {
    name: &'static str,
}

#[derive(Debug, Clone, Copy)]
struct ContainerToolPlatform {
    id: &'static str,
    rust_target: &'static str,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Manifest {
    schema_version: u32,
    protocol_version: u32,
    tools: Vec<ManifestEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManifestEntry {
    name: String,
    platform: String,
    path: String,
    sha256: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    use tempfile::TempDir;

    #[test]
    fn required_platform_mapping_is_stable() {
        assert_eq!(PLATFORMS[0].id, "linux-amd64");
        assert_eq!(PLATFORMS[0].rust_target, "x86_64-unknown-linux-musl");
        assert_eq!(PLATFORMS[1].id, "linux-arm64");
        assert_eq!(PLATFORMS[1].rust_target, "aarch64-unknown-linux-musl");
    }

    #[test]
    fn replace_dir_promotes_staging_directory() {
        let parent = TempDir::new().unwrap();
        let staging = TempDir::new_in(parent.path()).unwrap();
        let staging_path = staging.path().to_path_buf();
        fs::write(staging.path().join("new"), "new").unwrap();
        let target = parent.path().join("target");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("old"), "old").unwrap();

        replace_dir(staging, &target).unwrap();

        assert!(!staging_path.exists());
        assert!(!target.join("old").exists());
        assert_eq!(fs::read_to_string(target.join("new")).unwrap(), "new");
    }

    #[test]
    fn replace_dir_cleans_staging_when_target_removal_fails() {
        let parent = TempDir::new().unwrap();
        let staging = TempDir::new_in(parent.path()).unwrap();
        let staging_path = staging.path().to_path_buf();
        let target = parent.path().join("target");
        fs::write(&target, "not a directory").unwrap();

        let error = replace_dir(staging, &target).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Failed to remove existing container tools directory")
        );
        assert!(!staging_path.exists());
        assert!(target.is_file());
    }

    #[test]
    fn replace_dir_cleans_staging_when_rename_fails() {
        let parent = TempDir::new().unwrap();
        let staging = TempDir::new_in(parent.path()).unwrap();
        let staging_path = staging.path().to_path_buf();
        let target = parent.path().join("missing/target");

        let error = replace_dir(staging, &target).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Failed to replace container tools directory")
        );
        assert!(!staging_path.exists());
        assert!(!target.exists());
    }

    #[test]
    fn rejects_manifest_path_traversal() {
        assert!(validate_manifest_path(Path::new("../tool")).is_err());
        assert!(validate_manifest_path(Path::new("/tmp/tool")).is_err());
        assert!(validate_manifest_path(Path::new("linux-amd64/tool")).is_ok());
    }

    #[test]
    fn check_container_tools_accepts_valid_bundle() {
        let temp = TempDir::new().unwrap();
        let entries = create_container_tool_files(temp.path());
        write_manifest_and_sums(temp.path(), entries).unwrap();

        check_container_tools(temp.path()).unwrap();
    }

    #[test]
    fn check_container_tools_rejects_unknown_tool() {
        let temp = TempDir::new().unwrap();
        let mut entries = create_container_tool_files(temp.path());
        entries[0].name = "unknown-tool".to_owned();
        write_manifest_and_sums(temp.path(), entries).unwrap();

        let error = check_container_tools(temp.path()).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Unexpected container tool artifact in manifest")
        );
    }

    #[test]
    fn check_container_tools_rejects_unknown_platform() {
        let temp = TempDir::new().unwrap();
        let mut entries = create_container_tool_files(temp.path());
        entries[0].platform = "linux-s390x".to_owned();
        write_manifest_and_sums(temp.path(), entries).unwrap();

        let error = check_container_tools(temp.path()).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Unexpected container tool artifact in manifest")
        );
    }

    #[test]
    fn check_container_tools_rejects_duplicate_entry() {
        let temp = TempDir::new().unwrap();
        let mut entries = create_container_tool_files(temp.path());
        entries[1].name = entries[0].name.clone();
        entries[1].platform = entries[0].platform.clone();
        write_manifest_and_sums(temp.path(), entries).unwrap();

        let error = check_container_tools(temp.path()).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Duplicate container tool artifact in manifest")
        );
    }

    #[test]
    fn check_container_tools_rejects_missing_required_entry() {
        let temp = TempDir::new().unwrap();
        let mut entries = create_container_tool_files(temp.path());
        entries.pop();
        write_manifest_and_sums(temp.path(), entries).unwrap();

        let error = check_container_tools(temp.path()).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Missing required container tool artifacts")
        );
    }

    #[test]
    fn check_container_tools_rejects_invalid_sha256_format() {
        let temp = TempDir::new().unwrap();
        let mut entries = create_container_tool_files(temp.path());
        entries[0].sha256 = "NOT-A-SHA256".to_owned();
        write_manifest_and_sums(temp.path(), entries).unwrap();

        let error = check_container_tools(temp.path()).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Invalid sha256 value in container tools manifest")
        );
    }

    #[test]
    fn check_container_tools_rejects_sha256sums_mismatch() {
        let temp = TempDir::new().unwrap();
        let entries = create_container_tool_files(temp.path());
        write_manifest_and_sums(temp.path(), entries).unwrap();
        fs::write(
            temp.path().join("SHA256SUMS"),
            "0000000000000000000000000000000000000000000000000000000000000000  linux-amd64/git-credential-decune\n",
        )
        .unwrap();

        let error = check_container_tools(temp.path()).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("SHA256SUMS does not match container tools manifest")
        );
    }

    #[test]
    fn check_container_tools_rejects_missing_sha256sums() {
        let temp = TempDir::new().unwrap();
        let entries = create_container_tool_files(temp.path());
        let manifest = Manifest {
            schema_version: SCHEMA_VERSION,
            protocol_version: PROTOCOL_VERSION,
            tools: entries,
        };
        fs::write(
            temp.path().join("manifest.json"),
            format!("{}\n", serde_json::to_string_pretty(&manifest).unwrap()),
        )
        .unwrap();

        let error = check_container_tools(temp.path()).unwrap_err();

        assert!(error.to_string().contains("Failed to read"));
    }

    fn create_container_tool_files(dir: &Path) -> Vec<ManifestEntry> {
        let mut entries = Vec::new();
        for platform in PLATFORMS {
            fs::create_dir_all(dir.join(platform.id)).unwrap();
            for tool in TOOLS {
                let relative_path = PathBuf::from(platform.id).join(tool.name);
                let path = dir.join(&relative_path);
                fs::write(
                    &path,
                    format!("{} for {}", tool.name, platform.id).as_bytes(),
                )
                .unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
                entries.push(ManifestEntry {
                    name: tool.name.to_owned(),
                    platform: platform.id.to_owned(),
                    path: relative_path.to_string_lossy().into_owned(),
                    sha256: sha256_file(&path).unwrap(),
                });
            }
        }
        entries
    }
}
