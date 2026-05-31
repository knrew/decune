#![allow(dead_code)]

use std::collections::BTreeMap;

use anyhow::{Result, anyhow};
use serde::Deserialize;
use serde_json::Value;

use crate::{
    config::layer::{
        ConfigLayer, LayerDevcontainerBuild, LayerDevcontainerMetadata, LayerDevcontainerSource,
        LayerFeature, LayerRunArg, LayerUserEnvProbe,
    },
    devcontainer::lifecycle::parse_lifecycle_layer_definition,
    devcontainer::mounts::DevcontainerMount,
    devcontainer::ports::{
        DevcontainerPort, DevcontainerPortAttributes, forwarding_port_to_layer,
        port_attributes_to_layer, publish_port_to_layer,
    },
};

pub(crate) fn parse_metadata(value: Value) -> Result<DevcontainerMetadata> {
    let raw: RawDevcontainerMetadata = serde_json::from_value(value)
        .map_err(|error| anyhow!("Failed to parse devcontainer metadata schema: {error}"))?;

    raw.validate(
        SourceRequirement::Required,
        MetadataLayerKind::DevcontainerJson,
    )
}

pub(crate) fn parse_metadata_layer(value: Value) -> Result<DevcontainerMetadata> {
    let raw: RawDevcontainerMetadata = serde_json::from_value(value)
        .map_err(|error| anyhow!("Failed to parse devcontainer metadata schema: {error}"))?;

    raw.validate(SourceRequirement::Optional, MetadataLayerKind::Generic)
}

pub(crate) fn parse_image_metadata_layer(value: Value) -> Result<DevcontainerMetadata> {
    let raw: RawDevcontainerMetadata = serde_json::from_value(value)
        .map_err(|error| anyhow!("Failed to parse devcontainer metadata schema: {error}"))?;

    raw.validate(SourceRequirement::Optional, MetadataLayerKind::ImageLabel)
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DevcontainerMetadata {
    source: Option<DevcontainerSource>,
    features: BTreeMap<String, Value>,
    override_feature_install_order: Vec<String>,
    mounts: Vec<DevcontainerMount>,
    workspace_mount: Option<String>,
    workspace_folder: Option<String>,
    container_env: BTreeMap<String, String>,
    remote_env: BTreeMap<String, String>,
    remote_user: Option<String>,
    container_user: Option<String>,
    update_remote_user_uid: Option<bool>,
    override_command: Option<bool>,
    user_env_probe: Option<UserEnvProbe>,
    forward_ports: Vec<DevcontainerPort>,
    ports_attributes: BTreeMap<String, DevcontainerPortAttributes>,
    other_ports_attributes: Option<DevcontainerPortAttributes>,
    app_port: Vec<DevcontainerPort>,
    run_args: Vec<DevcontainerRunArg>,
    init: Option<bool>,
    privileged: Option<bool>,
    cap_add: Vec<String>,
    security_opt: Vec<String>,
    lifecycle: BTreeMap<LifecycleProperty, Value>,
    customizations: Option<Value>,
    unsupported_properties: BTreeMap<String, Value>,
}

impl DevcontainerMetadata {
    pub(crate) fn source(&self) -> Option<&DevcontainerSource> {
        self.source.as_ref()
    }

    pub(crate) fn mounts(&self) -> &[DevcontainerMount] {
        &self.mounts
    }

    pub(crate) fn features(&self) -> &BTreeMap<String, Value> {
        &self.features
    }

    pub(crate) fn override_feature_install_order(&self) -> &[String] {
        &self.override_feature_install_order
    }

    pub(crate) fn workspace_mount(&self) -> Option<&str> {
        self.workspace_mount.as_deref()
    }

    pub(crate) fn workspace_folder(&self) -> Option<&str> {
        self.workspace_folder.as_deref()
    }

    pub(crate) fn container_env(&self) -> &BTreeMap<String, String> {
        &self.container_env
    }

    pub(crate) fn remote_env(&self) -> &BTreeMap<String, String> {
        &self.remote_env
    }

    pub(crate) fn remote_user(&self) -> Option<&str> {
        self.remote_user.as_deref()
    }

    pub(crate) fn container_user(&self) -> Option<&str> {
        self.container_user.as_deref()
    }

    pub(crate) fn update_remote_user_uid(&self) -> Option<bool> {
        self.update_remote_user_uid
    }

    pub(crate) fn user_env_probe(&self) -> Option<&UserEnvProbe> {
        self.user_env_probe.as_ref()
    }

    pub(crate) fn forward_ports(&self) -> &[DevcontainerPort] {
        &self.forward_ports
    }

    pub(crate) fn ports_attributes(&self) -> &BTreeMap<String, DevcontainerPortAttributes> {
        &self.ports_attributes
    }

    pub(crate) fn other_ports_attributes(&self) -> Option<&DevcontainerPortAttributes> {
        self.other_ports_attributes.as_ref()
    }

    pub(crate) fn app_port(&self) -> &[DevcontainerPort] {
        &self.app_port
    }

    pub(crate) fn run_args(&self) -> &[DevcontainerRunArg] {
        &self.run_args
    }

    pub(crate) fn init(&self) -> Option<bool> {
        self.init
    }

    pub(crate) fn privileged(&self) -> Option<bool> {
        self.privileged
    }

    pub(crate) fn cap_add(&self) -> &[String] {
        &self.cap_add
    }

    pub(crate) fn security_opt(&self) -> &[String] {
        &self.security_opt
    }

    pub(crate) fn lifecycle(&self) -> &BTreeMap<LifecycleProperty, Value> {
        &self.lifecycle
    }

    pub(crate) fn customizations(&self) -> Option<&Value> {
        self.customizations.as_ref()
    }

    pub(crate) fn unsupported_properties(&self) -> &BTreeMap<String, Value> {
        &self.unsupported_properties
    }

    pub(crate) fn to_config_layer(&self) -> Result<ConfigLayer> {
        self.to_config_layer_with_forward_ports(true)
    }

    pub(crate) fn to_config_layer_without_forward_ports(&self) -> Result<ConfigLayer> {
        self.to_config_layer_with_forward_ports(false)
    }

    fn to_config_layer_with_forward_ports(
        &self,
        include_forward_ports: bool,
    ) -> Result<ConfigLayer> {
        let mut layer = ConfigLayer {
            features: self
                .features
                .iter()
                .map(|(id, value)| feature_to_layer(id, value))
                .collect::<Result<Vec<_>>>()?,
            forward_ports: if include_forward_ports {
                self.forward_ports
                    .iter()
                    .map(|port| forwarding_port_to_layer(port, &self.ports_attributes))
                    .collect::<Result<Vec<_>>>()?
            } else {
                Vec::new()
            },
            devcontainer: Some(self.to_devcontainer_layer()?),
            ..ConfigLayer::default()
        };

        for run_arg in &self.run_args {
            match run_arg {
                DevcontainerRunArg::Init => {
                    if let Some(devcontainer) = &mut layer.devcontainer {
                        devcontainer.init = Some(true);
                    }
                }
                DevcontainerRunArg::Privileged => {
                    if let Some(devcontainer) = &mut layer.devcontainer {
                        devcontainer.privileged = Some(true);
                    }
                }
                DevcontainerRunArg::CapAdd(capability) => {
                    if let Some(devcontainer) = &mut layer.devcontainer {
                        devcontainer.cap_add.push(capability.clone());
                    }
                }
                DevcontainerRunArg::SecurityOpt(option) => {
                    if let Some(devcontainer) = &mut layer.devcontainer {
                        devcontainer.security_opt.push(option.clone());
                    }
                }
                DevcontainerRunArg::AddHost(value) => {
                    if let Some(devcontainer) = &mut layer.devcontainer {
                        devcontainer
                            .run_args
                            .push(LayerRunArg::AddHost(value.clone()));
                    }
                }
                DevcontainerRunArg::Dns(value) => {
                    if let Some(devcontainer) = &mut layer.devcontainer {
                        devcontainer.run_args.push(LayerRunArg::Dns(value.clone()));
                    }
                }
                DevcontainerRunArg::DnsSearch(value) => {
                    if let Some(devcontainer) = &mut layer.devcontainer {
                        devcontainer
                            .run_args
                            .push(LayerRunArg::DnsSearch(value.clone()));
                    }
                }
            }
        }

        Ok(layer)
    }

    fn to_devcontainer_layer(&self) -> Result<LayerDevcontainerMetadata> {
        Ok(LayerDevcontainerMetadata {
            source: self.source.as_ref().map(devcontainer_source_to_layer),
            override_feature_install_order: self.override_feature_install_order.clone(),
            mounts: self
                .mounts
                .iter()
                .map(DevcontainerMount::to_layer)
                .collect(),
            workspace_mount: self.workspace_mount.clone(),
            workspace_folder: self.workspace_folder.clone(),
            container_env: self.container_env.clone(),
            remote_env: self.remote_env.clone(),
            remote_user: self.remote_user.clone(),
            container_user: self.container_user.clone(),
            update_remote_user_uid: self.update_remote_user_uid,
            override_command: self.override_command,
            user_env_probe: self.user_env_probe.as_ref().map(user_env_probe_to_layer),
            publish_ports: self
                .app_port
                .iter()
                .map(publish_port_to_layer)
                .collect::<Result<Vec<_>>>()?,
            port_attributes: self
                .ports_attributes
                .iter()
                .map(|(key, attributes)| Ok((key.clone(), port_attributes_to_layer(attributes))))
                .collect::<Result<BTreeMap<_, _>>>()?,
            other_ports_attributes: self
                .other_ports_attributes
                .as_ref()
                .map(port_attributes_to_layer),
            run_args: Vec::new(),
            init: self.init,
            privileged: self.privileged,
            cap_add: self.cap_add.clone(),
            security_opt: self.security_opt.clone(),
            lifecycle: parse_lifecycle_layer_definition(&self.lifecycle)?,
        })
    }
}

fn devcontainer_source_to_layer(source: &DevcontainerSource) -> LayerDevcontainerSource {
    match source {
        DevcontainerSource::Image(image) => LayerDevcontainerSource::Image(image.clone()),
        DevcontainerSource::Dockerfile(build) => {
            LayerDevcontainerSource::Dockerfile(LayerDevcontainerBuild {
                dockerfile: build.dockerfile.clone(),
                context: build.context.clone(),
                args: build.args.clone(),
                target: build.target.clone(),
                cache_from: build.cache_from.clone(),
            })
        }
    }
}

fn feature_to_layer(id: &str, value: &Value) -> Result<LayerFeature> {
    let mut feature = LayerFeature::new(id.to_owned());

    match value {
        Value::Object(options) => {
            feature.options = options
                .iter()
                .map(|(key, value)| Ok((key.clone(), json_to_toml(value)?)))
                .collect::<Result<BTreeMap<_, _>>>()?;
        }
        Value::Bool(enabled) => {
            feature.enabled = *enabled;
        }
        Value::Null => {}
        _ => {
            return Err(anyhow!(
                "Feature {id} value must be an object, boolean, or null"
            ));
        }
    }

    Ok(feature)
}

fn json_to_toml(value: &Value) -> Result<toml::Value> {
    match value {
        Value::String(value) => Ok(toml::Value::String(value.clone())),
        Value::Bool(value) => Ok(toml::Value::Boolean(*value)),
        Value::Number(value) => {
            if let Some(value) = value.as_i64() {
                Ok(toml::Value::Integer(value))
            } else if let Some(value) = value.as_f64() {
                Ok(toml::Value::Float(value))
            } else {
                Err(anyhow!("JSON number cannot be represented as TOML"))
            }
        }
        Value::Array(values) => values
            .iter()
            .map(json_to_toml)
            .collect::<Result<Vec<_>>>()
            .map(toml::Value::Array),
        Value::Object(values) => values
            .iter()
            .map(|(key, value)| Ok((key.clone(), json_to_toml(value)?)))
            .collect::<Result<toml::map::Map<_, _>>>()
            .map(toml::Value::Table),
        Value::Null => Err(anyhow!("JSON null cannot be represented as TOML")),
    }
}

fn user_env_probe_to_layer(value: &UserEnvProbe) -> LayerUserEnvProbe {
    match value {
        UserEnvProbe::None => LayerUserEnvProbe::None,
        UserEnvProbe::LoginShell => LayerUserEnvProbe::LoginShell,
        UserEnvProbe::InteractiveShell => LayerUserEnvProbe::InteractiveShell,
        UserEnvProbe::LoginInteractiveShell => LayerUserEnvProbe::LoginInteractiveShell,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DevcontainerSource {
    Image(String),
    Dockerfile(DevcontainerBuild),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DevcontainerBuild {
    pub(crate) dockerfile: String,
    pub(crate) context: Option<String>,
    pub(crate) args: std::collections::BTreeMap<String, String>,
    pub(crate) target: Option<String>,
    pub(crate) cache_from: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DevcontainerRunArg {
    Init,
    Privileged,
    CapAdd(String),
    SecurityOpt(String),
    AddHost(String),
    Dns(String),
    DnsSearch(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum UserEnvProbe {
    None,
    LoginShell,
    InteractiveShell,
    LoginInteractiveShell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum LifecycleProperty {
    InitializeCommand,
    OnCreateCommand,
    UpdateContentCommand,
    PostCreateCommand,
    PostStartCommand,
    PostAttachCommand,
    WaitFor,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDevcontainerMetadata {
    image: Option<String>,
    build: Option<RawDevcontainerBuild>,
    docker_compose_file: Option<Value>,
    service: Option<String>,
    #[serde(default)]
    features: BTreeMap<String, Value>,
    #[serde(default)]
    override_feature_install_order: Vec<String>,
    #[serde(default)]
    mounts: Vec<DevcontainerMount>,
    workspace_mount: Option<String>,
    workspace_folder: Option<String>,
    #[serde(default)]
    container_env: BTreeMap<String, String>,
    #[serde(default)]
    remote_env: BTreeMap<String, String>,
    remote_user: Option<String>,
    container_user: Option<String>,
    #[serde(rename = "updateRemoteUserUID")]
    update_remote_user_uid: Option<bool>,
    override_command: Option<bool>,
    user_env_probe: Option<UserEnvProbe>,
    #[serde(default)]
    forward_ports: Vec<DevcontainerPort>,
    #[serde(default)]
    ports_attributes: BTreeMap<String, DevcontainerPortAttributes>,
    other_ports_attributes: Option<DevcontainerPortAttributes>,
    #[serde(default, deserialize_with = "deserialize_ports")]
    app_port: Vec<DevcontainerPort>,
    #[serde(default)]
    run_args: Vec<String>,
    init: Option<bool>,
    privileged: Option<bool>,
    #[serde(default, rename = "capAdd")]
    cap_add: Vec<String>,
    #[serde(default, rename = "securityOpt")]
    security_opt: Vec<String>,
    initialize_command: Option<Value>,
    on_create_command: Option<Value>,
    update_content_command: Option<Value>,
    post_create_command: Option<Value>,
    post_start_command: Option<Value>,
    post_attach_command: Option<Value>,
    wait_for: Option<Value>,
    customizations: Option<Value>,
    #[serde(flatten)]
    unsupported_properties: BTreeMap<String, Value>,
}

impl RawDevcontainerMetadata {
    fn validate(
        self,
        source_requirement: SourceRequirement,
        layer_kind: MetadataLayerKind,
    ) -> Result<DevcontainerMetadata> {
        if self.docker_compose_file.is_some() || self.service.is_some() {
            return Err(anyhow!(
                "Docker Compose mode is not supported in decune v0.1"
            ));
        }

        if layer_kind == MetadataLayerKind::ImageLabel && self.initialize_command.is_some() {
            return Err(anyhow!(
                "Image devcontainer metadata must not specify initializeCommand"
            ));
        }

        let lifecycle = self.lifecycle_values();
        let source = match (self.image, self.build) {
            (Some(_), Some(_)) => {
                return Err(anyhow!(
                    "Devcontainer metadata must not specify both image and build"
                ));
            }
            (Some(image), None) => Some(DevcontainerSource::Image(image)),
            (None, Some(build)) => Some(DevcontainerSource::Dockerfile(build.validate()?)),
            (None, None) if source_requirement == SourceRequirement::Required => {
                return Err(anyhow!(
                    "Devcontainer metadata must specify either image or build"
                ));
            }
            (None, None) => None,
        };

        Ok(DevcontainerMetadata {
            source,
            features: self.features,
            override_feature_install_order: self.override_feature_install_order,
            mounts: self.mounts,
            workspace_mount: self.workspace_mount,
            workspace_folder: self.workspace_folder,
            container_env: self.container_env,
            remote_env: self.remote_env,
            remote_user: self.remote_user,
            container_user: self.container_user,
            update_remote_user_uid: self.update_remote_user_uid,
            override_command: self.override_command,
            user_env_probe: self.user_env_probe,
            forward_ports: self.forward_ports,
            ports_attributes: self.ports_attributes,
            other_ports_attributes: self.other_ports_attributes,
            app_port: self.app_port,
            run_args: normalize_run_args(&self.run_args)?,
            init: self.init,
            privileged: self.privileged,
            cap_add: self.cap_add,
            security_opt: self.security_opt,
            lifecycle,
            customizations: self.customizations,
            unsupported_properties: self.unsupported_properties,
        })
    }

    fn lifecycle_values(&self) -> BTreeMap<LifecycleProperty, Value> {
        [
            (
                LifecycleProperty::InitializeCommand,
                self.initialize_command.clone(),
            ),
            (
                LifecycleProperty::OnCreateCommand,
                self.on_create_command.clone(),
            ),
            (
                LifecycleProperty::UpdateContentCommand,
                self.update_content_command.clone(),
            ),
            (
                LifecycleProperty::PostCreateCommand,
                self.post_create_command.clone(),
            ),
            (
                LifecycleProperty::PostStartCommand,
                self.post_start_command.clone(),
            ),
            (
                LifecycleProperty::PostAttachCommand,
                self.post_attach_command.clone(),
            ),
            (LifecycleProperty::WaitFor, self.wait_for.clone()),
        ]
        .into_iter()
        .filter_map(|(key, value)| value.map(|value| (key, value)))
        .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceRequirement {
    Required,
    Optional,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MetadataLayerKind {
    DevcontainerJson,
    Generic,
    ImageLabel,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDevcontainerBuild {
    dockerfile: String,
    context: Option<String>,
    #[serde(default, deserialize_with = "deserialize_build_args")]
    args: BTreeMap<String, String>,
    target: Option<String>,
    #[serde(default, deserialize_with = "deserialize_string_or_strings")]
    cache_from: Vec<String>,
}

impl RawDevcontainerBuild {
    fn validate(self) -> Result<DevcontainerBuild> {
        Ok(DevcontainerBuild {
            dockerfile: self.dockerfile,
            context: self.context,
            args: self.args,
            target: self.target,
            cache_from: self.cache_from,
        })
    }
}

fn normalize_run_args(values: &[String]) -> Result<Vec<DevcontainerRunArg>> {
    let mut args = Vec::new();
    let mut index = 0;

    while index < values.len() {
        let current = &values[index];
        if let Some((option, value)) = current.split_once('=') {
            args.push(run_arg_with_value(option, value.to_owned())?);
            index += 1;
            continue;
        }

        match current.as_str() {
            "--init" => args.push(DevcontainerRunArg::Init),
            "--privileged" => args.push(DevcontainerRunArg::Privileged),
            "--cap-add" | "--security-opt" | "--add-host" | "--dns" | "--dns-search" => {
                let value = required_run_arg_value(values, current, index)?;
                args.push(run_arg_with_value(current, value)?);
                index += 1;
            }
            _ => return Err(anyhow!("Unsupported runArgs option: {current}")),
        }

        index += 1;
    }

    Ok(args)
}

fn required_run_arg_value(values: &[String], option: &str, index: usize) -> Result<String> {
    let value = values
        .get(index + 1)
        .ok_or_else(|| anyhow!("Missing value for runArgs option {option}"))?;

    if value.is_empty() || value.starts_with('-') {
        return Err(anyhow!("Missing value for runArgs option {option}"));
    }

    Ok(value.clone())
}

fn run_arg_with_value(option: &str, value: String) -> Result<DevcontainerRunArg> {
    if value.is_empty() {
        return Err(anyhow!("Missing value for runArgs option {option}"));
    }

    match option {
        "--cap-add" => Ok(DevcontainerRunArg::CapAdd(value)),
        "--security-opt" => Ok(DevcontainerRunArg::SecurityOpt(value)),
        "--add-host" => Ok(DevcontainerRunArg::AddHost(value)),
        "--dns" => Ok(DevcontainerRunArg::Dns(value)),
        "--dns-search" => Ok(DevcontainerRunArg::DnsSearch(value)),
        "--init" | "--privileged" => {
            Err(anyhow!("runArgs option {option} does not accept a value"))
        }
        _ => Err(anyhow!("Unsupported runArgs option: {option}")),
    }
}

fn deserialize_build_args<'de, D>(
    deserializer: D,
) -> std::result::Result<BTreeMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = BTreeMap::<String, Value>::deserialize(deserializer)?;
    let mut args = BTreeMap::new();

    for (key, value) in value {
        let value = value.as_str().ok_or_else(|| {
            serde::de::Error::custom(format!("build.args.{key} must be a string"))
        })?;
        args.insert(key, value.to_owned());
    }

    Ok(args)
}

fn deserialize_ports<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<DevcontainerPort>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;

    match value {
        None => Ok(Vec::new()),
        Some(Value::Array(values)) => values
            .into_iter()
            .map(serde_json::from_value)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(serde::de::Error::custom),
        Some(value) => serde_json::from_value(value)
            .map(|port| vec![port])
            .map_err(serde::de::Error::custom),
    }
}

fn deserialize_string_or_strings<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;

    match value {
        None => Ok(Vec::new()),
        Some(Value::String(value)) => Ok(vec![value]),
        Some(Value::Array(values)) => values
            .into_iter()
            .map(serde_json::from_value)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(serde::de::Error::custom),
        Some(_) => Err(serde::de::Error::custom("expected string or string array")),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::config::{
        layer::{ConfigMergeInput, LayerFeature, LayerPort},
        merge::resolve_config,
        resolved::ResolvedPublishPort,
        types::{DEFAULT_PORT_HOST_IP, OnAutoForward as ConfigOnAutoForward, PortProtocol},
    };
    use crate::devcontainer::lifecycle::{LifecycleCommand, LifecycleStage, WaitFor};
    use crate::devcontainer::mounts::DevcontainerMount;
    use crate::devcontainer::ports::OnAutoForward;
    use toml::Value as TomlValue;

    use super::*;

    #[test]
    fn parses_image_based_metadata() {
        let metadata = parse_metadata(json!({
            "image": "mcr.microsoft.com/devcontainers/rust:1-1-bookworm",
            "workspaceFolder": "/workspaces/decune",
            "containerEnv": {
                "RUST_LOG": "debug"
            },
            "remoteUser": "vscode",
            "updateRemoteUserUID": false,
            "customizations": {
                "vscode": {
                    "extensions": ["rust-lang.rust-analyzer"]
                }
            }
        }))
        .unwrap();

        assert_eq!(
            metadata.source(),
            Some(&DevcontainerSource::Image(
                "mcr.microsoft.com/devcontainers/rust:1-1-bookworm".to_owned()
            ))
        );
        assert_eq!(metadata.workspace_folder(), Some("/workspaces/decune"));
        assert_eq!(
            metadata.container_env().get("RUST_LOG").map(String::as_str),
            Some("debug")
        );
        assert_eq!(metadata.remote_user(), Some("vscode"));
        assert_eq!(metadata.update_remote_user_uid(), Some(false));
        assert!(metadata.customizations().is_some());
    }

    #[test]
    fn devcontainer_json_allows_initialize_command() {
        let metadata = parse_metadata(json!({
            "image": "ubuntu:24.04",
            "initializeCommand": "scripts/init.sh"
        }))
        .unwrap();
        let layer = metadata.to_config_layer().unwrap();
        let lifecycle = layer.devcontainer.unwrap().lifecycle.unwrap();
        let resolved = lifecycle.into_resolved();

        assert_eq!(
            resolved.command(LifecycleStage::Initialize),
            Some(&LifecycleCommand::Shell("scripts/init.sh".to_owned()))
        );
    }

    #[test]
    fn parses_dockerfile_based_metadata() {
        let metadata = parse_metadata(json!({
            "build": {
                "dockerfile": "Dockerfile",
                "context": "..",
                "args": {
                    "VARIANT": "bookworm"
                },
                "target": "dev",
                "cacheFrom": ["type=registry,ref=example.test/cache"]
            },
            "workspaceMount": "source=${localWorkspaceFolder},target=/workspace,type=bind",
            "mounts": [
                "source=decune-cache,target=/cache,type=volume"
            ]
        }))
        .unwrap();

        assert_eq!(
            metadata.source(),
            Some(&DevcontainerSource::Dockerfile(DevcontainerBuild {
                dockerfile: "Dockerfile".to_owned(),
                context: Some("..".to_owned()),
                args: [("VARIANT".to_owned(), "bookworm".to_owned())].into(),
                target: Some("dev".to_owned()),
                cache_from: vec!["type=registry,ref=example.test/cache".to_owned()],
            }))
        );
        assert_eq!(
            metadata.workspace_mount(),
            Some("source=${localWorkspaceFolder},target=/workspace,type=bind")
        );
        assert_eq!(
            metadata.mounts(),
            &[DevcontainerMount::String(
                "source=decune-cache,target=/cache,type=volume".to_owned()
            )]
        );
    }

    #[test]
    fn parses_object_mounts_without_interpreting_them() {
        let metadata = parse_metadata(json!({
            "image": "ubuntu:24.04",
            "mounts": [
                {
                    "type": "bind",
                    "source": "/host/cache",
                    "target": "/cache",
                    "readonly": true
                }
            ]
        }))
        .unwrap();

        match &metadata.mounts()[0] {
            DevcontainerMount::Object(values) => {
                assert_eq!(values.get("type"), Some(&json!("bind")));
                assert_eq!(values.get("source"), Some(&json!("/host/cache")));
                assert_eq!(values.get("target"), Some(&json!("/cache")));
                assert_eq!(values.get("readonly"), Some(&json!(true)));
            }
            DevcontainerMount::String(_) => panic!("expected object mount"),
        }

        let config = resolve_config(ConfigMergeInput {
            devcontainer: Some(metadata.to_config_layer().unwrap()),
            ..ConfigMergeInput::default()
        });

        assert_eq!(config.devcontainer.mounts.len(), 1);
    }

    #[test]
    fn parses_supported_runtime_and_forwarding_properties() {
        let metadata = parse_metadata(json!({
            "image": "ubuntu:24.04",
            "remoteEnv": {
                "PATH": "/custom/bin:${containerEnv:PATH}"
            },
            "containerUser": "root",
            "userEnvProbe": "loginInteractiveShell",
            "forwardPorts": [3000, "localhost:5432"],
            "portsAttributes": {
                "3000": {
                    "label": "web",
                    "onAutoForward": "notify",
                    "requireLocalPort": true
                }
            },
            "otherPortsAttributes": {
                "onAutoForward": "ignore"
            },
            "appPort": [8080, "127.0.0.1:8443:443"],
            "init": true,
            "privileged": true,
            "capAdd": ["SYS_PTRACE"],
            "securityOpt": ["seccomp=unconfined"],
            "postStartCommand": "echo ready",
            "x-extra": {
                "preserved": true
            }
        }))
        .unwrap();

        assert_eq!(
            metadata.remote_env().get("PATH").map(String::as_str),
            Some("/custom/bin:${containerEnv:PATH}")
        );
        assert_eq!(metadata.container_user(), Some("root"));
        assert_eq!(
            metadata.user_env_probe(),
            Some(&UserEnvProbe::LoginInteractiveShell)
        );
        assert_eq!(
            metadata.forward_ports(),
            &[
                DevcontainerPort::Number(3000),
                DevcontainerPort::String("localhost:5432".to_owned())
            ]
        );
        assert_eq!(
            metadata
                .ports_attributes()
                .get("3000")
                .and_then(|attributes| attributes.label.as_deref()),
            Some("web")
        );
        assert_eq!(
            metadata
                .other_ports_attributes()
                .and_then(|attributes| attributes.on_auto_forward.as_ref()),
            Some(&OnAutoForward::Ignore)
        );
        assert_eq!(
            metadata.app_port(),
            &[
                DevcontainerPort::Number(8080),
                DevcontainerPort::String("127.0.0.1:8443:443".to_owned())
            ]
        );
        assert_eq!(metadata.init(), Some(true));
        assert_eq!(metadata.privileged(), Some(true));
        assert_eq!(metadata.cap_add(), &["SYS_PTRACE".to_owned()]);
        assert_eq!(metadata.security_opt(), &["seccomp=unconfined".to_owned()]);
        assert_eq!(
            metadata
                .lifecycle()
                .get(&LifecycleProperty::PostStartCommand),
            Some(&json!("echo ready"))
        );
        assert!(metadata.unsupported_properties().contains_key("x-extra"));
    }

    #[test]
    fn compose_mode_is_rejected() {
        for value in [
            json!({"dockerComposeFile": "compose.yml", "service": "app"}),
            json!({"service": "app", "image": "ubuntu:24.04"}),
        ] {
            let error = parse_metadata(value).unwrap_err();

            assert!(error.to_string().contains("Docker Compose mode"));
        }
    }

    #[test]
    fn docker_compose_file_alone_is_rejected() {
        let error = parse_metadata(json!({
            "dockerComposeFile": "compose.yml"
        }))
        .unwrap_err();

        assert!(error.to_string().contains("Docker Compose mode"));
    }

    #[test]
    fn metadata_without_image_or_build_is_rejected() {
        let error = parse_metadata(json!({
            "features": {}
        }))
        .unwrap_err();

        assert!(error.to_string().contains("either image or build"));
    }

    #[test]
    fn source_less_metadata_layer_is_accepted() {
        let metadata = parse_metadata_layer(json!({
            "containerEnv": {
                "FEATURE_FLAG": "enabled"
            },
            "remoteUser": "vscode",
            "postCreateCommand": "feature setup"
        }))
        .unwrap();

        let config = resolve_config(ConfigMergeInput {
            image_metadata: vec![metadata.to_config_layer().unwrap()],
            ..ConfigMergeInput::default()
        });

        assert!(config.devcontainer.source.is_none());
        assert_eq!(
            config
                .devcontainer
                .container_env
                .get("FEATURE_FLAG")
                .map(String::as_str),
            Some("enabled")
        );
        assert_eq!(config.devcontainer.remote_user.as_deref(), Some("vscode"));
        assert!(config.devcontainer.lifecycle.is_some());
    }

    #[test]
    fn image_and_build_are_rejected_together() {
        let error = parse_metadata(json!({
            "image": "ubuntu:24.04",
            "build": {
                "dockerfile": "Dockerfile"
            }
        }))
        .unwrap_err();

        assert!(error.to_string().contains("image and build"));
    }

    #[test]
    fn build_args_must_be_strings() {
        let error = parse_metadata(json!({
            "build": {
                "dockerfile": "Dockerfile",
                "args": {
                    "UID": 1000
                }
            }
        }))
        .unwrap_err();

        assert!(error.to_string().contains("build.args"));
    }

    #[test]
    fn supported_run_args_are_normalized() {
        let metadata = parse_metadata(json!({
            "image": "ubuntu:24.04",
            "runArgs": [
                "--init",
                "--privileged",
                "--cap-add=SYS_PTRACE",
                "--security-opt", "seccomp=unconfined",
                "--add-host", "host.docker.internal:host-gateway",
                "--dns", "1.1.1.1",
                "--dns-search=example.test"
            ]
        }))
        .unwrap();

        assert_eq!(
            metadata.run_args(),
            &[
                DevcontainerRunArg::Init,
                DevcontainerRunArg::Privileged,
                DevcontainerRunArg::CapAdd("SYS_PTRACE".to_owned()),
                DevcontainerRunArg::SecurityOpt("seccomp=unconfined".to_owned()),
                DevcontainerRunArg::AddHost("host.docker.internal:host-gateway".to_owned()),
                DevcontainerRunArg::Dns("1.1.1.1".to_owned()),
                DevcontainerRunArg::DnsSearch("example.test".to_owned()),
            ]
        );
    }

    #[test]
    fn unsupported_run_args_are_rejected() {
        for run_args in [
            json!(["--publish", "3000:3000"]),
            json!(["-p", "3000:3000"]),
            json!(["--mount", "type=bind,source=/tmp,target=/tmp"]),
            json!(["--user", "vscode"]),
            json!(["--env", "RUST_LOG=debug"]),
        ] {
            let error = parse_metadata(json!({
                "image": "ubuntu:24.04",
                "runArgs": run_args
            }))
            .unwrap_err();

            assert!(error.to_string().contains("Unsupported runArgs option"));
        }
    }

    #[test]
    fn run_args_missing_values_are_rejected() {
        for run_args in [
            json!(["--cap-add"]),
            json!(["--security-opt"]),
            json!(["--add-host"]),
            json!(["--dns"]),
            json!(["--dns-search"]),
        ] {
            let error = parse_metadata(json!({
                "image": "ubuntu:24.04",
                "runArgs": run_args
            }))
            .unwrap_err();

            assert!(error.to_string().contains("Missing value"));
        }
    }

    #[test]
    fn run_args_value_options_reject_following_options_as_values() {
        for run_args in [
            json!(["--cap-add", "--init"]),
            json!(["--security-opt", "--privileged"]),
            json!(["--add-host", "--dns", "1.1.1.1"]),
            json!(["--dns", "--dns-search", "example.test"]),
            json!(["--dns-search", "--init"]),
        ] {
            let error = parse_metadata(json!({
                "image": "ubuntu:24.04",
                "runArgs": run_args
            }))
            .unwrap_err();

            assert!(error.to_string().contains("Missing value"));
        }
    }

    #[test]
    fn run_args_boolean_options_reject_values() {
        for run_args in [
            json!(["--init=true"]),
            json!(["--privileged=false"]),
            json!(["--init", "true"]),
        ] {
            let error = parse_metadata(json!({
                "image": "ubuntu:24.04",
                "runArgs": run_args
            }))
            .unwrap_err();

            assert!(
                error.to_string().contains("does not accept a value")
                    || error.to_string().contains("Unsupported runArgs option")
            );
        }
    }

    #[test]
    fn converts_forward_ports_to_forwarding_config_layer() {
        let metadata = parse_metadata(json!({
            "image": "ubuntu:24.04",
            "forwardPorts": [3000, "localhost:5432"],
            "portsAttributes": {
                "3000": {
                    "label": "web",
                    "requireLocalPort": true
                },
                "5432": {
                    "label": "db"
                }
            }
        }))
        .unwrap();

        let config = resolve_config(ConfigMergeInput {
            devcontainer: Some(metadata.to_config_layer().unwrap()),
            ..ConfigMergeInput::default()
        });

        assert_eq!(
            config.ports.entries,
            vec![
                LayerPort {
                    enabled: true,
                    container: 3000,
                    host: None,
                    host_ip: DEFAULT_PORT_HOST_IP.to_owned(),
                    protocol: PortProtocol::Tcp,
                    require_local: true,
                    label: Some("web".to_owned()),
                },
                LayerPort {
                    enabled: true,
                    container: 5432,
                    host: None,
                    host_ip: DEFAULT_PORT_HOST_IP.to_owned(),
                    protocol: PortProtocol::Tcp,
                    require_local: false,
                    label: Some("db".to_owned()),
                },
            ]
        );
    }

    #[test]
    fn applies_later_port_attributes_to_earlier_forward_ports() {
        let image_metadata = parse_metadata(json!({
            "image": "ubuntu:24.04",
            "forwardPorts": [3000]
        }))
        .unwrap()
        .to_config_layer()
        .unwrap();
        let devcontainer = parse_metadata(json!({
            "image": "ubuntu:24.04",
            "portsAttributes": {
                "3000": {
                    "label": "web",
                    "requireLocalPort": true
                }
            }
        }))
        .unwrap()
        .to_config_layer()
        .unwrap();

        let config = resolve_config(ConfigMergeInput {
            image_metadata: vec![image_metadata],
            devcontainer: Some(devcontainer),
            ..ConfigMergeInput::default()
        });

        assert_eq!(
            config.ports.entries,
            vec![LayerPort {
                enabled: true,
                container: 3000,
                host: None,
                host_ip: DEFAULT_PORT_HOST_IP.to_owned(),
                protocol: PortProtocol::Tcp,
                require_local: true,
                label: Some("web".to_owned()),
            }]
        );
    }

    #[test]
    fn converts_app_port_to_publish_config_separately_from_forwarding() {
        let metadata = parse_metadata(json!({
            "image": "ubuntu:24.04",
            "forwardPorts": [3000],
            "appPort": [8080, "0.0.0.0:8443:443"]
        }))
        .unwrap();

        let config = resolve_config(ConfigMergeInput {
            devcontainer: Some(metadata.to_config_layer().unwrap()),
            ..ConfigMergeInput::default()
        });

        assert_eq!(config.ports.entries.len(), 1);
        assert_eq!(
            config.devcontainer.publish_ports,
            vec![
                ResolvedPublishPort {
                    container: 8080,
                    host: Some(8080),
                    host_ip: None,
                    protocol: PortProtocol::Tcp,
                },
                ResolvedPublishPort {
                    container: 443,
                    host: Some(8443),
                    host_ip: Some("0.0.0.0".to_owned()),
                    protocol: PortProtocol::Tcp,
                },
            ]
        );
    }

    #[test]
    fn converts_features_and_metadata_fields_to_config_layer() {
        let metadata = parse_metadata(json!({
            "image": "ubuntu:24.04",
            "features": {
                "ghcr.io/devcontainers/features/github-cli:1": {
                    "version": "latest"
                }
            },
            "containerEnv": {
                "RUST_LOG": "debug"
            },
            "remoteEnv": {
                "PATH": "/tools:${containerEnv:PATH}"
            },
            "workspaceMount": "source=${localWorkspaceFolder},target=/workspace,type=bind",
            "workspaceFolder": "/workspace",
            "remoteUser": "vscode",
            "containerUser": "root",
            "updateRemoteUserUID": false,
            "overrideCommand": false,
            "userEnvProbe": "none",
            "init": true,
            "privileged": true,
            "capAdd": ["SYS_PTRACE"],
            "securityOpt": ["seccomp=unconfined"],
            "postCreateCommand": "echo ready",
            "waitFor": "postCreateCommand"
        }))
        .unwrap();

        let layer = metadata.to_config_layer().unwrap();
        assert_eq!(
            layer.features,
            vec![LayerFeature {
                id: "ghcr.io/devcontainers/features/github-cli:1".to_owned(),
                canonical_id: "ghcr.io/devcontainers/features/github-cli".to_owned(),
                enabled: true,
                options: [("version".to_owned(), TomlValue::String("latest".to_owned()))].into(),
            }]
        );

        let config = resolve_config(ConfigMergeInput {
            devcontainer: Some(layer),
            ..ConfigMergeInput::default()
        });

        assert_eq!(
            config
                .devcontainer
                .container_env
                .get("RUST_LOG")
                .map(String::as_str),
            Some("debug")
        );
        assert_eq!(
            config
                .devcontainer
                .remote_env
                .get("PATH")
                .map(String::as_str),
            Some("/tools:${containerEnv:PATH}")
        );
        assert_eq!(
            config.devcontainer.workspace_mount.as_deref(),
            Some("source=${localWorkspaceFolder},target=/workspace,type=bind")
        );
        assert_eq!(
            config.devcontainer.workspace_folder.as_deref(),
            Some("/workspace")
        );
        assert_eq!(config.devcontainer.remote_user.as_deref(), Some("vscode"));
        assert_eq!(config.devcontainer.container_user.as_deref(), Some("root"));
        assert!(!config.devcontainer.update_remote_user_uid);
        assert!(!config.devcontainer.override_command);
        assert!(config.devcontainer.init);
        assert!(config.devcontainer.privileged);
        assert_eq!(config.devcontainer.cap_add, vec!["SYS_PTRACE"]);
        assert_eq!(config.devcontainer.security_opt, vec!["seccomp=unconfined"]);
        assert!(config.devcontainer.lifecycle.is_some());
    }

    #[test]
    fn devcontainer_without_lifecycle_preserves_image_metadata_lifecycle() {
        let image_metadata = parse_metadata(json!({
            "image": "ubuntu:24.04",
            "postCreateCommand": "image setup"
        }))
        .unwrap()
        .to_config_layer()
        .unwrap();
        let devcontainer = parse_metadata(json!({
            "image": "ubuntu:24.04"
        }))
        .unwrap()
        .to_config_layer()
        .unwrap();

        let config = resolve_config(ConfigMergeInput {
            image_metadata: vec![image_metadata],
            devcontainer: Some(devcontainer),
            ..ConfigMergeInput::default()
        });

        let lifecycle = config.devcontainer.lifecycle.as_ref().unwrap();
        assert_eq!(
            lifecycle.command(LifecycleStage::PostCreate),
            Some(&LifecycleCommand::Shell("image setup".to_owned()))
        );
    }

    #[test]
    fn lifecycle_metadata_merges_commands_by_stage_and_wait_for_when_explicit() {
        let image_metadata = parse_metadata(json!({
            "image": "ubuntu:24.04",
            "postCreateCommand": "image setup",
            "waitFor": "postCreateCommand"
        }))
        .unwrap()
        .to_config_layer()
        .unwrap();
        let devcontainer = parse_metadata(json!({
            "image": "ubuntu:24.04",
            "postStartCommand": "project start"
        }))
        .unwrap()
        .to_config_layer()
        .unwrap();

        let config = resolve_config(ConfigMergeInput {
            image_metadata: vec![image_metadata],
            devcontainer: Some(devcontainer),
            ..ConfigMergeInput::default()
        });

        let lifecycle = config.devcontainer.lifecycle.as_ref().unwrap();
        assert_eq!(
            lifecycle.command(LifecycleStage::PostCreate),
            Some(&LifecycleCommand::Shell("image setup".to_owned()))
        );
        assert_eq!(
            lifecycle.command(LifecycleStage::PostStart),
            Some(&LifecycleCommand::Shell("project start".to_owned()))
        );
        assert_eq!(lifecycle.wait_for(), WaitFor::PostCreate);
    }

    #[test]
    fn preserves_port_attributes_for_automatic_forwarding() {
        let metadata = parse_metadata(json!({
            "image": "ubuntu:24.04",
            "portsAttributes": {
                "3000": {
                    "label": "web",
                    "onAutoForward": "silent",
                    "requireLocalPort": true
                }
            },
            "otherPortsAttributes": {
                "onAutoForward": "ignore"
            }
        }))
        .unwrap();

        let config = resolve_config(ConfigMergeInput {
            devcontainer: Some(metadata.to_config_layer().unwrap()),
            ..ConfigMergeInput::default()
        });

        let attributes = config.devcontainer.port_attributes.get("3000").unwrap();
        assert_eq!(attributes.label.as_deref(), Some("web"));
        assert_eq!(
            attributes.on_auto_forward,
            Some(ConfigOnAutoForward::Silent)
        );
        assert_eq!(attributes.require_local_port, Some(true));
        assert_eq!(
            config
                .devcontainer
                .other_ports_attributes
                .as_ref()
                .and_then(|attributes| attributes.on_auto_forward),
            Some(ConfigOnAutoForward::Ignore)
        );
    }
}
