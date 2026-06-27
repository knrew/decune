use std::collections::BTreeMap;

use anyhow::{Result, anyhow};
use serde::Deserialize;
use serde_json::Value;

use super::{
    DevcontainerBuild, DevcontainerCompose, DevcontainerMetadata, DevcontainerShutdownAction,
    DevcontainerSource, LifecycleProperty, UserEnvProbe,
    args::{normalize_run_args, validate_build_options},
    serde_helpers::{deserialize_build_args, deserialize_ports, deserialize_string_or_strings},
};
use crate::{
    devcontainer::mounts::DevcontainerMount,
    devcontainer::ports::{DevcontainerPort, DevcontainerPortAttributes},
};

pub(super) fn parse_metadata_value(
    value: Value,
    source_requirement: SourceRequirement,
    layer_kind: MetadataLayerKind,
) -> Result<DevcontainerMetadata> {
    let raw: RawDevcontainerMetadata = serde_json::from_value(value)
        .map_err(|error| anyhow!("Failed to parse devcontainer metadata schema: {error}"))?;

    raw.validate(source_requirement, layer_kind)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDevcontainerMetadata {
    image: Option<String>,
    build: Option<RawDevcontainerBuild>,
    docker_compose_file: Option<Value>,
    service: Option<String>,
    run_services: Option<Vec<String>>,
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
    entrypoint: Option<String>,
    shutdown_action: Option<DevcontainerShutdownAction>,
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
        mut self,
        source_requirement: SourceRequirement,
        layer_kind: MetadataLayerKind,
    ) -> Result<DevcontainerMetadata> {
        if layer_kind == MetadataLayerKind::ImageLabel && self.initialize_command.is_some() {
            return Err(anyhow!(
                "Image devcontainer metadata must not specify initializeCommand"
            ));
        }

        let lifecycle = self.lifecycle_values();
        let docker_compose_file = parse_docker_compose_file(self.docker_compose_file.take())?;
        validate_compose_only_properties(&self, docker_compose_file.is_some())?;
        let is_compose_mode =
            docker_compose_file.is_some() || self.service.is_some() || self.run_services.is_some();
        let (source, run_args) = if is_compose_mode {
            validate_compose_unsupported_properties(&self)?;
            if self.image.is_some() {
                return Err(anyhow!(
                    "Devcontainer metadata must not specify image with dockerComposeFile"
                ));
            }
            if self.build.is_some() {
                return Err(anyhow!(
                    "Devcontainer metadata must not specify build with dockerComposeFile"
                ));
            }
            let files = docker_compose_file.ok_or_else(|| {
                anyhow!("Docker Compose devcontainer metadata must specify dockerComposeFile")
            })?;
            let service = self.service.take().ok_or_else(|| {
                anyhow!("Docker Compose devcontainer metadata must specify service")
            })?;
            if self.workspace_folder.is_none() {
                self.workspace_folder = Some("/".to_owned());
            }
            if self.override_command.is_none() {
                self.override_command = Some(false);
            }
            if self.shutdown_action.is_none() {
                self.shutdown_action = Some(DevcontainerShutdownAction::StopCompose);
            }

            (
                Some(DevcontainerSource::Compose(DevcontainerCompose {
                    files,
                    service,
                    run_services: self.run_services.take(),
                })),
                Vec::new(),
            )
        } else {
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
                        "Devcontainer metadata must specify either image, build, or dockerComposeFile with service"
                    ));
                }
                (None, None) => None,
            };
            (source, normalize_run_args(&self.run_args)?)
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
            run_args,
            init: self.init,
            privileged: self.privileged,
            cap_add: self.cap_add,
            security_opt: self.security_opt,
            entrypoints: self.entrypoint.into_iter().collect(),
            shutdown_action: self.shutdown_action,
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

fn validate_compose_only_properties(
    raw: &RawDevcontainerMetadata,
    has_docker_compose_file: bool,
) -> Result<()> {
    if raw.run_services.is_none() {
        return Ok(());
    }

    let mut missing = Vec::new();
    if !has_docker_compose_file {
        missing.push("dockerComposeFile");
    }
    if raw.service.is_none() {
        missing.push("service");
    }

    if !missing.is_empty() {
        return Err(anyhow!(
            "runServices is only supported in Docker Compose mode and requires {}",
            human_join(&missing)
        ));
    }

    Ok(())
}

fn validate_compose_unsupported_properties(raw: &RawDevcontainerMetadata) -> Result<()> {
    if raw.workspace_mount.is_some() {
        return Err(anyhow!(
            "workspaceMount is not supported in Docker Compose mode; define workspace volumes in the Compose file"
        ));
    }
    if !raw.app_port.is_empty() {
        return Err(anyhow!(
            "appPort is not supported in Docker Compose mode; define published ports in the Compose file"
        ));
    }
    if !raw.run_args.is_empty() {
        return Err(anyhow!(
            "runArgs is not supported in Docker Compose mode; define service options in the Compose file"
        ));
    }

    Ok(())
}

fn parse_docker_compose_file(value: Option<Value>) -> Result<Option<Vec<String>>> {
    let Some(value) = value else {
        return Ok(None);
    };

    let files = match value {
        Value::String(value) => vec![value],
        Value::Array(values) => values
            .into_iter()
            .map(|value| match value {
                Value::String(value) => Ok(value),
                _ => Err(anyhow!("dockerComposeFile entries must be strings")),
            })
            .collect::<Result<Vec<_>>>()?,
        _ => {
            return Err(anyhow!(
                "dockerComposeFile must be a string or string array"
            ));
        }
    };

    if files.is_empty() {
        return Err(anyhow!("dockerComposeFile must not be empty"));
    }

    Ok(Some(files))
}

fn human_join(values: &[&str]) -> String {
    match values {
        [] => String::new(),
        [value] => (*value).to_owned(),
        [left, right] => format!("{left} and {right}"),
        [head @ .., tail] => format!("{}, and {tail}", head.join(", ")),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SourceRequirement {
    Required,
    Optional,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MetadataLayerKind {
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
    #[serde(default)]
    options: Vec<String>,
    target: Option<String>,
    #[serde(default, deserialize_with = "deserialize_string_or_strings")]
    cache_from: Vec<String>,
}

impl RawDevcontainerBuild {
    fn validate(self) -> Result<DevcontainerBuild> {
        validate_build_options(&self.options)?;
        Ok(DevcontainerBuild {
            dockerfile: self.dockerfile,
            context: self.context,
            args: self.args,
            options: self.options,
            target: self.target,
            cache_from: self.cache_from,
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::{
        DevcontainerCompose, DevcontainerSource, LifecycleProperty, UserEnvProbe,
        parse_image_metadata_layer, parse_metadata, parse_metadata_layer,
    };
    use crate::{
        config::{layer::ConfigMergeInput, merge::resolve_config},
        devcontainer::{mounts::DevcontainerMount, ports::OnAutoForward},
    };

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
    fn image_metadata_layer_rejects_initialize_command() {
        let error = parse_image_metadata_layer(json!({
            "initializeCommand": "scripts/init.sh"
        }))
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Image devcontainer metadata must not specify initializeCommand")
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
                crate::devcontainer::ports::DevcontainerPort::Number(3000),
                crate::devcontainer::ports::DevcontainerPort::String("localhost:5432".to_owned())
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
                crate::devcontainer::ports::DevcontainerPort::Number(8080),
                crate::devcontainer::ports::DevcontainerPort::String(
                    "127.0.0.1:8443:443".to_owned()
                )
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
    fn parses_false_security_booleans_as_explicit_metadata_values() {
        let metadata = parse_metadata(json!({
            "image": "ubuntu:24.04",
            "init": false,
            "privileged": false
        }))
        .unwrap();

        assert_eq!(metadata.init(), Some(false));
        assert_eq!(metadata.privileged(), Some(false));
    }

    #[test]
    fn parses_compose_metadata_from_string_file() {
        let metadata = parse_metadata(json!({
            "dockerComposeFile": "compose.yml",
            "service": "app"
        }))
        .unwrap();

        assert_eq!(
            metadata.source(),
            Some(&DevcontainerSource::Compose(DevcontainerCompose {
                files: vec!["compose.yml".to_owned()],
                service: "app".to_owned(),
                run_services: None,
            }))
        );
    }

    #[test]
    fn parses_compose_metadata_from_file_array_preserving_order() {
        let metadata = parse_metadata(json!({
            "dockerComposeFile": ["compose.yml", "compose.override.yml"],
            "service": "app"
        }))
        .unwrap();

        assert_eq!(
            metadata.source(),
            Some(&DevcontainerSource::Compose(DevcontainerCompose {
                files: vec!["compose.yml".to_owned(), "compose.override.yml".to_owned()],
                service: "app".to_owned(),
                run_services: None,
            }))
        );
    }

    #[test]
    fn compose_run_services_distinguishes_missing_empty_and_values() {
        for (run_services, expected) in [
            (None, None),
            (Some(json!([])), Some(Vec::new())),
            (
                Some(json!(["app", "db"])),
                Some(vec!["app".to_owned(), "db".to_owned()]),
            ),
        ] {
            let mut value = json!({
                "dockerComposeFile": "compose.yml",
                "service": "app"
            });
            if let Some(run_services) = run_services {
                value["runServices"] = run_services;
            }
            let metadata = parse_metadata(value).unwrap();
            let Some(DevcontainerSource::Compose(compose)) = metadata.source() else {
                panic!("expected Compose source");
            };

            assert_eq!(compose.run_services, expected);
        }
    }

    #[test]
    fn run_services_requires_compose_metadata() {
        for (value, expected) in [
            (
                json!({
                    "image": "ubuntu:24.04",
                    "runServices": ["app"]
                }),
                "runServices is only supported in Docker Compose mode and requires dockerComposeFile and service",
            ),
            (
                json!({
                    "build": {"dockerfile": "Dockerfile"},
                    "runServices": ["app"]
                }),
                "runServices is only supported in Docker Compose mode and requires dockerComposeFile and service",
            ),
            (
                json!({
                    "runServices": ["app"]
                }),
                "runServices is only supported in Docker Compose mode and requires dockerComposeFile and service",
            ),
            (
                json!({
                    "dockerComposeFile": "compose.yml",
                    "runServices": ["app"]
                }),
                "runServices is only supported in Docker Compose mode and requires service",
            ),
            (
                json!({
                    "service": "app",
                    "runServices": ["app"]
                }),
                "runServices is only supported in Docker Compose mode and requires dockerComposeFile",
            ),
        ] {
            let error = parse_metadata(value).unwrap_err();

            assert!(
                error.to_string().contains(expected),
                "expected {expected:?} in {error}"
            );
        }
    }

    #[test]
    fn invalid_compose_mode_metadata_is_rejected() {
        for (value, expected) in [
            (
                json!({"dockerComposeFile": "compose.yml"}),
                "must specify service",
            ),
            (json!({"service": "app"}), "must specify dockerComposeFile"),
            (
                json!({
                    "image": "ubuntu:24.04",
                    "dockerComposeFile": "compose.yml",
                    "service": "app"
                }),
                "must not specify image with dockerComposeFile",
            ),
            (
                json!({
                    "build": {"dockerfile": "Dockerfile"},
                    "dockerComposeFile": "compose.yml",
                    "service": "app"
                }),
                "must not specify build with dockerComposeFile",
            ),
            (
                json!({
                    "dockerComposeFile": "compose.yml",
                    "service": "app",
                    "workspaceMount": "source=${localWorkspaceFolder},target=/workspace,type=bind"
                }),
                "workspaceMount is not supported in Docker Compose mode",
            ),
            (
                json!({
                    "dockerComposeFile": "compose.yml",
                    "service": "app",
                    "appPort": [8080]
                }),
                "appPort is not supported in Docker Compose mode",
            ),
            (
                json!({
                    "dockerComposeFile": "compose.yml",
                    "service": "app",
                    "runArgs": ["--init"]
                }),
                "runArgs is not supported in Docker Compose mode",
            ),
        ] {
            let error = parse_metadata(value).unwrap_err();

            assert!(
                error.to_string().contains(expected),
                "expected {expected:?} in {error}"
            );
        }
    }

    #[test]
    fn metadata_without_image_or_build_is_rejected() {
        let error = parse_metadata(json!({
            "features": {}
        }))
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("either image, build, or dockerComposeFile with service")
        );
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
}
