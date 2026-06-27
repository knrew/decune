#![allow(
    clippy::all,
    clippy::cargo,
    clippy::nursery,
    clippy::pedantic,
    clippy::restriction,
    reason = "Temporary allow while strict clippy policy is introduced; code fixes will follow separately."
)]

use std::{
    collections::BTreeSet,
    env, fs,
    path::{Component, Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const REQUIRED_PROTOCOL_VERSION: u32 = 1;
const REQUIRED_SCHEMA_VERSION: u32 = 1;
const TOOLS: [&str; 2] = ["git-credential-decune", "decune-forward-agent"];
const PLATFORMS: [&str; 2] = ["linux-amd64", "linux-arm64"];

fn main() -> Result<()> {
    emit_display_version()?;

    println!("cargo:rerun-if-env-changed=DECUNE_CONTAINER_TOOLS_BUNDLE");
    println!("cargo:rerun-if-env-changed=DECUNE_CONTAINER_TOOLS_BUNDLE_DIR");
    println!("cargo:rerun-if-env-changed=CARGO_TARGET_DIR");

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").context("OUT_DIR is not set")?);
    let generated_path = out_dir.join("container_tools_bundle.rs");
    let mode = env::var("DECUNE_CONTAINER_TOOLS_BUNDLE").unwrap_or_else(|_| "auto".to_owned());
    let bundle_dir = resolve_bundle_dir()?;
    println!("cargo:rerun-if-changed={}", bundle_dir.display());

    match mode.as_str() {
        "off" => write_empty_bundle(&generated_path),
        "auto" if !bundle_dir.join("manifest.json").is_file() => {
            write_empty_bundle(&generated_path)
        }
        "auto" | "required" => {
            let entries = validate_bundle(&bundle_dir).with_context(|| {
                format!(
                    "Invalid decune container tools bundle: {}",
                    bundle_dir.display()
                )
            })?;
            write_bundle(&generated_path, &entries)
        }
        other => bail!(
            "Unsupported DECUNE_CONTAINER_TOOLS_BUNDLE value: {other}. Expected auto, required, or off."
        ),
    }
}

fn emit_display_version() -> Result<()> {
    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").context("CARGO_MANIFEST_DIR is not set")?);
    let package_version = env::var("CARGO_PKG_VERSION").context("CARGO_PKG_VERSION is not set")?;
    let metadata = display_version_metadata(&manifest_dir, &package_version);

    emit_display_version_rerun_instructions(&manifest_dir);
    println!(
        "cargo:rustc-env=DECUNE_DISPLAY_VERSION={}",
        metadata.display_version
    );
    if metadata.full_commit.is_some() {
        println!(
            "cargo:rustc-env=DECUNE_VERSION_SOURCE_ROOT={}",
            manifest_dir.display()
        );
    }
    if let Some(commit) = &metadata.full_commit {
        println!("cargo:rustc-env=DECUNE_VERSION_FULL_COMMIT={commit}");
    }
    if let Some(commit) = &metadata.short_commit {
        println!("cargo:rustc-env=DECUNE_VERSION_SHORT_COMMIT={commit}");
    }
    println!(
        "cargo:rustc-env=DECUNE_VERSION_RELEASE_TAG_MATCHES={}",
        metadata.release_tag_matches
    );
    Ok(())
}

fn emit_display_version_rerun_instructions(manifest_dir: &Path) {
    let mut emitted = BTreeSet::new();
    for path in [
        "build.rs",
        "Cargo.toml",
        "Cargo.lock",
        "src",
        ".git/HEAD",
        ".git/index",
        ".git/packed-refs",
        ".git/refs/heads",
        ".git/refs/tags",
    ] {
        let absolute = manifest_dir.join(path);
        emit_rerun_if_changed(&absolute, &mut emitted);
    }

    for path in git_worktree_paths(manifest_dir) {
        emit_rerun_if_changed(&path, &mut emitted);
    }
}

fn emit_rerun_if_changed(path: &Path, emitted: &mut BTreeSet<PathBuf>) {
    if path.exists() && emitted.insert(path.to_path_buf()) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn git_worktree_paths(workspace: &Path) -> Vec<PathBuf> {
    let Some(paths) = git_output(
        workspace,
        [
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ],
    ) else {
        return Vec::new();
    };

    paths
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(|path| workspace.join(path))
        .collect()
}

fn display_version_metadata(workspace: &Path, package_version: &str) -> DisplayVersionMetadata {
    let full_commit = match git_output(workspace, ["rev-parse", "HEAD"]) {
        Some(commit) if is_git_hash(&commit) => commit,
        _ => {
            return DisplayVersionMetadata {
                display_version: format!("{package_version}+source"),
                full_commit: None,
                short_commit: None,
                release_tag_matches: false,
            };
        }
    };
    let short_commit = full_commit.chars().take(12).collect::<String>();

    let dirty = git_dirty(workspace).unwrap_or(true);
    let release_tag_matches = head_has_release_tag(workspace, package_version).unwrap_or(false);

    let display_version = if !dirty && release_tag_matches {
        package_version.to_owned()
    } else {
        let dirty_suffix = if dirty { ".dirty" } else { "" };
        format!("{package_version}+g{short_commit}{dirty_suffix}")
    };
    DisplayVersionMetadata {
        display_version,
        full_commit: Some(full_commit),
        short_commit: Some(short_commit),
        release_tag_matches,
    }
}

fn git_dirty(workspace: &Path) -> Option<bool> {
    git_output(
        workspace,
        [
            "--no-optional-locks",
            "status",
            "--porcelain",
            "--untracked-files=normal",
        ],
    )
    .map(|status| !status.is_empty())
}

fn head_has_release_tag(workspace: &Path, package_version: &str) -> Option<bool> {
    let release_tag = format!("v{package_version}");
    git_output(
        workspace,
        ["tag", "--points-at", "HEAD", "--list", &release_tag],
    )
    .map(|tags| tags.lines().any(|tag| tag == release_tag))
}

fn git_output<const N: usize>(workspace: &Path, args: [&str; N]) -> Option<String> {
    let output = Command::new("git")
        .current_dir(workspace)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn is_git_hash(value: &str) -> bool {
    value.len() >= 7
        && value.len() <= 64
        && value
            .chars()
            .all(|ch| ch.is_ascii_hexdigit() && !ch.is_ascii_uppercase())
}

struct DisplayVersionMetadata {
    display_version: String,
    full_commit: Option<String>,
    short_commit: Option<String>,
    release_tag_matches: bool,
}

fn resolve_bundle_dir() -> Result<PathBuf> {
    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").context("CARGO_MANIFEST_DIR is not set")?);
    let bundle_dir = env::var_os("DECUNE_CONTAINER_TOOLS_BUNDLE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| default_bundle_dir(&manifest_dir));
    if bundle_dir.is_absolute() {
        Ok(bundle_dir)
    } else {
        Ok(manifest_dir.join(bundle_dir))
    }
}

fn default_bundle_dir(manifest_dir: &Path) -> PathBuf {
    target_dir(manifest_dir)
        .join("decune-xtask")
        .join("container-tools-bundle")
}

fn target_dir(manifest_dir: &Path) -> PathBuf {
    match env::var_os("CARGO_TARGET_DIR").map(PathBuf::from) {
        Some(path) if path.is_absolute() => path,
        Some(path) => manifest_dir.join(path),
        None => manifest_dir.join("target"),
    }
}

fn write_empty_bundle(path: &Path) -> Result<()> {
    fs::write(
        path,
        "pub(crate) static EMBEDDED_CONTAINER_TOOLS: &[EmbeddedContainerToolArtifact] = &[];\n",
    )
    .with_context(|| {
        format!(
            "Failed to write generated container tools bundle: {}",
            path.display()
        )
    })
}

fn write_bundle(path: &Path, entries: &[ValidatedEntry]) -> Result<()> {
    let mut code = String::from(
        "pub(crate) static EMBEDDED_CONTAINER_TOOLS: &[EmbeddedContainerToolArtifact] = &[\n",
    );
    for entry in entries {
        code.push_str("    EmbeddedContainerToolArtifact {\n");
        code.push_str(&format!("        name: {:?},\n", entry.name));
        code.push_str(&format!("        platform: {:?},\n", entry.platform));
        code.push_str(&format!("        sha256: {:?},\n", entry.sha256));
        code.push_str(&format!(
            "        bytes: include_bytes!({:?}),\n",
            entry.absolute_path
        ));
        code.push_str("    },\n");
    }
    code.push_str("];\n");
    fs::write(path, code).with_context(|| {
        format!(
            "Failed to write generated container tools bundle: {}",
            path.display()
        )
    })
}

fn validate_bundle(bundle_dir: &Path) -> Result<Vec<ValidatedEntry>> {
    let bundle_dir = fs::canonicalize(bundle_dir).with_context(|| {
        format!(
            "Failed to canonicalize container tools bundle dir: {}",
            bundle_dir.display()
        )
    })?;
    let manifest_path = bundle_dir.join("manifest.json");
    let manifest_bytes = fs::read(&manifest_path).with_context(|| {
        format!(
            "Failed to read container tools manifest: {}",
            manifest_path.display()
        )
    })?;
    let manifest: Manifest = serde_json::from_slice(&manifest_bytes).with_context(|| {
        format!(
            "Failed to parse container tools manifest: {}",
            manifest_path.display()
        )
    })?;
    if manifest.schema_version != REQUIRED_SCHEMA_VERSION {
        bail!(
            "Unsupported container tools manifest schemaVersion: {}",
            manifest.schema_version
        );
    }
    if manifest.protocol_version != REQUIRED_PROTOCOL_VERSION {
        bail!(
            "Unsupported container tools protocolVersion: {}",
            manifest.protocol_version
        );
    }

    let expected = expected_container_tool_set();
    let mut seen = BTreeSet::new();
    let mut entries = Vec::new();
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
        let absolute_path = bundle_dir.join(&entry.path);
        if !absolute_path.is_file() {
            bail!(
                "Container tools manifest entry does not exist: {}",
                absolute_path.display()
            );
        }
        validate_executable(&absolute_path)?;
        let bytes = fs::read(&absolute_path).with_context(|| {
            format!(
                "Failed to read container tool artifact for checksum: {}",
                absolute_path.display()
            )
        })?;
        let actual = hex_lower(&Sha256::digest(&bytes));
        if actual != entry.sha256 {
            bail!(
                "Container tool artifact checksum mismatch: {}",
                absolute_path.display()
            );
        }
        entries.push(ValidatedEntry {
            name: entry.name,
            platform: entry.platform,
            sha256: entry.sha256,
            absolute_path,
        });
    }

    if seen != expected {
        let missing = expected.difference(&seen).cloned().collect::<Vec<_>>();
        bail!("Missing required container tool artifacts: {missing:?}");
    }

    entries.sort_by(|left, right| {
        left.platform
            .cmp(&right.platform)
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(entries)
}

fn expected_container_tool_set() -> BTreeSet<(String, String)> {
    PLATFORMS
        .iter()
        .flat_map(|platform| {
            TOOLS
                .iter()
                .map(move |tool| ((*tool).to_owned(), (*platform).to_owned()))
        })
        .collect()
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

#[cfg(unix)]
fn validate_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

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

#[cfg(not(unix))]
fn validate_executable(_path: &Path) -> Result<()> {
    Ok(())
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    schema_version: u32,
    protocol_version: u32,
    tools: Vec<ManifestEntry>,
}

#[derive(Debug, Deserialize)]
struct ManifestEntry {
    name: String,
    platform: String,
    path: PathBuf,
    sha256: String,
}

#[derive(Debug)]
struct ValidatedEntry {
    name: String,
    platform: String,
    sha256: String,
    absolute_path: PathBuf,
}
