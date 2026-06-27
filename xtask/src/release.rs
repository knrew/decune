use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

pub(crate) fn release_preflight(workspace: &Path, tag: &str, version: &str) -> Result<()> {
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
    let output = Command::new("cargo")
        .current_dir(workspace)
        .args(["metadata", "--locked", "--no-deps", "--format-version", "1"])
        .output()
        .context("Failed to run cargo metadata for release preflight")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "Failed to run cargo metadata for release preflight: {}",
            stderr.trim()
        );
    }
    workspace_package_versions_from_metadata(&output.stdout)
}

fn workspace_package_versions_from_metadata(bytes: &[u8]) -> Result<Vec<PackageVersion>> {
    let metadata: CargoMetadata = serde_json::from_slice(bytes)
        .context("Failed to parse cargo metadata for release preflight")?;
    let mut packages_by_id = BTreeMap::new();
    for package in metadata.packages {
        packages_by_id.insert(package.id.clone(), package);
    }

    metadata
        .workspace_members
        .into_iter()
        .map(|id| {
            let package = packages_by_id
                .remove(&id)
                .with_context(|| format!("cargo metadata did not include workspace member {id}"))?;
            Ok(PackageVersion {
                manifest: package.manifest_path,
                version: package.version,
            })
        })
        .collect()
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

#[derive(Debug, Deserialize)]
struct CargoMetadata {
    packages: Vec<CargoMetadataPackage>,
    workspace_members: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct CargoMetadataPackage {
    id: String,
    manifest_path: PathBuf,
    version: String,
}

#[derive(Debug, PartialEq, Eq)]
struct PackageVersion {
    manifest: PathBuf,
    version: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_package_versions_from_metadata_reads_workspace_members() {
        let metadata = br#"
        {
          "packages": [
            {
              "name": "decune",
              "version": "1.2.3",
              "id": "path+file:///workspace#1.2.3",
              "manifest_path": "/workspace/Cargo.toml"
            },
            {
              "name": "tools",
              "version": "1.2.3",
              "id": "path+file:///workspace/tools#1.2.3",
              "manifest_path": "/workspace/tools/Cargo.toml"
            }
          ],
          "workspace_members": [
            "path+file:///workspace#1.2.3",
            "path+file:///workspace/tools#1.2.3"
          ]
        }
        "#;

        let versions = workspace_package_versions_from_metadata(metadata).unwrap();

        assert_eq!(
            versions,
            [
                PackageVersion {
                    manifest: PathBuf::from("/workspace/Cargo.toml"),
                    version: "1.2.3".to_owned(),
                },
                PackageVersion {
                    manifest: PathBuf::from("/workspace/tools/Cargo.toml"),
                    version: "1.2.3".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn workspace_package_versions_from_metadata_rejects_missing_member_package() {
        let error = workspace_package_versions_from_metadata(
            br#"
            {
              "packages": [],
              "workspace_members": ["path+file:///workspace#1.2.3"]
            }
            "#,
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("cargo metadata did not include workspace member")
        );
    }

    #[test]
    fn release_version_allows_semver_core_and_prerelease_suffix() {
        for version in [
            "1.2.3",
            "1.20.300",
            "1.2.3-alpha",
            "1.2.3-alpha.1",
            "1.2.3-rc-1",
        ] {
            assert!(is_release_version(version), "{version}");
        }
    }

    #[test]
    fn release_version_rejects_invalid_semver_shapes() {
        for version in [
            "1.2",
            "1.2.3.4",
            "1.2.x",
            "01.2.3",
            "1.02.3",
            "1.2.03",
            "1.2.3-",
            "1.2.3-alpha.",
            "1.2.3-.alpha",
            "1.2.3--alpha",
            "1.2.3+build",
        ] {
            assert!(!is_release_version(version), "{version}");
        }
    }
}
