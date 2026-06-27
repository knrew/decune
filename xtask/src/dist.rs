use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use flate2::{Compression, write::GzEncoder};
use serde::Serialize;
use tar::Builder;
use tempfile::TempDir;

use crate::{
    command::{ChildCommand, cargo_command_with_container_tools, run_command_spec},
    container_tools::{check_container_tools, prepare_xtask_container_tools_bundle},
    hash::sha256_file,
    paths::{resolve_dist_dir, target_dir, workspace_relative},
};

pub(crate) fn dist(
    workspace: &Path,
    target: &str,
    version: &str,
    locked: bool,
    dist_dir: Option<&Path>,
    container_tools_dir: Option<&Path>,
) -> Result<()> {
    let bundle_dir = match container_tools_dir {
        Some(bundle_dir) => {
            let bundle_dir = workspace_relative(workspace, bundle_dir);
            check_container_tools(&bundle_dir)?;
            bundle_dir
        }
        None => prepare_xtask_container_tools_bundle(workspace, locked)?,
    };

    let command = dist_build_command(workspace, target, locked, &bundle_dir);
    run_command_spec(command, "Failed to build decune release binary")?;

    let binary = target_dir(workspace)
        .join(target)
        .join("dist")
        .join("decune");
    if !binary.is_file() {
        bail!("Missing decune release binary: {}", binary.display());
    }

    let dist_dir = resolve_dist_dir(workspace, dist_dir);
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

pub(crate) fn checksum(dist_dir: &Path, version: Option<&str>) -> Result<()> {
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

pub(crate) fn release_manifest(dist_dir: &Path, version: &str) -> Result<()> {
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
    use crate::container_tools::default_xtask_container_tools_bundle_dir;

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
    fn dist_build_command_uses_prepared_container_tools_dir() {
        let workspace = Path::new("/workspace/decune");
        let bundle_dir = default_xtask_container_tools_bundle_dir(workspace);

        let command =
            dist_build_command(workspace, "x86_64-unknown-linux-musl", false, &bundle_dir);

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
    fn dist_archives_filters_by_version() {
        let temp = TempDir::new().unwrap();
        for name in [
            "decune-v1.2.3-x86_64-unknown-linux-musl.tar.gz",
            "decune-v2.0.0-x86_64-unknown-linux-musl.tar.gz",
            "notes.txt",
        ] {
            fs::write(temp.path().join(name), b"archive").unwrap();
        }

        let archives = dist_archives_for_version(temp.path(), Some("1.2.3")).unwrap();

        assert_eq!(archives.len(), 1);
        assert_eq!(
            archives[0].file_name().and_then(|name| name.to_str()),
            Some("decune-v1.2.3-x86_64-unknown-linux-musl.tar.gz")
        );
    }
}
