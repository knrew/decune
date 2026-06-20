use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::OsString,
    fs, io,
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
const COMPOSE_CAPABILITIES: [ComposeCapabilityRequirement; 8] = [
    ComposeCapabilityRequirement {
        subcommand: "config",
        option: "--format",
        capability: "docker compose config --format json",
    },
    ComposeCapabilityRequirement {
        subcommand: "ps",
        option: "--format",
        capability: "docker compose ps --format json",
    },
    ComposeCapabilityRequirement {
        subcommand: "build",
        option: "--with-dependencies",
        capability: "docker compose build --with-dependencies",
    },
    ComposeCapabilityRequirement {
        subcommand: "pull",
        option: "--policy",
        capability: "docker compose pull --policy always",
    },
    ComposeCapabilityRequirement {
        subcommand: "pull",
        option: "--ignore-buildable",
        capability: "docker compose pull --ignore-buildable",
    },
    ComposeCapabilityRequirement {
        subcommand: "pull",
        option: "--include-deps",
        capability: "docker compose pull --include-deps",
    },
    ComposeCapabilityRequirement {
        subcommand: "up",
        option: "--force-recreate",
        capability: "docker compose up --force-recreate",
    },
    ComposeCapabilityRequirement {
        subcommand: "up",
        option: "--remove-orphans",
        capability: "docker compose up --remove-orphans",
    },
];

#[derive(Debug, Clone, Copy)]
struct ComposeCapabilityRequirement {
    subcommand: &'static str,
    option: &'static str,
    capability: &'static str,
}

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
    ComposeIntegration {
        #[arg(long)]
        release: bool,
    },
    WorkspaceTest {
        #[arg(long)]
        release: bool,
    },
    Install {
        #[arg(long)]
        locked: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        root: Option<PathBuf>,
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
        #[arg(long)]
        container_tools_dir: Option<PathBuf>,
    },
    Checksum {
        #[arg(long)]
        dist_dir: Option<PathBuf>,
        #[arg(long)]
        version: Option<String>,
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
        XtaskCommand::ComposeIntegration { release } => compose_integration(&workspace, release),
        XtaskCommand::WorkspaceTest { release } => workspace_test(&workspace, release),
        XtaskCommand::Install {
            locked,
            force,
            root,
        } => install(&workspace, locked, force, root.as_deref()),
        XtaskCommand::Dist {
            target,
            version,
            locked,
            dist_dir,
            container_tools_dir,
        } => dist(
            &workspace,
            &target,
            &version,
            locked,
            dist_dir.as_deref(),
            container_tools_dir.as_deref(),
        ),
        XtaskCommand::Checksum { dist_dir, version } => checksum(
            &resolve_dist_dir(&workspace, dist_dir.as_deref())?,
            version.as_deref(),
        ),
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct ChildCommand {
    program: String,
    current_dir: Option<PathBuf>,
    args: Vec<String>,
    env: BTreeMap<String, OsString>,
}

impl ChildCommand {
    fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            current_dir: None,
            args: Vec::new(),
            env: BTreeMap::new(),
        }
    }

    fn current_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.current_dir = Some(path.into());
        self
    }

    fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    fn env(mut self, key: impl Into<String>, value: impl Into<OsString>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    fn into_command(self) -> Command {
        let mut command = Command::new(&self.program);
        if let Some(current_dir) = self.current_dir {
            command.current_dir(current_dir);
        }
        command.args(self.args);
        for (key, value) in self.env {
            command.env(key, value);
        }
        command
    }
}

fn compose_integration(workspace: &Path, release: bool) -> Result<()> {
    let mut docker_version = Command::new("docker");
    docker_version.arg("version");
    run_command(
        docker_version,
        "Docker CLI is required for Docker Compose integration tests",
    )?;
    compose_integration_preflight()?;

    let bundle_dir = prepare_xtask_container_tools_bundle(workspace, true)?;
    let command = compose_integration_cargo_command(workspace, release, &bundle_dir);

    run_command_spec(command, "Failed to run Docker Compose integration tests")
}

fn compose_integration_preflight() -> Result<()> {
    let version = docker_output_text(&["compose", "version"])
        .context("Docker Compose v2 plugin is required for Docker Compose integration tests")?;
    let version_short = docker_output_text(&["compose", "version", "--short"])
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| version.trim().to_owned());
    eprintln!("Docker Compose version: {version_short}");

    for requirement in COMPOSE_CAPABILITIES {
        let help = docker_output_text(&["compose", requirement.subcommand, "--help"])
            .with_context(|| {
                format!(
                    "Failed to probe Docker Compose capability: {}",
                    requirement.capability
                )
            })?;
        if !help_contains_option(&help, requirement.option) {
            bail!(
                "Docker Compose v2 plugin is missing required capability: {} ({} --help does not list {}). Update Docker Compose v2 plugin to a newer release.",
                requirement.capability,
                requirement.subcommand,
                requirement.option
            );
        }
        eprintln!("Docker Compose capability OK: {}", requirement.capability);
    }

    Ok(())
}

fn workspace_test(workspace: &Path, release: bool) -> Result<()> {
    let bundle_dir = prepare_xtask_container_tools_bundle(workspace, true)?;
    let command = workspace_test_cargo_command(workspace, release, &bundle_dir);

    run_command_spec(command, "Failed to run workspace tests")
}

fn install(workspace: &Path, locked: bool, force: bool, root: Option<&Path>) -> Result<()> {
    let plan = install_plan(workspace, locked, force, root);
    prepare_container_tools_bundle(workspace, &plan.bundle_dir, plan.bundle_locked)?;

    run_command_spec(
        plan.command,
        "Failed to install decune from local source checkout",
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InstallPlan {
    bundle_dir: PathBuf,
    bundle_locked: bool,
    command: ChildCommand,
}

fn install_plan(workspace: &Path, locked: bool, force: bool, root: Option<&Path>) -> InstallPlan {
    let bundle_dir = default_xtask_container_tools_bundle_dir(workspace);
    let command = install_cargo_command(workspace, locked, force, root, &bundle_dir);
    InstallPlan {
        bundle_dir,
        bundle_locked: locked,
        command,
    }
}

fn compose_integration_cargo_command(
    workspace: &Path,
    release: bool,
    bundle_dir: &Path,
) -> ChildCommand {
    let mut command = cargo_command_with_container_tools(workspace, bundle_dir).arg("test");
    if release {
        command = command.arg("--release");
    }
    command.args([
        "--workspace",
        "--all-features",
        "--no-fail-fast",
        "compose_integration",
        "--",
        "--ignored",
        "--test-threads=1",
    ])
}

fn workspace_test_cargo_command(
    workspace: &Path,
    release: bool,
    bundle_dir: &Path,
) -> ChildCommand {
    let mut command = cargo_command_with_container_tools(workspace, bundle_dir).arg("test");
    if release {
        command = command.arg("--release");
    }
    command.args([
        "--workspace",
        "--all-features",
        "--no-fail-fast",
        "--verbose",
    ])
}

fn cargo_command_with_container_tools(workspace: &Path, bundle_dir: &Path) -> ChildCommand {
    ChildCommand::new("cargo")
        .current_dir(workspace)
        .env("DECUNE_CONTAINER_TOOLS_BUNDLE", "required")
        .env("DECUNE_CONTAINER_TOOLS_BUNDLE_DIR", bundle_dir.as_os_str())
}

fn install_cargo_command(
    workspace: &Path,
    locked: bool,
    force: bool,
    root: Option<&Path>,
    bundle_dir: &Path,
) -> ChildCommand {
    let mut command = cargo_command_with_container_tools(workspace, bundle_dir).args([
        "install",
        "--path",
        ".",
        "--profile",
        "dist",
        "--bin",
        "decune",
    ]);
    if locked {
        command = command.arg("--locked");
    }
    if force {
        command = command.arg("--force");
    }
    if let Some(root) = root {
        command = command
            .arg("--root")
            .arg(root.to_string_lossy().into_owned());
    }
    command
}

fn prepare_xtask_container_tools_bundle(workspace: &Path, locked: bool) -> Result<PathBuf> {
    let bundle_dir = default_xtask_container_tools_bundle_dir(workspace);
    prepare_container_tools_bundle(workspace, &bundle_dir, locked)?;
    Ok(bundle_dir)
}

fn prepare_container_tools_bundle(workspace: &Path, bundle_dir: &Path, locked: bool) -> Result<()> {
    build_container_tools(workspace, bundle_dir, locked)?;
    check_container_tools(bundle_dir)?;
    Ok(())
}

fn default_xtask_container_tools_bundle_dir(workspace: &Path) -> PathBuf {
    target_dir(workspace)
        .join("decune-xtask")
        .join("container-tools-bundle")
}

fn dist_build_command(
    workspace: &Path,
    target: &str,
    locked: bool,
    bundle_dir: &Path,
) -> ChildCommand {
    let mut command = cargo_command_with_container_tools(workspace, bundle_dir).args([
        "build",
        "--profile",
        "dist",
        "--target",
        target,
        "-p",
        "decune",
    ]);
    if locked {
        command = command.arg("--locked");
    }
    command
}

fn run_command_spec(command: ChildCommand, context: &str) -> Result<()> {
    run_command(command.into_command(), context)
}

fn dist(
    workspace: &Path,
    target: &str,
    version: &str,
    locked: bool,
    dist_dir: Option<&Path>,
    container_tools_dir: Option<&Path>,
) -> Result<()> {
    let bundle_dir = container_tools_dir
        .map(Path::to_path_buf)
        .or_else(|| env::var_os("DECUNE_CONTAINER_TOOLS_BUNDLE_DIR").map(PathBuf::from))
        .unwrap_or_else(|| workspace.join("assets/container-tools"));
    let bundle_dir = workspace_relative(workspace, &bundle_dir)?;
    check_container_tools(&bundle_dir)?;

    let command = dist_build_command(workspace, target, locked, &bundle_dir);
    run_command_spec(command, "Failed to build decune release binary")?;

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

fn checksum(dist_dir: &Path, version: Option<&str>) -> Result<()> {
    let mut archives = dist_archives_for_version(dist_dir, version)?;
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
    let mut archives = dist_archives_for_version(dist_dir, Some(version))?;
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
    for package in workspace_package_versions(workspace)? {
        if package.version != version {
            bail!(
                "{} package version does not match release version: expected {version}, got {}",
                package.manifest.display(),
                package.version
            );
        }
    }
    require_release_doc_refs(workspace, version)?;
    if !workspace.join("LICENSE").is_file() {
        bail!("LICENSE is required for release archives");
    }
    require_clean_worktree(workspace)?;
    Ok(())
}

fn require_release_doc_refs(workspace: &Path, version: &str) -> Result<()> {
    for path in ["README.md", "docs/usage.md"] {
        let path = workspace.join(path);
        let text = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read release documentation: {}", path.display()))?;
        let install_ref = format!("/v{version}/scripts/install.sh");
        let version_ref = format!("--version {version}");
        if !text.contains(&install_ref) || !text.contains(&version_ref) {
            bail!(
                "{} must reference release v{version} in the install command",
                path.display()
            );
        }
    }
    Ok(())
}

fn require_clean_worktree(workspace: &Path) -> Result<()> {
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

fn workspace_package_versions(workspace: &Path) -> Result<Vec<PackageVersion>> {
    let root_manifest = workspace.join("Cargo.toml");
    let source = fs::read_to_string(&root_manifest)
        .with_context(|| format!("Failed to read {}", root_manifest.display()))?;
    let parsed: toml::Value =
        toml::from_str(&source).context("Failed to parse Cargo.toml for release preflight")?;
    let members = parsed
        .get("workspace")
        .and_then(|workspace| workspace.get("members"))
        .and_then(toml::Value::as_array)
        .context("Cargo.toml workspace.members is missing or not an array")?;

    let mut packages = Vec::new();
    for member in members {
        let member = member
            .as_str()
            .context("Cargo.toml workspace member is not a string")?;
        if member.contains('*') {
            bail!("Release preflight does not support workspace member globs: {member}");
        }
        let manifest = workspace.join(member).join("Cargo.toml");
        let source = fs::read_to_string(&manifest)
            .with_context(|| format!("Failed to read {}", manifest.display()))?;
        packages.push(PackageVersion {
            manifest,
            version: package_version_from_toml(&source)?,
        });
    }
    Ok(packages)
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
        .all(is_semver_numeric_identifier)
}

fn is_semver_numeric_identifier(part: &str) -> bool {
    if part.is_empty() {
        return false;
    }
    if part.len() > 1 && part.starts_with('0') {
        return false;
    }
    part.chars().all(|ch| ch.is_ascii_digit())
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

fn dist_archives_for_version(dist_dir: &Path, version: Option<&str>) -> Result<Vec<PathBuf>> {
    let expected_prefix = version.map(|version| format!("decune-v{version}-"));
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
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.ends_with(".tar.gz") {
            continue;
        }
        if expected_prefix
            .as_deref()
            .is_some_and(|prefix| !name.starts_with(prefix))
        {
            continue;
        }
        archives.push(path);
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

fn docker_output_text(args: &[&str]) -> Result<String> {
    let output = Command::new("docker")
        .args(args)
        .output()
        .with_context(|| format!("failed to spawn docker {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "docker {} exited with {}: {}",
            args.join(" "),
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.stderr.is_empty() {
        text.push('\n');
        text.push_str(&String::from_utf8_lossy(&output.stderr));
    }
    Ok(text)
}

fn help_contains_option(help: &str, option: &str) -> bool {
    help.split(|ch: char| {
        ch.is_ascii_whitespace() || matches!(ch, ',' | ';' | '[' | ']' | '(' | ')' | '{' | '}')
    })
    .any(|token| token == option || token.starts_with(&format!("{option}=")))
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
    target_dir_from_env_value(
        workspace,
        env::var_os("CARGO_TARGET_DIR").map(PathBuf::from),
    )
}

fn target_dir_from_env_value(workspace: &Path, value: Option<PathBuf>) -> PathBuf {
    match value {
        Some(path) if path.is_absolute() => path,
        Some(path) => workspace.join(path),
        None => workspace.join("target"),
    }
}

fn resolve_dist_dir(workspace: &Path, path: Option<&Path>) -> Result<PathBuf> {
    match path {
        Some(path) => workspace_relative(workspace, path),
        None => Ok(target_dir(workspace).join("dist")),
    }
}

fn package_version_from_toml(source: &str) -> Result<String> {
    let parsed: toml::Value =
        toml::from_str(source).context("Failed to parse Cargo.toml for release preflight")?;
    parsed
        .get("package")
        .and_then(|package| package.get("version"))
        .and_then(|version| version.as_str())
        .map(str::to_owned)
        .context("Cargo.toml package.version is missing or not a string")
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, PartialEq, Eq)]
struct PackageVersion {
    manifest: PathBuf,
    version: String,
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
    fn rejects_manifest_path_traversal() {
        assert!(validate_manifest_path(Path::new("../tool")).is_err());
        assert!(validate_manifest_path(Path::new("/tmp/tool")).is_err());
        assert!(validate_manifest_path(Path::new("linux-amd64/tool")).is_ok());
    }

    #[test]
    fn target_dir_resolves_relative_cargo_target_dir_against_workspace() {
        let workspace = Path::new("/workspace/decune");
        assert_eq!(
            target_dir_from_env_value(workspace, Some(PathBuf::from("target-custom"))),
            PathBuf::from("/workspace/decune/target-custom"),
        );
    }

    #[test]
    fn target_dir_preserves_absolute_cargo_target_dir() {
        let workspace = Path::new("/workspace/decune");
        assert_eq!(
            target_dir_from_env_value(workspace, Some(PathBuf::from("/tmp/target-custom"))),
            PathBuf::from("/tmp/target-custom"),
        );
    }

    #[test]
    fn target_dir_defaults_to_workspace_target() {
        let workspace = Path::new("/workspace/decune");
        assert_eq!(
            target_dir_from_env_value(workspace, None),
            PathBuf::from("/workspace/decune/target"),
        );
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

    #[test]
    fn compose_integration_cargo_command_runs_ignored_tests_without_env_gate() {
        let workspace = Path::new("/workspace/decune");
        let bundle_dir = Path::new("/workspace/decune/target/decune-xtask/container-tools-bundle");

        let command = compose_integration_cargo_command(workspace, false, bundle_dir);

        assert_eq!(command.program, "cargo");
        assert_eq!(command.current_dir.as_deref(), Some(workspace));
        assert_eq!(
            command.args,
            [
                "test",
                "--workspace",
                "--all-features",
                "--no-fail-fast",
                "compose_integration",
                "--",
                "--ignored",
                "--test-threads=1",
            ]
        );
        assert_eq!(
            command.env.get("DECUNE_CONTAINER_TOOLS_BUNDLE"),
            Some(&std::ffi::OsString::from("required"))
        );
        assert_eq!(
            command.env.get("DECUNE_CONTAINER_TOOLS_BUNDLE_DIR"),
            Some(&bundle_dir.as_os_str().to_owned())
        );
        assert!(!command.env.contains_key("DECUNE_COMPOSE_INTEGRATION"));
    }

    #[test]
    fn workspace_test_cargo_command_hides_container_tool_env_from_ci_yaml() {
        let workspace = Path::new("/workspace/decune");
        let bundle_dir = Path::new("/workspace/decune/target/decune-xtask/container-tools-bundle");

        let command = workspace_test_cargo_command(workspace, true, bundle_dir);

        assert_eq!(
            command.args,
            [
                "test",
                "--release",
                "--workspace",
                "--all-features",
                "--no-fail-fast",
                "--verbose",
            ]
        );
        assert_eq!(
            command.env.get("DECUNE_CONTAINER_TOOLS_BUNDLE"),
            Some(&std::ffi::OsString::from("required"))
        );
        assert!(!command.env.contains_key("DECUNE_COMPOSE_INTEGRATION"));
    }

    #[test]
    fn install_cargo_command_installs_local_checkout_with_required_bundle() {
        let workspace = Path::new("/workspace/decune");
        let bundle_dir = Path::new("/workspace/decune/target/decune-xtask/container-tools-bundle");
        let root = Path::new("/workspace/decune/target/install-smoke");

        let command = install_cargo_command(workspace, true, true, Some(root), bundle_dir);

        assert_eq!(command.program, "cargo");
        assert_eq!(command.current_dir.as_deref(), Some(workspace));
        assert_eq!(
            command.args,
            [
                "install",
                "--path",
                ".",
                "--profile",
                "dist",
                "--bin",
                "decune",
                "--locked",
                "--force",
                "--root",
                "/workspace/decune/target/install-smoke",
            ]
        );
        assert_eq!(
            command.env.get("DECUNE_CONTAINER_TOOLS_BUNDLE"),
            Some(&std::ffi::OsString::from("required"))
        );
        assert_eq!(
            command.env.get("DECUNE_CONTAINER_TOOLS_BUNDLE_DIR"),
            Some(&bundle_dir.as_os_str().to_owned())
        );
    }

    #[test]
    fn install_plan_uses_same_locked_mode_for_bundle_build_and_install_command() {
        let workspace = Path::new("/workspace/decune");

        let unlocked = install_plan(workspace, false, false, None);

        assert!(!unlocked.bundle_locked);
        assert_eq!(
            unlocked.bundle_dir,
            PathBuf::from("/workspace/decune/target/decune-xtask/container-tools-bundle")
        );
        assert!(!unlocked.command.args.iter().any(|arg| arg == "--locked"));

        let locked = install_plan(workspace, true, false, None);

        assert!(locked.bundle_locked);
        assert_eq!(
            locked.bundle_dir,
            PathBuf::from("/workspace/decune/target/decune-xtask/container-tools-bundle")
        );
        assert!(locked.command.args.iter().any(|arg| arg == "--locked"));
    }

    #[test]
    fn dist_build_command_accepts_explicit_container_tools_dir() {
        let workspace = Path::new("/workspace/decune");
        let bundle_dir = Path::new("/tmp/container-tools-bundle");

        let command = dist_build_command(workspace, "x86_64-unknown-linux-musl", true, bundle_dir);

        assert_eq!(
            command.env.get("DECUNE_CONTAINER_TOOLS_BUNDLE"),
            Some(&std::ffi::OsString::from("required"))
        );
        assert_eq!(
            command.env.get("DECUNE_CONTAINER_TOOLS_BUNDLE_DIR"),
            Some(&bundle_dir.as_os_str().to_owned())
        );
        assert_eq!(
            command.args,
            [
                "build",
                "--profile",
                "dist",
                "--target",
                "x86_64-unknown-linux-musl",
                "-p",
                "decune",
                "--locked",
            ]
        );
    }

    #[test]
    fn dist_archives_filters_by_version() {
        let temp = TempDir::new().unwrap();
        for name in [
            "decune-v0.1.0-x86_64-unknown-linux-musl.tar.gz",
            "decune-v0.2.0-x86_64-unknown-linux-musl.tar.gz",
            "notes.txt",
        ] {
            fs::write(temp.path().join(name), b"archive").unwrap();
        }

        let archives = dist_archives_for_version(temp.path(), Some("0.1.0")).unwrap();

        assert_eq!(archives.len(), 1);
        assert_eq!(
            archives[0].file_name().and_then(|name| name.to_str()),
            Some("decune-v0.1.0-x86_64-unknown-linux-musl.tar.gz")
        );
    }

    #[test]
    fn package_version_from_toml_reads_package_version() {
        let version = package_version_from_toml(
            r#"
            [package]
            name = "decune"
            version     = "0.1.0"
            "#,
        )
        .unwrap();

        assert_eq!(version, "0.1.0");
    }

    #[test]
    fn package_version_from_toml_rejects_missing_package_version() {
        let error = package_version_from_toml(
            r#"
            [package]
            name = "decune"
            "#,
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Cargo.toml package.version is missing")
        );
    }

    #[test]
    fn workspace_package_versions_reads_workspace_members() {
        let temp = TempDir::new().unwrap();
        fs::write(
            temp.path().join("Cargo.toml"),
            r#"
            [package]
            name = "decune"
            version = "0.1.0"

            [workspace]
            members = [".", "tools"]
            "#,
        )
        .unwrap();
        fs::create_dir(temp.path().join("tools")).unwrap();
        fs::write(
            temp.path().join("tools/Cargo.toml"),
            r#"
            [package]
            name = "tools"
            version = "0.1.0"
            "#,
        )
        .unwrap();

        let versions = workspace_package_versions(temp.path()).unwrap();

        assert_eq!(
            versions,
            [
                PackageVersion {
                    manifest: temp.path().join("./Cargo.toml"),
                    version: "0.1.0".to_owned(),
                },
                PackageVersion {
                    manifest: temp.path().join("tools/Cargo.toml"),
                    version: "0.1.0".to_owned(),
                },
            ]
        );
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
            "01.2.3",
            "1.02.3",
            "1.2.03",
            "0.1.0-",
            "0.1.0-alpha.",
            "0.1.0-.alpha",
            "0.1.0--alpha",
            "0.1.0+build",
        ] {
            assert!(!is_release_version(version), "{version}");
        }
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
