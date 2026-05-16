#![allow(dead_code)]

use std::collections::BTreeMap;

use anyhow::{Result, anyhow};
use serde::Deserialize;
use serde_json::Value;

pub(crate) fn parse_metadata(value: Value) -> Result<DevcontainerMetadata> {
    let raw: RawDevcontainerMetadata = serde_json::from_value(value)
        .map_err(|error| anyhow!("Failed to parse devcontainer metadata schema: {error}"))?;

    raw.validate()
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DevcontainerMetadata {
    source: DevcontainerSource,
    features: BTreeMap<String, Value>,
    override_feature_install_order: Vec<String>,
    mounts: Vec<String>,
    workspace_mount: Option<String>,
    workspace_folder: Option<String>,
    container_env: BTreeMap<String, String>,
    remote_env: BTreeMap<String, String>,
    remote_user: Option<String>,
    container_user: Option<String>,
    update_remote_user_uid: Option<bool>,
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
    pub(crate) fn source(&self) -> &DevcontainerSource {
        &self.source
    }

    pub(crate) fn mounts(&self) -> &[String] {
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

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum OnAutoForward {
    Notify,
    Silent,
    Ignore,
    OpenBrowser,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DevcontainerPortAttributes {
    pub(crate) label: Option<String>,
    pub(crate) on_auto_forward: Option<OnAutoForward>,
    pub(crate) require_local_port: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub(crate) enum DevcontainerPort {
    Number(u16),
    String(String),
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
    mounts: Vec<String>,
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
    fn validate(self) -> Result<DevcontainerMetadata> {
        if self.docker_compose_file.is_some() || self.service.is_some() {
            return Err(anyhow!(
                "Docker Compose mode is not supported in decune v0.1"
            ));
        }

        let lifecycle = self.lifecycle_values();
        let source = match (self.image, self.build) {
            (Some(_), Some(_)) => {
                return Err(anyhow!(
                    "Devcontainer metadata must not specify both image and build"
                ));
            }
            (Some(image), None) => DevcontainerSource::Image(image),
            (None, Some(build)) => DevcontainerSource::Dockerfile(build.validate()?),
            (None, None) => {
                return Err(anyhow!(
                    "Devcontainer metadata must specify either image or build"
                ));
            }
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
                let value = values
                    .get(index + 1)
                    .ok_or_else(|| anyhow!("Missing value for runArgs option {current}"))?;
                args.push(run_arg_with_value(current, value.clone())?);
                index += 1;
            }
            _ => return Err(anyhow!("Unsupported runArgs option: {current}")),
        }

        index += 1;
    }

    Ok(args)
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
            &DevcontainerSource::Image(
                "mcr.microsoft.com/devcontainers/rust:1-1-bookworm".to_owned()
            )
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
            &DevcontainerSource::Dockerfile(DevcontainerBuild {
                dockerfile: "Dockerfile".to_owned(),
                context: Some("..".to_owned()),
                args: [("VARIANT".to_owned(), "bookworm".to_owned())].into(),
                target: Some("dev".to_owned()),
                cache_from: vec!["type=registry,ref=example.test/cache".to_owned()],
            })
        );
        assert_eq!(
            metadata.workspace_mount(),
            Some("source=${localWorkspaceFolder},target=/workspace,type=bind")
        );
        assert_eq!(
            metadata.mounts(),
            &["source=decune-cache,target=/cache,type=volume".to_owned()]
        );
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
}
