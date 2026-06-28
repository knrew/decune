use std::collections::BTreeMap;

use anyhow::{Result, anyhow};
use serde_json::Value;

use super::{
    DevcontainerMetadata, DevcontainerRunArg, DevcontainerShutdownAction, DevcontainerSource,
    UserEnvProbe,
};
use crate::{
    config::layer::{
        ConfigLayer, LayerDevcontainerBuild, LayerDevcontainerCompose, LayerDevcontainerMetadata,
        LayerDevcontainerSource, LayerFeature, LayerRunArg, LayerShutdownAction, LayerUserEnvProbe,
    },
    devcontainer::lifecycle::parse_lifecycle_layer_definition,
    devcontainer::mounts::DevcontainerMount,
    devcontainer::ports::{
        forwarding_port_to_layer, port_attributes_to_layer, publish_port_to_layer,
    },
};

impl DevcontainerMetadata {
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
                DevcontainerRunArg::Passthrough { option, value } => {
                    if let Some(devcontainer) = &mut layer.devcontainer {
                        devcontainer.run_args.push(LayerRunArg::Passthrough {
                            option: option.clone(),
                            value: value.clone(),
                        });
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
            entrypoints: self.entrypoints.clone(),
            shutdown_action: self.shutdown_action.as_ref().map(shutdown_action_to_layer),
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
                options: build.options.clone(),
                target: build.target.clone(),
                cache_from: build.cache_from.clone(),
            })
        }
        DevcontainerSource::Compose(compose) => {
            LayerDevcontainerSource::Compose(LayerDevcontainerCompose {
                files: compose.files.clone(),
                service: compose.service.clone(),
                run_services: compose.run_services.clone(),
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
        Value::String(version) => {
            feature
                .options
                .insert("version".to_owned(), toml::Value::String(version.clone()));
        }
        Value::Bool(enabled) => {
            feature.enabled = *enabled;
        }
        Value::Null => {}
        Value::Number(_) | Value::Array(_) => {
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
        Value::Number(value) => value.as_i64().map_or_else(
            || {
                value.as_f64().map_or_else(
                    || Err(anyhow!("JSON number cannot be represented as TOML")),
                    |value| Ok(toml::Value::Float(value)),
                )
            },
            |value| Ok(toml::Value::Integer(value)),
        ),
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

const fn user_env_probe_to_layer(value: &UserEnvProbe) -> LayerUserEnvProbe {
    match value {
        UserEnvProbe::None => LayerUserEnvProbe::None,
        UserEnvProbe::LoginShell => LayerUserEnvProbe::LoginShell,
        UserEnvProbe::InteractiveShell => LayerUserEnvProbe::InteractiveShell,
        UserEnvProbe::LoginInteractiveShell => LayerUserEnvProbe::LoginInteractiveShell,
    }
}

const fn shutdown_action_to_layer(value: &DevcontainerShutdownAction) -> LayerShutdownAction {
    match value {
        DevcontainerShutdownAction::None => LayerShutdownAction::None,
        DevcontainerShutdownAction::StopContainer => LayerShutdownAction::StopContainer,
        DevcontainerShutdownAction::StopCompose => LayerShutdownAction::StopCompose,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use toml::Value as TomlValue;

    use super::super::parse_metadata;
    use crate::config::{
        layer::{ConfigMergeInput, LayerFeature, LayerPort, LayerShutdownAction},
        merge::resolve_config,
        resolved::ResolvedPublishPort,
        types::{DEFAULT_PORT_HOST_IP, OnAutoForward as ConfigOnAutoForward, PortProtocol},
    };
    use crate::devcontainer::lifecycle::{LifecycleCommand, LifecycleStage, WaitFor};

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
    fn compose_mode_applies_defaults_to_config_layer() {
        let metadata = parse_metadata(json!({
            "dockerComposeFile": "compose.yml",
            "service": "app"
        }))
        .unwrap();

        let config = resolve_config(ConfigMergeInput {
            devcontainer: Some(metadata.to_config_layer().unwrap()),
            ..ConfigMergeInput::default()
        });

        assert_eq!(config.devcontainer.workspace_folder.as_deref(), Some("/"));
        assert!(!config.devcontainer.override_command);
        assert_eq!(
            config.devcontainer.shutdown_action,
            LayerShutdownAction::StopCompose
        );
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
                    service: None,
                    container: 3000,
                    host: None,
                    host_ip: DEFAULT_PORT_HOST_IP.to_owned(),
                    protocol: PortProtocol::Tcp,
                    require_local: true,
                    label: Some("web".to_owned()),
                },
                LayerPort {
                    enabled: true,
                    service: None,
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
    fn converts_compose_forward_ports_to_service_aware_layer_ports() {
        let metadata = parse_metadata(json!({
            "dockerComposeFile": "compose.yml",
            "service": "app",
            "forwardPorts": [3000, "3001", "db:5432"],
            "portsAttributes": {
                "5432": {
                    "label": "generic"
                },
                "db:5432": {
                    "label": "db",
                    "requireLocalPort": true
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
                    service: None,
                    container: 3000,
                    host: None,
                    host_ip: DEFAULT_PORT_HOST_IP.to_owned(),
                    protocol: PortProtocol::Tcp,
                    require_local: false,
                    label: None,
                },
                LayerPort {
                    enabled: true,
                    service: None,
                    container: 3001,
                    host: None,
                    host_ip: DEFAULT_PORT_HOST_IP.to_owned(),
                    protocol: PortProtocol::Tcp,
                    require_local: false,
                    label: None,
                },
                LayerPort {
                    enabled: true,
                    service: Some("db".to_owned()),
                    container: 5432,
                    host: None,
                    host_ip: DEFAULT_PORT_HOST_IP.to_owned(),
                    protocol: PortProtocol::Tcp,
                    require_local: true,
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
                service: None,
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
            config.ports.entries[0],
            LayerPort {
                enabled: true,
                service: None,
                container: 3000,
                host: None,
                host_ip: DEFAULT_PORT_HOST_IP.to_owned(),
                protocol: PortProtocol::Tcp,
                require_local: false,
                label: None,
            }
        );
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
            "entrypoint": "/usr/local/share/feature/start.sh",
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
        assert_eq!(config.devcontainer.init, Some(true));
        assert_eq!(config.devcontainer.privileged, Some(true));
        assert_eq!(config.devcontainer.cap_add, vec!["SYS_PTRACE"]);
        assert_eq!(config.devcontainer.security_opt, vec!["seccomp=unconfined"]);
        assert_eq!(
            config.devcontainer.entrypoints,
            vec!["/usr/local/share/feature/start.sh"]
        );
        assert!(config.devcontainer.lifecycle.is_some());
    }

    #[test]
    fn converts_feature_string_value_to_version_option() {
        let metadata = parse_metadata(json!({
            "image": "ubuntu:24.04",
            "features": {
                "ghcr.io/devcontainers/features/go": "1.18"
            }
        }))
        .unwrap();

        let layer = metadata.to_config_layer().unwrap();

        assert_eq!(
            layer.features[0].options.get("version"),
            Some(&TomlValue::String("1.18".to_owned()))
        );
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
