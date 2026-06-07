use std::{
    collections::BTreeSet,
    env, fs, io,
    os::unix::fs::PermissionsExt,
    path::{Component, Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use flate2::{Compression, write::GzEncoder};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tar::Builder;
use tempfile::TempDir;

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

#[derive(Debug, Parser)]
#[command(author, version, about)]
struct Args {
    #[command(subcommand)]
    command: XtaskCommand,
}

#[derive(Debug, Subcommand)]
enum XtaskCommand {
    BuildContainerTools {
        #[arg(long, default_value = "assets/container-tools")]
        out: PathBuf,
        #[arg(long)]
        locked: bool,
    },
    CheckContainerTools {
        #[arg(long, default_value = "assets/container-tools")]
        dir: PathBuf,
    },
    Dist {
        #[arg(long)]
        target: String,
        #[arg(long)]
        version: String,
        #[arg(long)]
        locked: bool,
        #[arg(long)]
        dist_dir: Option<PathBuf>,
    },
    Checksum {
        #[arg(long)]
        dist_dir: Option<PathBuf>,
    },
    ReleaseManifest {
        #[arg(long)]
        dist_dir: Option<PathBuf>,
        #[arg(long)]
        version: String,
    },
    ReleasePreflight {
        #[arg(long)]
        tag: String,
        #[arg(long)]
        version: String,
    },
}

fn main() -> Result<()> {
    let args = Args::parse();
    let workspace = workspace_root()?;
    match args.command {
        XtaskCommand::BuildContainerTools { out, locked } => {
            build_container_tools(&workspace, &out, locked)
        }
        XtaskCommand::CheckContainerTools { dir } => {
            check_container_tools(&workspace_relative(&workspace, &dir)?)?;
            Ok(())
        }
        XtaskCommand::Dist {
            target,
            version,
            locked,
            dist_dir,
        } => dist(&workspace, &target, &version, locked, dist_dir.as_deref()),
        XtaskCommand::Checksum { dist_dir } => {
            checksum(&resolve_dist_dir(&workspace, dist_dir.as_deref())?)
        }
        XtaskCommand::ReleaseManifest { dist_dir, version } => release_manifest(
            &resolve_dist_dir(&workspace, dist_dir.as_deref())?,
            &version,
        ),
        XtaskCommand::ReleasePreflight { tag, version } => {
            release_preflight(&workspace, &tag, &version)
        }
    }
}

fn build_container_tools(workspace: &Path, out: &Path, locked: bool) -> Result<()> {
    let out = workspace_relative(workspace, out)?;
    let temp_parent = out
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| workspace.to_path_buf());
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

fn check_container_tools(dir: &Path) -> Result<Manifest> {
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

    let mut coverage = BTreeSet::new();
    for entry in &manifest.tools {
        validate_manifest_path(Path::new(&entry.path))?;
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
        coverage.insert((entry.name.clone(), entry.platform.clone()));
    }

    for platform in PLATFORMS {
        for tool in TOOLS {
            if !coverage.contains(&(tool.name.to_owned(), platform.id.to_owned())) {
                bail!(
                    "Missing required container tool artifact: {} for {}",
                    tool.name,
                    platform.id
                );
            }
        }
    }
    Ok(manifest)
}

fn dist(
    workspace: &Path,
    target: &str,
    version: &str,
    locked: bool,
    dist_dir: Option<&Path>,
) -> Result<()> {
    let bundle_dir = env::var_os("DECUNE_CONTAINER_TOOLS_BUNDLE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.join("assets/container-tools"));
    let bundle_dir = workspace_relative(workspace, &bundle_dir)?;
    check_container_tools(&bundle_dir)?;

    let mut command = Command::new("cargo");
    command
        .current_dir(workspace)
        .env("DECUNE_CONTAINER_TOOLS_BUNDLE", "required")
        .env("DECUNE_CONTAINER_TOOLS_BUNDLE_DIR", &bundle_dir)
        .arg("build")
        .arg("--profile")
        .arg("dist")
        .arg("--target")
        .arg(target)
        .arg("-p")
        .arg("decune");
    if locked {
        command.arg("--locked");
    }
    run_command(command, "Failed to build decune release binary")?;

    let binary = target_dir(workspace)
        .join(target)
        .join("dist")
        .join("decune");
    if !binary.is_file() {
        bail!("Missing decune release binary: {}", binary.display());
    }

    let dist_dir = resolve_dist_dir(workspace, dist_dir)?;
    fs::create_dir_all(&dist_dir)
        .with_context(|| format!("Failed to create dist directory: {}", dist_dir.display()))?;
    let archive_root = format!("decune-v{version}-{target}");
    let staging = TempDir::new_in(&dist_dir).with_context(|| {
        format!(
            "Failed to create dist staging directory: {}",
            dist_dir.display()
        )
    })?;
    let root_dir = staging.path().join(&archive_root);
    fs::create_dir_all(&root_dir).with_context(|| {
        format!(
            "Failed to create archive root directory: {}",
            root_dir.display()
        )
    })?;
    fs::copy(&binary, root_dir.join("decune")).with_context(|| {
        format!(
            "Failed to copy decune release binary: {} -> {}",
            binary.display(),
            root_dir.join("decune").display()
        )
    })?;
    fs::set_permissions(root_dir.join("decune"), fs::Permissions::from_mode(0o755)).with_context(
        || {
            format!(
                "Failed to set decune binary permissions: {}",
                root_dir.join("decune").display()
            )
        },
    )?;
    fs::copy(workspace.join("LICENSE"), root_dir.join("LICENSE"))
        .context("Failed to copy LICENSE into release archive")?;
    if workspace.join("README.md").is_file() {
        fs::copy(workspace.join("README.md"), root_dir.join("README.md"))
            .context("Failed to copy README.md into release archive")?;
    }

    let archive = dist_dir.join(format!("{archive_root}.tar.gz"));
    create_tar_gz(&archive, staging.path(), &archive_root)?;
    validate_archive_paths(&archive, &archive_root)?;
    Ok(())
}

fn checksum(dist_dir: &Path) -> Result<()> {
    let mut archives = dist_archives(dist_dir)?;
    archives.sort();
    let mut sums = String::new();
    for archive in archives {
        let file_name = archive
            .file_name()
            .and_then(|name| name.to_str())
            .context("Dist archive file name is not UTF-8")?;
        sums.push_str(&format!("{}  {}\n", sha256_file(&archive)?, file_name));
    }
    fs::write(dist_dir.join("SHA256SUMS"), sums)
        .with_context(|| format!("Failed to write {}", dist_dir.join("SHA256SUMS").display()))
}

fn release_manifest(dist_dir: &Path, version: &str) -> Result<()> {
    let mut archives = dist_archives(dist_dir)?;
    archives.sort();
    let mut artifacts = Vec::new();
    for archive in archives {
        let file_name = archive
            .file_name()
            .and_then(|name| name.to_str())
            .context("Dist archive file name is not UTF-8")?
            .to_owned();
        let target = file_name
            .strip_prefix(&format!("decune-v{version}-"))
            .and_then(|value| value.strip_suffix(".tar.gz"))
            .context("Dist archive name does not match requested version")?
            .to_owned();
        artifacts.push(ReleaseArtifact {
            file: file_name,
            target,
            sha256: sha256_file(&archive)?,
        });
    }
    let manifest = ReleaseManifest {
        schema_version: 1,
        version: version.to_owned(),
        artifacts,
    };
    let json = serde_json::to_string_pretty(&manifest)?;
    fs::write(dist_dir.join("release-manifest.json"), format!("{json}\n")).with_context(|| {
        format!(
            "Failed to write {}",
            dist_dir.join("release-manifest.json").display()
        )
    })
}

fn release_preflight(workspace: &Path, tag: &str, version: &str) -> Result<()> {
    if tag != format!("v{version}") {
        bail!("Release tag and version mismatch: tag {tag}, version {version}");
    }
    if !is_release_version(version) {
        bail!(
            "Release version must be numeric semver core with optional prerelease suffix: {version}"
        );
    }
    let cargo_toml = fs::read_to_string(workspace.join("Cargo.toml"))
        .context("Failed to read Cargo.toml for release preflight")?;
    if !cargo_toml.contains(&format!("version     = \"{version}\""))
        && !cargo_toml.contains(&format!("version = \"{version}\""))
    {
        bail!("Cargo.toml package version does not match release version: {version}");
    }
    if !workspace.join("LICENSE").is_file() {
        bail!("LICENSE is required for release archives");
    }
    let output = Command::new("git")
        .current_dir(workspace)
        .args(["status", "--porcelain"])
        .output()
        .context("Failed to check git working tree status")?;
    if !output.status.success() {
        bail!("Failed to check git working tree status");
    }
    if !output.stdout.is_empty() {
        bail!("Release preflight requires a clean working tree");
    }
    Ok(())
}

fn is_release_version(version: &str) -> bool {
    let (core, prerelease) = match version.split_once('-') {
        Some((core, prerelease)) => (core, Some(prerelease)),
        None => (version, None),
    };
    if !is_semver_core(core) {
        return false;
    }
    prerelease.is_none_or(is_prerelease_suffix)
}

fn is_semver_core(core: &str) -> bool {
    let mut parts = core.split('.');
    let Some(major) = parts.next() else {
        return false;
    };
    let Some(minor) = parts.next() else {
        return false;
    };
    let Some(patch) = parts.next() else {
        return false;
    };
    if parts.next().is_some() {
        return false;
    }
    [major, minor, patch]
        .into_iter()
        .all(|part| !part.is_empty() && part.chars().all(|ch| ch.is_ascii_digit()))
}

fn is_prerelease_suffix(suffix: &str) -> bool {
    !suffix.is_empty()
        && suffix.split('.').all(|part| match part.chars().next() {
            Some(first) if first.is_ascii_alphanumeric() => part
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-'),
            _ => false,
        })
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
        sums.push_str(&format!("{}  {}\n", entry.sha256, entry.path));
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

fn create_tar_gz(archive: &Path, staging: &Path, archive_root: &str) -> Result<()> {
    let file = fs::File::create(archive)
        .with_context(|| format!("Failed to create release archive: {}", archive.display()))?;
    let encoder = GzEncoder::new(file, Compression::default());
    let mut tar = Builder::new(encoder);
    tar.append_dir_all(archive_root, staging.join(archive_root))
        .with_context(|| format!("Failed to write release archive: {}", archive.display()))?;
    tar.finish()
        .with_context(|| format!("Failed to finish release archive: {}", archive.display()))?;
    Ok(())
}

fn validate_archive_paths(archive: &Path, archive_root: &str) -> Result<()> {
    let file = fs::File::open(archive).with_context(|| {
        format!(
            "Failed to open release archive for validation: {}",
            archive.display()
        )
    })?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive_reader = tar::Archive::new(decoder);
    for entry in archive_reader.entries()? {
        let entry = entry?;
        let path = entry.path()?;
        if !path.starts_with(archive_root) {
            bail!(
                "Release archive contains path outside archive root: {}",
                path.display()
            );
        }
    }
    Ok(())
}

fn dist_archives(dist_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut archives = Vec::new();
    for entry in fs::read_dir(dist_dir)
        .with_context(|| format!("Failed to read dist directory: {}", dist_dir.display()))?
    {
        let entry = entry.with_context(|| {
            format!(
                "Failed to read dist directory entry: {}",
                dist_dir.display()
            )
        })?;
        let path = entry.path();
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".tar.gz"))
        {
            archives.push(path);
        }
    }
    if archives.is_empty() {
        bail!(
            "No release archives found in dist directory: {}",
            dist_dir.display()
        );
    }
    Ok(archives)
}

fn replace_dir(temp: TempDir, target: &Path) -> Result<()> {
    let persist_path = temp.keep();
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
    fs::rename(&persist_path, target).with_context(|| {
        format!(
            "Failed to replace container tools directory: {} -> {}",
            persist_path.display(),
            target.display()
        )
    })
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

fn run_command(mut command: Command, context: &str) -> Result<()> {
    let output = command
        .output()
        .with_context(|| format!("{context}: failed to spawn command"))?;
    if output.status.success() {
        return Ok(());
    }
    bail!(
        "{}.\nstdout:\n{}\nstderr:\n{}",
        context,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn sha256_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path)
        .with_context(|| format!("Failed to read file for sha256: {}", path.display()))?;
    Ok(hex_lower(&Sha256::digest(&bytes)))
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn workspace_root() -> Result<PathBuf> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .map(Path::to_path_buf)
        .context("Failed to determine workspace root from xtask manifest directory")
}

fn workspace_relative(workspace: &Path, path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(workspace.join(path))
    }
}

fn target_dir(workspace: &Path) -> PathBuf {
    env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.join("target"))
}

fn resolve_dist_dir(workspace: &Path, path: Option<&Path>) -> Result<PathBuf> {
    match path {
        Some(path) => workspace_relative(workspace, path),
        None => Ok(target_dir(workspace).join("dist")),
    }
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
struct Manifest {
    schema_version: u32,
    protocol_version: u32,
    tools: Vec<ManifestEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ManifestEntry {
    name: String,
    platform: String,
    path: String,
    sha256: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReleaseManifest {
    schema_version: u32,
    version: String,
    artifacts: Vec<ReleaseArtifact>,
}

#[derive(Debug, Serialize)]
struct ReleaseArtifact {
    file: String,
    target: String,
    sha256: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_platform_mapping_is_stable() {
        assert_eq!(PLATFORMS[0].id, "linux-amd64");
        assert_eq!(PLATFORMS[0].rust_target, "x86_64-unknown-linux-musl");
        assert_eq!(PLATFORMS[1].id, "linux-arm64");
        assert_eq!(PLATFORMS[1].rust_target, "aarch64-unknown-linux-musl");
    }

    #[test]
    fn rejects_manifest_path_traversal() {
        assert!(validate_manifest_path(Path::new("../tool")).is_err());
        assert!(validate_manifest_path(Path::new("/tmp/tool")).is_err());
        assert!(validate_manifest_path(Path::new("linux-amd64/tool")).is_ok());
    }

    #[test]
    fn release_version_allows_semver_core_and_prerelease_suffix() {
        for version in [
            "0.1.0",
            "1.20.300",
            "0.1.0-alpha",
            "0.1.0-alpha.1",
            "0.1.0-rc-1",
        ] {
            assert!(is_release_version(version), "{version}");
        }
    }

    #[test]
    fn release_version_rejects_invalid_semver_shapes() {
        for version in [
            "0.1",
            "0.1.0.1",
            "0.1.x",
            "0.1.0-",
            "0.1.0-alpha.",
            "0.1.0-.alpha",
            "0.1.0--alpha",
            "0.1.0+build",
        ] {
            assert!(!is_release_version(version), "{version}");
        }
    }
}
