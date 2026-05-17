#![allow(dead_code)]

use std::collections::BTreeMap;

use toml::Value;

use crate::config::{
    layer::{
        LayerDevcontainerBuild, LayerDevcontainerSource, LayerHook, LayerHooks, LayerPort,
        LayerPortAttributes, LayerPublishPort, LayerRunArg, LayerUserEnvProbe,
    },
    types::{
        DEFAULT_AUTO_PORT_MAX, DEFAULT_AUTO_PORT_MIN, DotfileConflict, GitHttpsMode,
        GithubCredentialsMode, MountCreate, MountType, OnAutoForward, SshAgentMode,
    },
};

#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct ResolvedConfig {
    pub(crate) shell: Option<String>,
    pub(crate) features: Vec<ResolvedFeature>,
    pub(crate) dotfiles: Vec<ResolvedDotfile>,
    pub(crate) mounts: Vec<ResolvedMount>,
    pub(crate) ports: ResolvedPorts,
    pub(crate) devcontainer: ResolvedDevcontainer,
    pub(crate) credentials: ResolvedCredentials,
    pub(crate) hooks: ResolvedHooks,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ResolvedFeature {
    pub(crate) id: String,
    pub(crate) canonical_id: String,
    pub(crate) options: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedDotfile {
    pub(crate) source: String,
    pub(crate) target: String,
    pub(crate) read_only: bool,
    pub(crate) resolve_symlink: bool,
    pub(crate) on_conflict: DotfileConflict,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedMount {
    pub(crate) source: Option<String>,
    pub(crate) target: String,
    pub(crate) mount_type: MountType,
    pub(crate) read_only: bool,
    pub(crate) resolve_symlink: bool,
    pub(crate) create: Option<MountCreate>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ResolvedPorts {
    pub(crate) entries: Vec<ResolvedPort>,
    pub(crate) auto: ResolvedAutoPorts,
}

pub(crate) type ResolvedPort = LayerPort;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedAutoPorts {
    pub(crate) enabled: bool,
    pub(crate) min: u16,
    pub(crate) max: u16,
    pub(crate) ignore: Vec<u16>,
    pub(crate) on_auto_forward: OnAutoForward,
}

impl Default for ResolvedAutoPorts {
    fn default() -> Self {
        Self {
            enabled: true,
            min: DEFAULT_AUTO_PORT_MIN,
            max: DEFAULT_AUTO_PORT_MAX,
            ignore: Vec::new(),
            on_auto_forward: OnAutoForward::Notify,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct ResolvedDevcontainer {
    pub(crate) source: Option<ResolvedDevcontainerSource>,
    pub(crate) override_feature_install_order: Vec<String>,
    pub(crate) mounts: Vec<String>,
    pub(crate) workspace_mount: Option<String>,
    pub(crate) workspace_folder: Option<String>,
    pub(crate) container_env: BTreeMap<String, String>,
    pub(crate) remote_env: BTreeMap<String, String>,
    pub(crate) remote_user: Option<String>,
    pub(crate) container_user: Option<String>,
    pub(crate) update_remote_user_uid: Option<bool>,
    pub(crate) user_env_probe: Option<ResolvedUserEnvProbe>,
    pub(crate) publish_ports: Vec<ResolvedPublishPort>,
    pub(crate) port_attributes: BTreeMap<String, ResolvedPortAttributes>,
    pub(crate) other_ports_attributes: Option<ResolvedPortAttributes>,
    pub(crate) run_args: Vec<ResolvedRunArg>,
    pub(crate) init: bool,
    pub(crate) privileged: bool,
    pub(crate) cap_add: Vec<String>,
    pub(crate) security_opt: Vec<String>,
    pub(crate) lifecycle: Option<crate::devcontainer::lifecycle::LifecycleDefinition>,
}

pub(crate) type ResolvedDevcontainerSource = LayerDevcontainerSource;
pub(crate) type ResolvedDevcontainerBuild = LayerDevcontainerBuild;
pub(crate) type ResolvedUserEnvProbe = LayerUserEnvProbe;
pub(crate) type ResolvedPublishPort = LayerPublishPort;
pub(crate) type ResolvedPortAttributes = LayerPortAttributes;
pub(crate) type ResolvedRunArg = LayerRunArg;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ResolvedCredentials {
    pub(crate) git: ResolvedGitCredentials,
    pub(crate) github: ResolvedGithubCredentials,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedGitCredentials {
    pub(crate) enabled: bool,
    pub(crate) copy_user: bool,
    pub(crate) copy_global_config: bool,
    pub(crate) https: GitHttpsMode,
    pub(crate) ssh_agent: SshAgentMode,
}

impl Default for ResolvedGitCredentials {
    fn default() -> Self {
        Self {
            enabled: true,
            copy_user: true,
            copy_global_config: false,
            https: GitHttpsMode::HostHelper,
            ssh_agent: SshAgentMode::Auto,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedGithubCredentials {
    pub(crate) enabled: bool,
    pub(crate) mode: GithubCredentialsMode,
    pub(crate) install_feature_if_missing: bool,
}

impl Default for ResolvedGithubCredentials {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: GithubCredentialsMode::GhTokenFile,
            install_feature_if_missing: true,
        }
    }
}

pub(crate) type ResolvedHooks = LayerHooks;
pub(crate) type ResolvedHook = LayerHook;
