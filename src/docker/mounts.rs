use std::{collections::BTreeMap, path::Path};

use anyhow::{Context, Result, anyhow, bail};
use bollard::models::{Mount, MountType as DockerMountType};
use serde_json::Value as JsonValue;

use crate::config::{
    layer::LayerDevcontainerMount,
    path::{HostPathOptions, PathCreate, SymlinkResolution, resolve_host_path},
    resolved::{ResolvedConfig, ResolvedMount},
    types::{MountCreate, MountType},
    variables::VariableContext,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DockerMountSpec {
    pub(crate) source: Option<String>,
    pub(crate) target: String,
    pub(crate) mount_type: MountType,
    pub(crate) read_only: bool,
}

impl DockerMountSpec {
    pub(crate) fn to_bollard_mount(&self) -> Mount {
        Mount {
            target: Some(self.target.clone()),
            source: self.source.clone(),
            typ: Some(docker_mount_type(self.mount_type)),
            read_only: Some(self.read_only),
            ..Default::default()
        }
    }
}

fn docker_mount_type(mount_type: MountType) -> DockerMountType {
    match mount_type {
        MountType::Bind => DockerMountType::BIND,
        MountType::Volume => DockerMountType::VOLUME,
        MountType::Tmpfs => DockerMountType::TMPFS,
    }
}

pub(crate) fn config_mount_specs(
    config: &ResolvedConfig,
    workspace_root: &Path,
    variables: &VariableContext,
) -> Result<Vec<DockerMountSpec>> {
    let mut mounts = Vec::new();

    for mount in &config.devcontainer.mounts {
        mounts.push(devcontainer_mount_spec(mount, workspace_root, variables)?);
    }

    for mount in &config.mounts {
        mounts.push(resolved_mount_spec(mount, workspace_root, variables)?);
    }

    Ok(mounts)
}

fn resolved_mount_spec(
    mount: &ResolvedMount,
    workspace_root: &Path,
    variables: &VariableContext,
) -> Result<DockerMountSpec> {
    validate_target(&mount.target)?;

    match mount.mount_type {
        MountType::Bind => {
            let source = mount.source.as_deref().ok_or_else(|| {
                anyhow!("Bind mount source is required for target: {}", mount.target)
            })?;
            let source = resolve_bind_source(
                source,
                HostPathOptions::new(mount.origin, workspace_root, variables)
                    .with_create(path_create(mount.create))
                    .with_symlink_resolution(symlink_resolution(mount.resolve_symlink)),
            )
            .with_context(|| {
                format!(
                    "Failed to resolve bind mount source for target: {}",
                    mount.target
                )
            })?;

            Ok(DockerMountSpec {
                source: Some(source),
                target: mount.target.clone(),
                mount_type: MountType::Bind,
                read_only: mount.read_only,
            })
        }
        MountType::Volume => Ok(DockerMountSpec {
            source: mount.source.clone(),
            target: mount.target.clone(),
            mount_type: MountType::Volume,
            read_only: mount.read_only,
        }),
        MountType::Tmpfs => bail!("tmpfs mounts are not supported yet: {}", mount.target),
    }
}

fn devcontainer_mount_spec(
    mount: &LayerDevcontainerMount,
    workspace_root: &Path,
    variables: &VariableContext,
) -> Result<DockerMountSpec> {
    let parsed = parse_devcontainer_mount(mount)?;
    validate_target(&parsed.target)?;

    match parsed.mount_type {
        MountType::Bind => {
            let source = parsed.source.as_deref().ok_or_else(|| {
                anyhow!(
                    "Bind mount source is required for target: {}",
                    parsed.target
                )
            })?;
            let source = resolve_bind_source(
                source,
                HostPathOptions::new(
                    crate::config::path::ConfigPathOrigin::Project,
                    workspace_root,
                    variables,
                ),
            )
            .with_context(|| {
                format!(
                    "Failed to resolve devcontainer bind mount source for target: {}",
                    parsed.target
                )
            })?;

            Ok(DockerMountSpec {
                source: Some(source),
                target: parsed.target,
                mount_type: MountType::Bind,
                read_only: parsed.read_only,
            })
        }
        MountType::Volume => Ok(DockerMountSpec {
            source: parsed.source,
            target: parsed.target,
            mount_type: MountType::Volume,
            read_only: parsed.read_only,
        }),
        MountType::Tmpfs => bail!("tmpfs mounts are not supported yet: {}", parsed.target),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedMount {
    source: Option<String>,
    target: String,
    mount_type: MountType,
    read_only: bool,
}

fn parse_devcontainer_mount(mount: &LayerDevcontainerMount) -> Result<ParsedMount> {
    match mount {
        LayerDevcontainerMount::String(value) => parse_devcontainer_mount_fields(
            docker_mount_string_fields(value)
                .with_context(|| format!("Failed to parse devcontainer mount: {value}"))?,
        ),
        LayerDevcontainerMount::Object(values) => parse_devcontainer_mount_fields(
            devcontainer_mount_object_fields(values)
                .context("Failed to parse devcontainer mount object")?,
        ),
    }
}

fn docker_mount_string_fields(input: &str) -> Result<BTreeMap<String, MountFieldValue>> {
    let mut fields = BTreeMap::new();

    for segment in input
        .split(',')
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
    {
        if let Some((key, value)) = segment.split_once('=') {
            fields.insert(
                normalize_key(key),
                MountFieldValue::String(value.trim().to_owned()),
            );
        } else {
            let key = normalize_key(segment);
            if key == "readonly" {
                fields.insert(key, MountFieldValue::Bool(true));
            } else {
                bail!("Unsupported Docker mount flag: {segment}");
            }
        }
    }

    Ok(fields)
}

fn devcontainer_mount_object_fields(
    values: &BTreeMap<String, JsonValue>,
) -> Result<BTreeMap<String, MountFieldValue>> {
    values
        .iter()
        .map(|(key, value)| {
            Ok((
                normalize_key(key),
                match value {
                    JsonValue::String(value) => MountFieldValue::String(value.clone()),
                    JsonValue::Bool(value) => MountFieldValue::Bool(*value),
                    _ => bail!("Mount field must be a string or boolean: {key}"),
                },
            ))
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MountFieldValue {
    String(String),
    Bool(bool),
}

fn parse_devcontainer_mount_fields(
    mut fields: BTreeMap<String, MountFieldValue>,
) -> Result<ParsedMount> {
    let mount_type = match string_field(&mut fields, "type")?.as_deref() {
        Some("bind") => MountType::Bind,
        Some("volume") => MountType::Volume,
        Some("tmpfs") => MountType::Tmpfs,
        Some(value) => bail!("Unsupported mount type: {value}"),
        None => bail!("Mount type is required"),
    };
    let target =
        string_field(&mut fields, "target")?.ok_or_else(|| anyhow!("Mount target is required"))?;
    let source = string_field(&mut fields, "source")?;
    let read_only = bool_field(&mut fields, "readonly")?.unwrap_or(false);

    if !fields.is_empty() {
        bail!(
            "Unsupported mount option: {}",
            fields.keys().cloned().collect::<Vec<_>>().join(", ")
        );
    }

    Ok(ParsedMount {
        source,
        target,
        mount_type,
        read_only,
    })
}

fn string_field(
    fields: &mut BTreeMap<String, MountFieldValue>,
    key: &str,
) -> Result<Option<String>> {
    match fields.remove(key) {
        Some(MountFieldValue::String(value)) => Ok(Some(value)),
        Some(MountFieldValue::Bool(_)) => bail!("Mount field must be a string: {key}"),
        None => Ok(None),
    }
}

fn bool_field(fields: &mut BTreeMap<String, MountFieldValue>, key: &str) -> Result<Option<bool>> {
    match fields.remove(key) {
        Some(MountFieldValue::Bool(value)) => Ok(Some(value)),
        Some(MountFieldValue::String(value)) => match value.as_str() {
            "true" => Ok(Some(true)),
            "false" => Ok(Some(false)),
            _ => bail!("Mount field must be a boolean: {key}"),
        },
        None => Ok(None),
    }
}

fn normalize_key(key: &str) -> String {
    match key.trim() {
        "src" => "source".to_owned(),
        "dst" | "destination" => "target".to_owned(),
        "readonly" | "readOnly" | "read_only" | "ro" => "readonly".to_owned(),
        key => key.to_owned(),
    }
}

fn resolve_bind_source(source: &str, options: HostPathOptions<'_>) -> Result<String> {
    Ok(resolve_host_path(source, &options)?.display().to_string())
}

fn path_create(create: Option<MountCreate>) -> PathCreate {
    match create {
        Some(MountCreate::Directory) => PathCreate::Directory,
        None => PathCreate::None,
    }
}

fn symlink_resolution(resolve_symlink: bool) -> SymlinkResolution {
    if resolve_symlink {
        SymlinkResolution::Resolve
    } else {
        SymlinkResolution::Preserve
    }
}

fn validate_target(target: &str) -> Result<()> {
    if target.starts_with('/') {
        Ok(())
    } else {
        bail!("Mount target must be an absolute container path: {target}")
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    #[cfg(unix)]
    use std::os::unix::fs as unix_fs;

    use serde_json::json;

    use super::*;
    use crate::config::{
        layer::LayerDevcontainerMount,
        path::ConfigPathOrigin,
        resolved::ResolvedConfig,
        types::{MountCreate, MountType},
        variables::VariableContext,
    };

    fn variables(workspace_root: &Path) -> VariableContext {
        VariableContext::new(
            workspace_root.to_path_buf(),
            "project".to_owned(),
            "/workspaces/project".to_owned(),
            "project".to_owned(),
            "abc123def456".to_owned(),
            1000,
            1000,
            "vscode".to_owned(),
            "/home/vscode".to_owned(),
        )
    }

    #[test]
    fn resolves_decune_bind_mount_source_path() {
        let workspace = tempfile::tempdir().unwrap();
        let source = workspace.path().join("cache");
        fs::create_dir_all(&source).unwrap();
        let config = ResolvedConfig {
            mounts: vec![ResolvedMount {
                source: Some("cache".to_owned()),
                target: "/cache".to_owned(),
                mount_type: MountType::Bind,
                read_only: true,
                resolve_symlink: true,
                create: None,
                origin: ConfigPathOrigin::Project,
            }],
            ..ResolvedConfig::default()
        };

        let mounts =
            config_mount_specs(&config, workspace.path(), &variables(workspace.path())).unwrap();

        assert_eq!(
            mounts,
            vec![DockerMountSpec {
                source: Some(source.canonicalize().unwrap().display().to_string()),
                target: "/cache".to_owned(),
                mount_type: MountType::Bind,
                read_only: true,
            }]
        );
    }

    #[test]
    fn creates_decune_bind_directory_when_requested() {
        let workspace = tempfile::tempdir().unwrap();
        let source = workspace.path().join("generated/cache");
        let config = ResolvedConfig {
            mounts: vec![ResolvedMount {
                source: Some("generated/cache".to_owned()),
                target: "/cache".to_owned(),
                mount_type: MountType::Bind,
                read_only: false,
                resolve_symlink: true,
                create: Some(MountCreate::Directory),
                origin: ConfigPathOrigin::Project,
            }],
            ..ResolvedConfig::default()
        };

        let mounts =
            config_mount_specs(&config, workspace.path(), &variables(workspace.path())).unwrap();

        assert!(source.is_dir());
        assert_eq!(
            mounts[0].source.as_deref(),
            Some(source.canonicalize().unwrap().to_str().unwrap())
        );
    }

    #[cfg(unix)]
    #[test]
    fn preserves_decune_bind_symlink_when_requested() {
        let workspace = tempfile::tempdir().unwrap();
        let real_source = workspace.path().join("real-cache");
        let link_source = workspace.path().join("cache-link");
        fs::create_dir_all(&real_source).unwrap();
        unix_fs::symlink(&real_source, &link_source).unwrap();
        let config = ResolvedConfig {
            mounts: vec![ResolvedMount {
                source: Some("cache-link".to_owned()),
                target: "/cache".to_owned(),
                mount_type: MountType::Bind,
                read_only: false,
                resolve_symlink: false,
                create: None,
                origin: ConfigPathOrigin::Project,
            }],
            ..ResolvedConfig::default()
        };

        let mounts =
            config_mount_specs(&config, workspace.path(), &variables(workspace.path())).unwrap();

        assert_eq!(
            mounts[0].source.as_deref(),
            Some(link_source.to_str().unwrap())
        );
    }

    #[test]
    fn converts_decune_volume_mount_without_host_path_resolution() {
        let workspace = tempfile::tempdir().unwrap();
        let config = ResolvedConfig {
            mounts: vec![ResolvedMount {
                source: Some("project-cache".to_owned()),
                target: "/cache".to_owned(),
                mount_type: MountType::Volume,
                read_only: false,
                resolve_symlink: true,
                create: None,
                origin: ConfigPathOrigin::Project,
            }],
            ..ResolvedConfig::default()
        };

        let mounts =
            config_mount_specs(&config, workspace.path(), &variables(workspace.path())).unwrap();

        assert_eq!(mounts[0].source.as_deref(), Some("project-cache"));
        assert_eq!(mounts[0].mount_type, MountType::Volume);
    }

    #[test]
    fn parses_devcontainer_string_mount() {
        let workspace = tempfile::tempdir().unwrap();
        let source = workspace.path().join("tools");
        fs::create_dir_all(&source).unwrap();
        let config = ResolvedConfig {
            devcontainer: crate::config::resolved::ResolvedDevcontainer {
                mounts: vec![LayerDevcontainerMount::String(
                    "source=${localWorkspaceFolder}/tools,target=/tools,type=bind,readonly=true"
                        .to_owned(),
                )],
                ..Default::default()
            },
            ..ResolvedConfig::default()
        };

        let mounts =
            config_mount_specs(&config, workspace.path(), &variables(workspace.path())).unwrap();

        assert_eq!(mounts[0].source.as_deref(), Some(source.to_str().unwrap()));
        assert_eq!(mounts[0].target, "/tools");
        assert_eq!(mounts[0].mount_type, MountType::Bind);
        assert!(mounts[0].read_only);
    }

    #[test]
    fn parses_devcontainer_object_mount() {
        let workspace = tempfile::tempdir().unwrap();
        let source = workspace.path().join("data");
        fs::create_dir_all(&source).unwrap();
        let config = ResolvedConfig {
            devcontainer: crate::config::resolved::ResolvedDevcontainer {
                mounts: vec![LayerDevcontainerMount::Object(
                    [
                        ("type".to_owned(), json!("bind")),
                        ("source".to_owned(), json!("data")),
                        ("target".to_owned(), json!("/data")),
                        ("readOnly".to_owned(), json!(true)),
                    ]
                    .into(),
                )],
                ..Default::default()
            },
            ..ResolvedConfig::default()
        };

        let mounts =
            config_mount_specs(&config, workspace.path(), &variables(workspace.path())).unwrap();

        assert_eq!(
            mounts[0].source.as_deref(),
            Some(source.canonicalize().unwrap().to_str().unwrap())
        );
        assert_eq!(mounts[0].target, "/data");
        assert!(mounts[0].read_only);
    }

    #[test]
    fn rejects_devcontainer_tmpfs_mount_until_supported() {
        let workspace = tempfile::tempdir().unwrap();
        let config = ResolvedConfig {
            devcontainer: crate::config::resolved::ResolvedDevcontainer {
                mounts: vec![LayerDevcontainerMount::String(
                    "target=/tmp/cache,type=tmpfs".to_owned(),
                )],
                ..Default::default()
            },
            ..ResolvedConfig::default()
        };

        let error = config_mount_specs(&config, workspace.path(), &variables(workspace.path()))
            .unwrap_err();

        assert!(error.to_string().contains("tmpfs mounts are not supported"));
    }
}
