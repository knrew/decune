use std::{collections::BTreeMap, path::Path};

use anyhow::{Context, Result, anyhow, bail};
use bollard::models::{
    Mount, MountBindOptions, MountBindOptionsPropagationEnum, MountType as DockerMountType,
    MountVolumeOptions,
};
use serde_json::Value as JsonValue;

use crate::config::{
    layer::LayerDevcontainerMount,
    path::{
        ConfigPathOrigin, HostPathOptions, PathCreate, SymlinkResolution,
        resolve_expanded_host_path, resolve_host_path,
    },
    resolved::{ResolvedConfig, ResolvedMount},
    types::{MountCreate, MountType},
    variables::{VariableContext, expand_variables},
};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DockerMountSpec {
    pub(crate) source: Option<String>,
    pub(crate) target: String,
    pub(crate) mount_type: MountType,
    pub(crate) read_only: bool,
    pub(crate) consistency: Option<String>,
    pub(crate) bind_options: Option<MountBindOptions>,
    pub(crate) volume_options: Option<MountVolumeOptions>,
}

impl DockerMountSpec {
    pub(crate) fn to_bollard_mount(&self) -> Mount {
        Mount {
            target: Some(self.target.clone()),
            source: self.source.clone(),
            typ: Some(docker_mount_type(self.mount_type)),
            read_only: Some(self.read_only),
            consistency: self.consistency.clone(),
            bind_options: self.bind_options.clone(),
            volume_options: self.volume_options.clone(),
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

    for mount in config
        .mounts
        .iter()
        .filter(|mount| mount.origin == ConfigPathOrigin::Global)
    {
        replace_mount_by_target(
            &mut mounts,
            resolved_mount_spec(mount, workspace_root, variables)?,
        );
    }

    for mount in &config.devcontainer.mounts {
        replace_mount_by_target(
            &mut mounts,
            devcontainer_mount_spec(mount, workspace_root, variables)?,
        );
    }

    for mount in config
        .mounts
        .iter()
        .filter(|mount| mount.origin == ConfigPathOrigin::Project)
    {
        replace_mount_by_target(
            &mut mounts,
            resolved_mount_spec(mount, workspace_root, variables)?,
        );
    }

    Ok(mounts)
}

fn replace_mount_by_target(mounts: &mut Vec<DockerMountSpec>, mount: DockerMountSpec) {
    match mounts
        .iter()
        .position(|existing| existing.target == mount.target)
    {
        Some(index) => mounts[index] = mount,
        None => mounts.push(mount),
    }
}

fn resolved_mount_spec(
    mount: &ResolvedMount,
    workspace_root: &Path,
    variables: &VariableContext,
) -> Result<DockerMountSpec> {
    let target = expand_variables(&mount.target, variables)
        .with_context(|| format!("Failed to expand mount target: {}", mount.target))?;
    let target = validate_target(&target)?;

    match mount.mount_type {
        MountType::Bind => {
            let source = mount
                .source
                .as_deref()
                .ok_or_else(|| anyhow!("Bind mount source is required for target: {}", target))?;
            let source = resolve_bind_source(
                source,
                HostPathOptions::new(mount.origin, workspace_root, variables)
                    .with_create(path_create(mount.create))
                    .with_symlink_resolution(symlink_resolution(mount.resolve_symlink)),
            )
            .with_context(|| {
                format!("Failed to resolve bind mount source for target: {}", target)
            })?;

            Ok(DockerMountSpec {
                source: Some(source),
                target,
                mount_type: MountType::Bind,
                read_only: mount.read_only,
                consistency: None,
                bind_options: None,
                volume_options: None,
            })
        }
        MountType::Volume => {
            let source = mount
                .source
                .as_deref()
                .map(|source| {
                    expand_variables(source, variables)
                        .with_context(|| format!("Failed to expand mount source: {source}"))
                })
                .transpose()?;

            Ok(DockerMountSpec {
                source,
                target,
                mount_type: MountType::Volume,
                read_only: mount.read_only,
                consistency: None,
                bind_options: None,
                volume_options: None,
            })
        }
        MountType::Tmpfs => bail!("tmpfs mounts are not supported yet: {}", target),
    }
}

pub(crate) fn devcontainer_mount_spec(
    mount: &LayerDevcontainerMount,
    workspace_root: &Path,
    variables: &VariableContext,
) -> Result<DockerMountSpec> {
    let parsed = parse_devcontainer_mount(mount, variables)?;
    let target = validate_target(&parsed.target)?;

    match parsed.mount_type {
        MountType::Bind => {
            let source = parsed
                .source
                .as_deref()
                .ok_or_else(|| anyhow!("Bind mount source is required for target: {}", target))?;
            let source = resolve_expanded_bind_source(
                source,
                HostPathOptions::new(
                    crate::config::path::ConfigPathOrigin::Project,
                    workspace_root,
                    variables,
                )
                .with_create(bind_path_create(parsed.bind_options.as_ref())),
            )
            .with_context(|| {
                format!(
                    "Failed to resolve devcontainer bind mount source for target: {}",
                    target
                )
            })?;

            Ok(DockerMountSpec {
                source: Some(source),
                target,
                mount_type: MountType::Bind,
                read_only: parsed.read_only,
                consistency: parsed.consistency,
                bind_options: parsed.bind_options,
                volume_options: None,
            })
        }
        MountType::Volume => Ok(DockerMountSpec {
            source: parsed.source,
            target,
            mount_type: MountType::Volume,
            read_only: parsed.read_only,
            consistency: parsed.consistency,
            bind_options: None,
            volume_options: parsed.volume_options,
        }),
        MountType::Tmpfs => bail!("tmpfs mounts are not supported yet: {}", target),
    }
}

#[derive(Debug, Clone, PartialEq)]
struct ParsedMount {
    source: Option<String>,
    target: String,
    mount_type: MountType,
    read_only: bool,
    consistency: Option<String>,
    bind_options: Option<MountBindOptions>,
    volume_options: Option<MountVolumeOptions>,
}

fn parse_devcontainer_mount(
    mount: &LayerDevcontainerMount,
    variables: &VariableContext,
) -> Result<ParsedMount> {
    match mount {
        LayerDevcontainerMount::String(value) => parse_devcontainer_mount_fields(
            expand_mount_fields(docker_mount_string_fields(value)?, variables)
                .with_context(|| format!("Failed to parse devcontainer mount: {value}"))?,
            true,
        ),
        LayerDevcontainerMount::Object(values) => parse_devcontainer_mount_fields(
            expand_mount_fields(devcontainer_mount_object_fields(values)?, variables)
                .context("Failed to parse devcontainer mount object")?,
            true,
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
            if matches!(
                key.as_str(),
                "readonly" | "bind-create-src" | "volume-nocopy"
            ) {
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

fn expand_mount_fields(
    fields: BTreeMap<String, MountFieldValue>,
    variables: &VariableContext,
) -> Result<BTreeMap<String, MountFieldValue>> {
    fields
        .into_iter()
        .map(|(key, value)| {
            Ok((
                key.clone(),
                match value {
                    MountFieldValue::String(value) => MountFieldValue::String(
                        expand_variables(&value, variables)
                            .with_context(|| format!("Failed to expand mount field: {key}"))?,
                    ),
                    MountFieldValue::Bool(value) => MountFieldValue::Bool(value),
                },
            ))
        })
        .collect()
}

fn parse_devcontainer_mount_fields(
    mut fields: BTreeMap<String, MountFieldValue>,
    docker_options: bool,
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
    let consistency = if docker_options {
        consistency_field(&mut fields)?
    } else {
        None
    };
    let bind_options = if docker_options && mount_type == MountType::Bind {
        bind_options(&mut fields)?
    } else {
        None
    };
    let volume_options = if docker_options && mount_type == MountType::Volume {
        volume_options(&mut fields)?
    } else {
        None
    };

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
        consistency,
        bind_options,
        volume_options,
    })
}

fn consistency_field(fields: &mut BTreeMap<String, MountFieldValue>) -> Result<Option<String>> {
    let Some(consistency) = string_field(fields, "consistency")? else {
        return Ok(None);
    };

    match consistency.as_str() {
        "default" | "consistent" | "cached" | "delegated" => Ok(Some(consistency)),
        value => bail!("Unsupported mount consistency: {value}"),
    }
}

fn bind_options(
    fields: &mut BTreeMap<String, MountFieldValue>,
) -> Result<Option<MountBindOptions>> {
    let propagation = string_field(fields, "bind-propagation")?
        .map(|value| {
            value
                .parse::<MountBindOptionsPropagationEnum>()
                .map_err(|_| anyhow!("Unsupported bind propagation: {value}"))
        })
        .transpose()?;
    let create_mountpoint = bool_field(fields, "bind-create-src")?;

    if propagation.is_none() && create_mountpoint.is_none() {
        return Ok(None);
    }

    Ok(Some(MountBindOptions {
        propagation,
        create_mountpoint,
        ..Default::default()
    }))
}

fn volume_options(
    fields: &mut BTreeMap<String, MountFieldValue>,
) -> Result<Option<MountVolumeOptions>> {
    let no_copy = bool_field(fields, "volume-nocopy")?;
    let subpath = string_field(fields, "volume-subpath")?;

    if no_copy.is_none() && subpath.is_none() {
        return Ok(None);
    }

    Ok(Some(MountVolumeOptions {
        no_copy,
        subpath,
        ..Default::default()
    }))
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

fn resolve_expanded_bind_source(source: &str, options: HostPathOptions<'_>) -> Result<String> {
    Ok(resolve_expanded_host_path(source, &options)?
        .display()
        .to_string())
}

fn path_create(create: Option<MountCreate>) -> PathCreate {
    match create {
        Some(MountCreate::Directory) => PathCreate::Directory,
        None => PathCreate::None,
    }
}

fn bind_path_create(bind_options: Option<&MountBindOptions>) -> PathCreate {
    match bind_options.and_then(|options| options.create_mountpoint) {
        Some(true) => PathCreate::Directory,
        Some(false) | None => PathCreate::None,
    }
}

fn symlink_resolution(resolve_symlink: bool) -> SymlinkResolution {
    if resolve_symlink {
        SymlinkResolution::Resolve
    } else {
        SymlinkResolution::Preserve
    }
}

fn validate_target(target: &str) -> Result<String> {
    if !target.starts_with('/') {
        bail!("Mount target must be an absolute container path: {target}")
    }

    let normalized = normalize_container_path(target);
    if is_reserved_decune_target(&normalized) {
        bail!("Mount target is reserved for decune internal use: {target}");
    }

    Ok(normalized)
}

pub(crate) fn normalize_container_path(target: &str) -> String {
    let mut components = Vec::new();

    for component in target.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                components.pop();
            }
            value => components.push(value),
        }
    }

    if components.is_empty() {
        "/".to_owned()
    } else {
        format!("/{}", components.join("/"))
    }
}

fn is_reserved_decune_target(target: &str) -> bool {
    ["/opt/decune", "/run/decune"]
        .iter()
        .any(|reserved| target == *reserved || target.starts_with(&format!("{reserved}/")))
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
            Some("/home/vscode".to_owned()),
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
                consistency: None,
                bind_options: None,
                volume_options: None,
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
    fn decune_mount_replaces_devcontainer_mount_with_same_target() {
        let workspace = tempfile::tempdir().unwrap();
        let devcontainer_source = workspace.path().join("devcontainer-cache");
        let decune_source = workspace.path().join("decune-cache");
        fs::create_dir_all(&devcontainer_source).unwrap();
        fs::create_dir_all(&decune_source).unwrap();
        let config = ResolvedConfig {
            mounts: vec![ResolvedMount {
                source: Some("decune-cache".to_owned()),
                target: "/cache".to_owned(),
                mount_type: MountType::Bind,
                read_only: false,
                resolve_symlink: true,
                create: None,
                origin: ConfigPathOrigin::Project,
            }],
            devcontainer: crate::config::resolved::ResolvedDevcontainer {
                mounts: vec![LayerDevcontainerMount::String(
                    "source=${localWorkspaceFolder}/devcontainer-cache,target=/cache,type=bind,readonly=true,consistency=cached"
                        .to_owned(),
                )],
                ..Default::default()
            },
            ..ResolvedConfig::default()
        };

        let mounts =
            config_mount_specs(&config, workspace.path(), &variables(workspace.path())).unwrap();

        assert_eq!(mounts.len(), 1);
        assert_eq!(
            mounts[0].source.as_deref(),
            Some(decune_source.canonicalize().unwrap().to_str().unwrap())
        );
        assert_eq!(mounts[0].target, "/cache");
        assert!(!mounts[0].read_only);
        assert_eq!(mounts[0].consistency, None);
    }

    #[test]
    fn devcontainer_mount_replaces_global_decune_mount_with_same_target() {
        let workspace = tempfile::tempdir().unwrap();
        let global_source = workspace.path().join("global-cache");
        let devcontainer_source = workspace.path().join("devcontainer-cache");
        fs::create_dir_all(&global_source).unwrap();
        fs::create_dir_all(&devcontainer_source).unwrap();
        let config = ResolvedConfig {
            mounts: vec![ResolvedMount {
                source: Some(global_source.to_str().unwrap().to_owned()),
                target: "/cache".to_owned(),
                mount_type: MountType::Bind,
                read_only: false,
                resolve_symlink: true,
                create: None,
                origin: ConfigPathOrigin::Global,
            }],
            devcontainer: crate::config::resolved::ResolvedDevcontainer {
                mounts: vec![LayerDevcontainerMount::String(
                    "source=${localWorkspaceFolder}/devcontainer-cache,target=/cache,type=bind,readonly=true,consistency=cached"
                        .to_owned(),
                )],
                ..Default::default()
            },
            ..ResolvedConfig::default()
        };

        let mounts =
            config_mount_specs(&config, workspace.path(), &variables(workspace.path())).unwrap();

        assert_eq!(mounts.len(), 1);
        assert_eq!(
            mounts[0].source.as_deref(),
            Some(devcontainer_source.to_str().unwrap())
        );
        assert_eq!(mounts[0].target, "/cache");
        assert!(mounts[0].read_only);
        assert_eq!(mounts[0].consistency.as_deref(), Some("cached"));
    }

    #[test]
    fn devcontainer_mount_replaces_global_decune_mount_with_equivalent_target() {
        let workspace = tempfile::tempdir().unwrap();
        let global_source = workspace.path().join("global-cache");
        let devcontainer_source = workspace.path().join("devcontainer-cache");
        fs::create_dir_all(&global_source).unwrap();
        fs::create_dir_all(&devcontainer_source).unwrap();
        let config = ResolvedConfig {
            mounts: vec![ResolvedMount {
                source: Some(global_source.to_str().unwrap().to_owned()),
                target: "/cache".to_owned(),
                mount_type: MountType::Bind,
                read_only: false,
                resolve_symlink: true,
                create: None,
                origin: ConfigPathOrigin::Global,
            }],
            devcontainer: crate::config::resolved::ResolvedDevcontainer {
                mounts: vec![LayerDevcontainerMount::String(
                    "source=${localWorkspaceFolder}/devcontainer-cache,target=/cache/.,type=bind,readonly=true"
                        .to_owned(),
                )],
                ..Default::default()
            },
            ..ResolvedConfig::default()
        };

        let mounts =
            config_mount_specs(&config, workspace.path(), &variables(workspace.path())).unwrap();

        assert_eq!(mounts.len(), 1);
        assert_eq!(
            mounts[0].source.as_deref(),
            Some(devcontainer_source.to_str().unwrap())
        );
        assert_eq!(mounts[0].target, "/cache");
        assert!(mounts[0].read_only);
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
    fn parses_devcontainer_bind_mount_consistency() {
        let workspace = tempfile::tempdir().unwrap();
        let source = workspace.path().join("tools");
        fs::create_dir_all(&source).unwrap();
        let config = ResolvedConfig {
            devcontainer: crate::config::resolved::ResolvedDevcontainer {
                mounts: vec![LayerDevcontainerMount::String(
                    "source=${localWorkspaceFolder}/tools,target=/tools,type=bind,consistency=cached"
                        .to_owned(),
                )],
                ..Default::default()
            },
            ..ResolvedConfig::default()
        };

        let mounts =
            config_mount_specs(&config, workspace.path(), &variables(workspace.path())).unwrap();
        let bollard_mount = mounts[0].to_bollard_mount();

        assert_eq!(bollard_mount.consistency.as_deref(), Some("cached"));
    }

    #[test]
    fn parses_devcontainer_bind_mount_propagation() {
        let workspace = tempfile::tempdir().unwrap();
        let source = workspace.path().join("tools");
        fs::create_dir_all(&source).unwrap();
        let config = ResolvedConfig {
            devcontainer: crate::config::resolved::ResolvedDevcontainer {
                mounts: vec![LayerDevcontainerMount::String(
                    "source=${localWorkspaceFolder}/tools,target=/tools,type=bind,bind-propagation=rshared"
                        .to_owned(),
                )],
                ..Default::default()
            },
            ..ResolvedConfig::default()
        };

        let mounts =
            config_mount_specs(&config, workspace.path(), &variables(workspace.path())).unwrap();
        let bollard_mount = mounts[0].to_bollard_mount();

        assert_eq!(
            bollard_mount
                .bind_options
                .unwrap()
                .propagation
                .unwrap()
                .to_string(),
            "rshared"
        );
    }

    #[test]
    fn parses_devcontainer_bind_create_source_option() {
        let workspace = tempfile::tempdir().unwrap();
        let source = workspace.path().join("generated/cache");
        let config = ResolvedConfig {
            devcontainer: crate::config::resolved::ResolvedDevcontainer {
                mounts: vec![LayerDevcontainerMount::String(
                    "source=${localWorkspaceFolder}/generated/cache,target=/cache,type=bind,bind-create-src"
                        .to_owned(),
                )],
                ..Default::default()
            },
            ..ResolvedConfig::default()
        };

        let mounts =
            config_mount_specs(&config, workspace.path(), &variables(workspace.path())).unwrap();
        let bollard_mount = mounts[0].to_bollard_mount();

        assert!(source.is_dir());
        assert_eq!(
            bollard_mount.bind_options.unwrap().create_mountpoint,
            Some(true)
        );
    }

    #[test]
    fn parses_devcontainer_volume_mount_options() {
        let workspace = tempfile::tempdir().unwrap();
        let config = ResolvedConfig {
            devcontainer: crate::config::resolved::ResolvedDevcontainer {
                mounts: vec![LayerDevcontainerMount::String(
                    "source=project-cache,target=/cache,type=volume,volume-nocopy,volume-subpath=deps"
                        .to_owned(),
                )],
                ..Default::default()
            },
            ..ResolvedConfig::default()
        };

        let mounts =
            config_mount_specs(&config, workspace.path(), &variables(workspace.path())).unwrap();
        let volume_options = mounts[0].to_bollard_mount().volume_options.unwrap();

        assert_eq!(volume_options.no_copy, Some(true));
        assert_eq!(volume_options.subpath.as_deref(), Some("deps"));
    }

    #[test]
    fn expands_devcontainer_mount_target_before_validation() {
        let workspace = tempfile::tempdir().unwrap();
        let source = workspace.path().join("tools");
        fs::create_dir_all(&source).unwrap();
        let config = ResolvedConfig {
            devcontainer: crate::config::resolved::ResolvedDevcontainer {
                mounts: vec![LayerDevcontainerMount::String(
                    "source=${localWorkspaceFolder}/tools,target=${containerWorkspaceFolder}/tools,type=bind"
                        .to_owned(),
                )],
                ..Default::default()
            },
            ..ResolvedConfig::default()
        };

        let mounts =
            config_mount_specs(&config, workspace.path(), &variables(workspace.path())).unwrap();

        assert_eq!(mounts[0].target, "/workspaces/project/tools");
    }

    #[test]
    fn does_not_expand_devcontainer_bind_source_twice() {
        let workspace = tempfile::tempdir().unwrap();
        let source = workspace.path().join("${localWorkspaceFolderBasename}");
        fs::create_dir_all(&source).unwrap();
        let config = ResolvedConfig {
            devcontainer: crate::config::resolved::ResolvedDevcontainer {
                mounts: vec![LayerDevcontainerMount::String(
                    "source=${localEnv:DECUNE_TEST_UNSET_BRACED_DEFAULT:${localWorkspaceFolderBasename}},target=/cache,type=bind"
                        .to_owned(),
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
    }

    #[test]
    fn expands_decune_volume_mount_strings() {
        let workspace = tempfile::tempdir().unwrap();
        let config = ResolvedConfig {
            mounts: vec![ResolvedMount {
                source: Some("${localWorkspaceFolderBasename}-cache".to_owned()),
                target: "/opt/${containerWorkspaceFolderBasename}".to_owned(),
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
        assert_eq!(mounts[0].target, "/opt/project");
    }

    #[test]
    fn rejects_decune_mount_target_under_reserved_opt_decune_path() {
        let workspace = tempfile::tempdir().unwrap();
        let config = ResolvedConfig {
            mounts: vec![ResolvedMount {
                source: Some("project-cache".to_owned()),
                target: "/opt/decune/cache".to_owned(),
                mount_type: MountType::Volume,
                read_only: false,
                resolve_symlink: true,
                create: None,
                origin: ConfigPathOrigin::Project,
            }],
            ..ResolvedConfig::default()
        };

        let error = config_mount_specs(&config, workspace.path(), &variables(workspace.path()))
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Mount target is reserved for decune internal use")
        );
    }

    #[test]
    fn rejects_decune_mount_target_that_normalizes_under_reserved_opt_decune_path() {
        let workspace = tempfile::tempdir().unwrap();
        let config = ResolvedConfig {
            mounts: vec![ResolvedMount {
                source: Some("project-cache".to_owned()),
                target: "/opt/./decune//cache".to_owned(),
                mount_type: MountType::Volume,
                read_only: false,
                resolve_symlink: true,
                create: None,
                origin: ConfigPathOrigin::Project,
            }],
            ..ResolvedConfig::default()
        };

        let error = config_mount_specs(&config, workspace.path(), &variables(workspace.path()))
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Mount target is reserved for decune internal use")
        );
    }

    #[test]
    fn rejects_devcontainer_mount_target_under_reserved_run_decune_path() {
        let workspace = tempfile::tempdir().unwrap();
        let config = ResolvedConfig {
            devcontainer: crate::config::resolved::ResolvedDevcontainer {
                mounts: vec![LayerDevcontainerMount::String(
                    "source=agent,target=/run/decune/ssh-agent.sock,type=volume".to_owned(),
                )],
                ..Default::default()
            },
            ..ResolvedConfig::default()
        };

        let error = config_mount_specs(&config, workspace.path(), &variables(workspace.path()))
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Mount target is reserved for decune internal use")
        );
    }

    #[test]
    fn rejects_devcontainer_mount_target_that_normalizes_under_reserved_run_decune_path() {
        let workspace = tempfile::tempdir().unwrap();
        let config = ResolvedConfig {
            devcontainer: crate::config::resolved::ResolvedDevcontainer {
                mounts: vec![LayerDevcontainerMount::String(
                    "source=agent,target=/run//decune/./ssh-agent.sock,type=volume".to_owned(),
                )],
                ..Default::default()
            },
            ..ResolvedConfig::default()
        };

        let error = config_mount_specs(&config, workspace.path(), &variables(workspace.path()))
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Mount target is reserved for decune internal use")
        );
    }

    #[test]
    fn allows_targets_outside_reserved_prefix_boundary() {
        let workspace = tempfile::tempdir().unwrap();
        let config = ResolvedConfig {
            mounts: vec![ResolvedMount {
                source: Some("project-cache".to_owned()),
                target: "/opt/decune-cache".to_owned(),
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

        assert_eq!(mounts[0].target, "/opt/decune-cache");
    }

    #[test]
    fn rejects_mount_target_that_expands_to_reserved_path() {
        let workspace = tempfile::tempdir().unwrap();
        let config = ResolvedConfig {
            mounts: vec![ResolvedMount {
                source: Some("project-cache".to_owned()),
                target: "/run/decune/${remoteUser}".to_owned(),
                mount_type: MountType::Volume,
                read_only: false,
                resolve_symlink: true,
                create: None,
                origin: ConfigPathOrigin::Project,
            }],
            ..ResolvedConfig::default()
        };

        let error = config_mount_specs(&config, workspace.path(), &variables(workspace.path()))
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Mount target is reserved for decune internal use")
        );
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
    fn parses_devcontainer_object_bind_mount_options() {
        let workspace = tempfile::tempdir().unwrap();
        let source = workspace.path().join("generated/cache");
        let config = ResolvedConfig {
            devcontainer: crate::config::resolved::ResolvedDevcontainer {
                mounts: vec![LayerDevcontainerMount::Object(
                    [
                        ("type".to_owned(), json!("bind")),
                        (
                            "source".to_owned(),
                            json!("${localWorkspaceFolder}/generated/cache"),
                        ),
                        ("target".to_owned(), json!("/cache")),
                        ("consistency".to_owned(), json!("cached")),
                        ("bind-propagation".to_owned(), json!("rshared")),
                        ("bind-create-src".to_owned(), json!(true)),
                    ]
                    .into(),
                )],
                ..Default::default()
            },
            ..ResolvedConfig::default()
        };

        let mounts =
            config_mount_specs(&config, workspace.path(), &variables(workspace.path())).unwrap();
        let bollard_mount = mounts[0].to_bollard_mount();
        let bind_options = bollard_mount.bind_options.unwrap();

        assert!(source.is_dir());
        assert_eq!(bollard_mount.consistency.as_deref(), Some("cached"));
        assert_eq!(bind_options.propagation.unwrap().to_string(), "rshared");
        assert_eq!(bind_options.create_mountpoint, Some(true));
    }

    #[test]
    fn parses_devcontainer_object_volume_mount_options() {
        let workspace = tempfile::tempdir().unwrap();
        let config = ResolvedConfig {
            devcontainer: crate::config::resolved::ResolvedDevcontainer {
                mounts: vec![LayerDevcontainerMount::Object(
                    [
                        ("type".to_owned(), json!("volume")),
                        ("source".to_owned(), json!("project-cache")),
                        ("target".to_owned(), json!("/cache")),
                        ("volume-nocopy".to_owned(), json!(true)),
                        ("volume-subpath".to_owned(), json!("deps")),
                    ]
                    .into(),
                )],
                ..Default::default()
            },
            ..ResolvedConfig::default()
        };

        let mounts =
            config_mount_specs(&config, workspace.path(), &variables(workspace.path())).unwrap();
        let volume_options = mounts[0].to_bollard_mount().volume_options.unwrap();

        assert_eq!(volume_options.no_copy, Some(true));
        assert_eq!(volume_options.subpath.as_deref(), Some("deps"));
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
