use std::collections::BTreeMap;

use serde_json::Value as JsonValue;
use toml::Value;

use crate::devcontainer::lifecycle::LayerLifecycleDefinition;

use crate::config::{
    path::ConfigPathOrigin,
    schema::{
        RawAutoPortsConfig, RawComposeCloneIsolationConfig, RawComposeCloneIsolationEndpointConfig,
        RawComposeCloneIsolationNamesConfig, RawComposeCloneIsolationNetworksConfig,
        RawComposeConfig, RawComposePublishedPortMappingConfig, RawComposePublishedPortsConfig,
        RawCredentialsConfig, RawDecuneConfig, RawDotfileConfig, RawFeatureConfig,
        RawGitCredentialsConfig, RawGithubCredentialsConfig, RawHookConfig, RawHooksConfig,
        RawMountConfig, RawPortConfig, RawPortProtocol,
    },
    types::{
        Command, DEFAULT_PORT_HOST_IP, DotfileConflict, GitHttpsMode, GithubCredentialsMode,
        HookLocation, MountCreate, MountType, OnAutoForward, PortProtocol, SshAgentMode,
    },
};

#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct ConfigMergeInput {
    pub(crate) image_metadata: Vec<ConfigLayer>,
    pub(crate) feature_metadata: Vec<ConfigLayer>,
    pub(crate) global: Option<ConfigLayer>,
    pub(crate) devcontainer: Option<ConfigLayer>,
    pub(crate) project: Option<ConfigLayer>,
    pub(crate) cli: Option<ConfigLayer>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct ConfigLayer {
    pub(crate) shell: Option<String>,
    pub(crate) features: Vec<LayerFeature>,
    pub(crate) dotfiles: Vec<LayerDotfile>,
    pub(crate) mounts: Vec<LayerMount>,
    pub(crate) ports: Vec<LayerPort>,
    pub(crate) forward_ports: Vec<LayerForwardPort>,
    pub(crate) auto_ports: Option<LayerAutoPorts>,
    pub(crate) compose: LayerCompose,
    pub(crate) devcontainer: Option<LayerDevcontainerMetadata>,
    pub(crate) credentials: LayerCredentials,
    pub(crate) hooks: LayerHooks,
}

impl ConfigLayer {
    #[cfg(test)]
    pub(crate) fn from_raw_decune(raw: RawDecuneConfig) -> Self {
        Self::from_raw_decune_with_origin(raw, ConfigPathOrigin::Project)
    }

    pub(crate) fn from_raw_decune_with_origin(
        raw: RawDecuneConfig,
        origin: ConfigPathOrigin,
    ) -> Self {
        Self {
            shell: raw.shell,
            features: raw
                .features
                .into_iter()
                .map(|(id, feature)| LayerFeature::from_raw(id, feature))
                .collect(),
            dotfiles: raw
                .dotfiles
                .into_iter()
                .map(|dotfile| LayerDotfile::from_raw(dotfile, origin))
                .collect(),
            mounts: raw
                .mounts
                .into_iter()
                .map(|mount| LayerMount::from_raw(mount, origin))
                .collect(),
            ports: raw
                .ports
                .entries
                .into_iter()
                .map(LayerPort::from_raw)
                .collect(),
            forward_ports: Vec::new(),
            auto_ports: raw.ports.auto.map(LayerAutoPorts::from_raw),
            compose: LayerCompose::from_raw(&raw.compose),
            devcontainer: None,
            credentials: LayerCredentials::from_raw(raw.credentials),
            hooks: LayerHooks::from_raw(raw.hooks),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct LayerCompose {
    pub(crate) published_ports: LayerComposePublishedPorts,
    pub(crate) clone_isolation: Box<LayerComposeCloneIsolation>,
}

impl LayerCompose {
    fn from_raw(raw: &RawComposeConfig) -> Self {
        Self {
            published_ports: raw
                .published_ports
                .as_ref()
                .map(LayerComposePublishedPorts::from_raw)
                .unwrap_or_default(),
            clone_isolation: Box::new(
                raw.clone_isolation
                    .as_ref()
                    .map(LayerComposeCloneIsolation::from_raw)
                    .unwrap_or_default(),
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct LayerComposeCloneIsolation {
    pub(crate) enabled: Option<bool>,
    pub(crate) networks: LayerComposeCloneIsolationNetworks,
    pub(crate) names: LayerComposeCloneIsolationNames,
    pub(crate) endpoints: Vec<LayerComposeCloneIsolationEndpoint>,
}

impl LayerComposeCloneIsolation {
    fn from_raw(raw: &RawComposeCloneIsolationConfig) -> Self {
        Self {
            enabled: raw.enabled,
            networks: raw
                .networks
                .as_ref()
                .map(LayerComposeCloneIsolationNetworks::from_raw)
                .unwrap_or_default(),
            names: raw
                .names
                .as_ref()
                .map(LayerComposeCloneIsolationNames::from_raw)
                .unwrap_or_default(),
            endpoints: raw
                .endpoints
                .iter()
                .map(LayerComposeCloneIsolationEndpoint::from_raw)
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct LayerComposeCloneIsolationNetworks {
    pub(crate) relocation: Option<bool>,
    pub(crate) subnet_pool: Option<String>,
    pub(crate) subnet_prefix: Option<u8>,
}

impl LayerComposeCloneIsolationNetworks {
    fn from_raw(raw: &RawComposeCloneIsolationNetworksConfig) -> Self {
        Self {
            relocation: raw.relocation,
            subnet_pool: raw.subnet_pool.clone(),
            subnet_prefix: raw.subnet_prefix,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct LayerComposeCloneIsolationNames {
    pub(crate) rewrite_container_names: Option<bool>,
    pub(crate) rewrite_resource_names: Option<bool>,
}

impl LayerComposeCloneIsolationNames {
    const fn from_raw(raw: &RawComposeCloneIsolationNamesConfig) -> Self {
        Self {
            rewrite_container_names: raw.rewrite_container_names,
            rewrite_resource_names: raw.rewrite_resource_names,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LayerComposeCloneIsolationEndpoint {
    pub(crate) service: String,
    pub(crate) env: String,
    pub(crate) value: String,
}

impl LayerComposeCloneIsolationEndpoint {
    fn from_raw(raw: &RawComposeCloneIsolationEndpointConfig) -> Self {
        Self {
            service: raw.service.clone(),
            env: raw.env.clone(),
            value: raw.value.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct LayerComposePublishedPorts {
    pub(crate) automatic_relocation: Option<bool>,
    pub(crate) warn_on_relocation: Option<bool>,
    pub(crate) mappings: Vec<LayerComposePublishedPortMapping>,
}

impl LayerComposePublishedPorts {
    fn from_raw(raw: &RawComposePublishedPortsConfig) -> Self {
        Self {
            automatic_relocation: raw.automatic_relocation,
            warn_on_relocation: raw.warn_on_relocation,
            mappings: raw
                .mappings
                .iter()
                .map(LayerComposePublishedPortMapping::from_raw)
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LayerComposePublishedPortMapping {
    pub(crate) enabled: bool,
    pub(crate) service: String,
    pub(crate) target: u16,
    pub(crate) protocol: PortProtocol,
    pub(crate) host: Option<u16>,
    pub(crate) host_ip: Option<String>,
}

impl LayerComposePublishedPortMapping {
    fn from_raw(raw: &RawComposePublishedPortMappingConfig) -> Self {
        Self {
            enabled: raw.enabled.unwrap_or(true),
            service: raw.service.clone(),
            target: raw.target,
            protocol: raw.protocol.unwrap_or(RawPortProtocol::Tcp).into(),
            host: raw.host,
            host_ip: raw.host_ip.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LayerFeature {
    pub(crate) id: String,
    pub(crate) canonical_id: String,
    pub(crate) enabled: bool,
    pub(crate) options: BTreeMap<String, Value>,
}

impl LayerFeature {
    pub(crate) fn new(id: impl Into<String>) -> Self {
        let id = id.into();
        let canonical_id = canonical_feature_id(&id);
        Self {
            id,
            canonical_id,
            enabled: true,
            options: BTreeMap::new(),
        }
    }

    fn from_raw(id: String, raw: RawFeatureConfig) -> Self {
        let canonical_id = canonical_feature_id(&id);
        Self {
            id,
            canonical_id,
            enabled: raw.enabled.unwrap_or(true),
            options: raw.options,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LayerDotfile {
    pub(crate) enabled: bool,
    pub(crate) source: Option<String>,
    pub(crate) target: String,
    pub(crate) read_only: bool,
    pub(crate) resolve_symlink: bool,
    pub(crate) on_conflict: DotfileConflict,
    pub(crate) origin: ConfigPathOrigin,
}

impl LayerDotfile {
    fn from_raw(raw: RawDotfileConfig, origin: ConfigPathOrigin) -> Self {
        Self {
            enabled: raw.enabled.unwrap_or(true),
            source: raw.source,
            target: raw.target,
            read_only: raw.read_only.unwrap_or(true),
            resolve_symlink: raw.resolve_symlink.unwrap_or(true),
            on_conflict: raw.on_conflict.map(Into::into).unwrap_or_default(),
            origin,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LayerMount {
    pub(crate) enabled: bool,
    pub(crate) source: Option<String>,
    pub(crate) target: String,
    pub(crate) mount_type: Option<MountType>,
    pub(crate) read_only: bool,
    pub(crate) resolve_symlink: bool,
    pub(crate) create: Option<MountCreate>,
    pub(crate) origin: ConfigPathOrigin,
}

impl LayerMount {
    fn from_raw(raw: RawMountConfig, origin: ConfigPathOrigin) -> Self {
        Self {
            enabled: raw.enabled.unwrap_or(true),
            source: raw.source,
            target: raw.target,
            mount_type: raw.mount_type.map(Into::into),
            read_only: raw.read_only.unwrap_or(false),
            resolve_symlink: raw.resolve_symlink.unwrap_or(true),
            create: raw.create.map(Into::into),
            origin,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LayerPort {
    pub(crate) enabled: bool,
    pub(crate) service: Option<String>,
    pub(crate) container: u16,
    pub(crate) host: Option<u16>,
    pub(crate) host_ip: String,
    pub(crate) protocol: PortProtocol,
    pub(crate) require_local: bool,
    pub(crate) label: Option<String>,
}

impl LayerPort {
    fn from_raw(raw: RawPortConfig) -> Self {
        Self {
            enabled: raw.enabled.unwrap_or(true),
            service: raw.service,
            container: raw.container,
            host: raw.host,
            host_ip: raw
                .host_ip
                .unwrap_or_else(|| DEFAULT_PORT_HOST_IP.to_owned()),
            protocol: raw.protocol.unwrap_or(RawPortProtocol::Tcp).into(),
            require_local: raw.require_local.unwrap_or(false),
            label: raw.label,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LayerForwardPort {
    pub(crate) port: LayerPort,
    pub(crate) attribute_keys: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct LayerAutoPorts {
    pub(crate) enabled: Option<bool>,
    pub(crate) min: Option<u16>,
    pub(crate) max: Option<u16>,
    pub(crate) ignore: Option<Vec<u16>>,
    pub(crate) on_auto_forward: Option<OnAutoForward>,
}

impl LayerAutoPorts {
    fn from_raw(raw: RawAutoPortsConfig) -> Self {
        Self {
            enabled: raw.enabled,
            min: raw.min,
            max: raw.max,
            ignore: raw.ignore,
            on_auto_forward: raw.on_auto_forward.map(Into::into),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct LayerDevcontainerMetadata {
    pub(crate) source: Option<LayerDevcontainerSource>,
    pub(crate) override_feature_install_order: Vec<String>,
    pub(crate) mounts: Vec<LayerDevcontainerMount>,
    pub(crate) workspace_mount: Option<String>,
    pub(crate) workspace_folder: Option<String>,
    pub(crate) container_env: BTreeMap<String, String>,
    pub(crate) remote_env: BTreeMap<String, String>,
    pub(crate) remote_user: Option<String>,
    pub(crate) container_user: Option<String>,
    pub(crate) update_remote_user_uid: Option<bool>,
    pub(crate) override_command: Option<bool>,
    pub(crate) user_env_probe: Option<LayerUserEnvProbe>,
    pub(crate) publish_ports: Vec<LayerPublishPort>,
    pub(crate) port_attributes: BTreeMap<String, LayerPortAttributes>,
    pub(crate) other_ports_attributes: Option<LayerPortAttributes>,
    pub(crate) run_args: Vec<LayerRunArg>,
    pub(crate) init: Option<bool>,
    pub(crate) privileged: Option<bool>,
    pub(crate) cap_add: Vec<String>,
    pub(crate) security_opt: Vec<String>,
    pub(crate) entrypoints: Vec<String>,
    pub(crate) shutdown_action: Option<LayerShutdownAction>,
    pub(crate) lifecycle: Option<LayerLifecycleDefinition>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LayerDevcontainerSource {
    Image(String),
    Dockerfile(LayerDevcontainerBuild),
    Compose(LayerDevcontainerCompose),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LayerDevcontainerBuild {
    pub(crate) dockerfile: String,
    pub(crate) context: Option<String>,
    pub(crate) args: BTreeMap<String, String>,
    pub(crate) options: Vec<String>,
    pub(crate) target: Option<String>,
    pub(crate) cache_from: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LayerDevcontainerCompose {
    pub(crate) files: Vec<String>,
    pub(crate) service: String,
    pub(crate) run_services: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LayerShutdownAction {
    None,
    StopContainer,
    StopCompose,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum LayerDevcontainerMount {
    String(String),
    Object(BTreeMap<String, JsonValue>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LayerUserEnvProbe {
    None,
    LoginShell,
    InteractiveShell,
    LoginInteractiveShell,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LayerPublishPort {
    pub(crate) container: u16,
    pub(crate) host: Option<u16>,
    pub(crate) host_ip: Option<String>,
    pub(crate) protocol: PortProtocol,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct LayerPortAttributes {
    pub(crate) label: Option<String>,
    pub(crate) on_auto_forward: Option<OnAutoForward>,
    pub(crate) require_local_port: Option<bool>,
    pub(crate) unsupported_protocol: Option<String>,
    pub(crate) unsupported_elevate_if_needed: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LayerRunArg {
    AddHost(String),
    Dns(String),
    DnsSearch(String),
    Passthrough { option: String, value: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct LayerCredentials {
    pub(crate) git: LayerGitCredentials,
    pub(crate) github: LayerGithubCredentials,
}

impl LayerCredentials {
    fn from_raw(raw: RawCredentialsConfig) -> Self {
        Self {
            git: raw
                .git
                .map(LayerGitCredentials::from_raw)
                .unwrap_or_default(),
            github: raw
                .github
                .map(LayerGithubCredentials::from_raw)
                .unwrap_or_default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct LayerGitCredentials {
    pub(crate) enabled: Option<bool>,
    pub(crate) copy_user: Option<bool>,
    pub(crate) copy_global_config: Option<bool>,
    pub(crate) https: Option<GitHttpsMode>,
    pub(crate) ssh_agent: Option<SshAgentMode>,
}

impl LayerGitCredentials {
    fn from_raw(raw: RawGitCredentialsConfig) -> Self {
        Self {
            enabled: raw.enabled,
            copy_user: raw.copy_user,
            copy_global_config: raw.copy_global_config,
            https: raw.https.map(Into::into),
            ssh_agent: raw.ssh_agent.map(Into::into),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct LayerGithubCredentials {
    pub(crate) enabled: Option<bool>,
    pub(crate) mode: Option<GithubCredentialsMode>,
    pub(crate) install_feature_if_missing: Option<bool>,
}

impl LayerGithubCredentials {
    fn from_raw(raw: RawGithubCredentialsConfig) -> Self {
        Self {
            enabled: raw.enabled,
            mode: raw.mode.map(Into::into),
            install_feature_if_missing: raw.install_feature_if_missing,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct LayerHooks {
    pub(crate) before_initialize: Vec<LayerHook>,
    pub(crate) after_initialize: Vec<LayerHook>,
    pub(crate) before_on_create: Vec<LayerHook>,
    pub(crate) after_on_create: Vec<LayerHook>,
    pub(crate) before_update_content: Vec<LayerHook>,
    pub(crate) after_update_content: Vec<LayerHook>,
    pub(crate) before_post_create: Vec<LayerHook>,
    pub(crate) after_post_create: Vec<LayerHook>,
    pub(crate) before_post_start: Vec<LayerHook>,
    pub(crate) after_post_start: Vec<LayerHook>,
    pub(crate) before_post_attach: Vec<LayerHook>,
    pub(crate) after_post_attach: Vec<LayerHook>,
}

impl LayerHooks {
    fn from_raw(raw: RawHooksConfig) -> Self {
        Self {
            before_initialize: raw
                .before_initialize
                .into_iter()
                .map(LayerHook::from_raw)
                .collect(),
            after_initialize: raw
                .after_initialize
                .into_iter()
                .map(LayerHook::from_raw)
                .collect(),
            before_on_create: raw
                .before_on_create
                .into_iter()
                .map(LayerHook::from_raw)
                .collect(),
            after_on_create: raw
                .after_on_create
                .into_iter()
                .map(LayerHook::from_raw)
                .collect(),
            before_update_content: raw
                .before_update_content
                .into_iter()
                .map(LayerHook::from_raw)
                .collect(),
            after_update_content: raw
                .after_update_content
                .into_iter()
                .map(LayerHook::from_raw)
                .collect(),
            before_post_create: raw
                .before_post_create
                .into_iter()
                .map(LayerHook::from_raw)
                .collect(),
            after_post_create: raw
                .after_post_create
                .into_iter()
                .map(LayerHook::from_raw)
                .collect(),
            before_post_start: raw
                .before_post_start
                .into_iter()
                .map(LayerHook::from_raw)
                .collect(),
            after_post_start: raw
                .after_post_start
                .into_iter()
                .map(LayerHook::from_raw)
                .collect(),
            before_post_attach: raw
                .before_post_attach
                .into_iter()
                .map(LayerHook::from_raw)
                .collect(),
            after_post_attach: raw
                .after_post_attach
                .into_iter()
                .map(LayerHook::from_raw)
                .collect(),
        }
    }

    pub(crate) fn append(&mut self, other: Self) {
        self.before_initialize.extend(other.before_initialize);
        self.after_initialize.extend(other.after_initialize);
        self.before_on_create.extend(other.before_on_create);
        self.after_on_create.extend(other.after_on_create);
        self.before_update_content
            .extend(other.before_update_content);
        self.after_update_content.extend(other.after_update_content);
        self.before_post_create.extend(other.before_post_create);
        self.after_post_create.extend(other.after_post_create);
        self.before_post_start.extend(other.before_post_start);
        self.after_post_start.extend(other.after_post_start);
        self.before_post_attach.extend(other.before_post_attach);
        self.after_post_attach.extend(other.after_post_attach);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LayerHook {
    pub(crate) command: Command,
    pub(crate) location: Option<HookLocation>,
    pub(crate) user: Option<String>,
    pub(crate) shell: bool,
    pub(crate) workdir: Option<String>,
}

impl LayerHook {
    fn from_raw(raw: RawHookConfig) -> Self {
        let command = Command::from(raw.command);
        let shell = raw.shell.unwrap_or(matches!(command, Command::Shell(_)));

        Self {
            command,
            location: raw.location.map(Into::into),
            user: raw.user,
            shell,
            workdir: raw.workdir,
        }
    }
}

pub(crate) fn canonical_feature_id(id: &str) -> String {
    if id.starts_with("./") {
        return id.to_owned();
    }

    let without_digest = id.split_once('@').map_or(id, |(base, _)| base);
    let last_slash = without_digest.rfind('/');
    let last_colon = without_digest.rfind(':');

    let canonical = match (last_slash, last_colon) {
        (_, Some(colon)) if last_slash.is_none_or(|slash| colon > slash) => {
            without_digest.split_at(colon).0
        }
        _ => without_digest,
    };

    canonical.to_ascii_lowercase()
}

pub(crate) fn feature_merge_identity(id: &str) -> String {
    let canonical_id = canonical_feature_id(id);
    if id.starts_with("./") {
        return canonical_id;
    }

    if let Some((_, digest)) = id.split_once('@') {
        return format!("{canonical_id}@{digest}");
    }

    let last_slash = id.rfind('/');
    let last_colon = id.rfind(':');
    let tag = match (last_slash, last_colon) {
        (_, Some(colon)) if last_slash.is_none_or(|slash| colon > slash) => {
            id.split_at(colon + ':'.len_utf8()).1
        }
        _ => "latest",
    };

    format!("{canonical_id}:{tag}")
}
