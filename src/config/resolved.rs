use std::collections::BTreeMap;

use toml::Value;

use crate::config::{
    layer::{
        LayerDevcontainerMount, LayerDevcontainerSource, LayerHook, LayerHooks, LayerPort,
        LayerPortAttributes, LayerPublishPort, LayerRunArg, LayerShutdownAction, LayerUserEnvProbe,
    },
    path::ConfigPathOrigin,
    types::{
        DEFAULT_AUTO_PORT_MAX, DEFAULT_AUTO_PORT_MIN, DotfileConflict, GitHttpsMode,
        GithubCredentialsMode, MountCreate, MountType, OnAutoForward, SshAgentMode,
    },
};

#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct ResolvedConfig {
    pub(crate) shell: Option<String>,
    pub(crate) features: Vec<ResolvedFeature>,
    pub(crate) dotfile_entries: Vec<ResolvedDotfileEntry>,
    pub(crate) dotfiles: Vec<ResolvedDotfile>,
    pub(crate) mounts: Vec<ResolvedMount>,
    pub(crate) ports: ResolvedPorts,
    pub(crate) compose: ResolvedCompose,
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
    pub(crate) origin: ConfigPathOrigin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResolvedDotfileEntry {
    Enabled(ResolvedDotfile),
    Disabled(ResolvedDotfileDisable),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedDotfileDisable {
    pub(crate) target: String,
    pub(crate) origin: ConfigPathOrigin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedMount {
    pub(crate) source: Option<String>,
    pub(crate) target: String,
    pub(crate) mount_type: MountType,
    pub(crate) read_only: bool,
    pub(crate) resolve_symlink: bool,
    pub(crate) create: Option<MountCreate>,
    pub(crate) origin: ConfigPathOrigin,
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

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ResolvedCompose {
    pub(crate) published_ports: ResolvedPublishedPorts,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ResolvedPublishedPorts {
    pub(crate) fallback: bool,
    pub(crate) warn_on_relocation: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ResolvedDevcontainer {
    pub(crate) source: Option<ResolvedDevcontainerSource>,
    pub(crate) override_feature_install_order: Vec<String>,
    pub(crate) mounts: Vec<ResolvedDevcontainerMount>,
    pub(crate) workspace_mount: Option<String>,
    pub(crate) workspace_folder: Option<String>,
    pub(crate) container_env: BTreeMap<String, String>,
    pub(crate) remote_env: BTreeMap<String, String>,
    pub(crate) remote_user: Option<String>,
    pub(crate) container_user: Option<String>,
    pub(crate) update_remote_user_uid: bool,
    pub(crate) override_command: bool,
    pub(crate) user_env_probe: Option<ResolvedUserEnvProbe>,
    pub(crate) publish_ports: Vec<ResolvedPublishPort>,
    pub(crate) port_attributes: BTreeMap<String, ResolvedPortAttributes>,
    pub(crate) other_ports_attributes: Option<ResolvedPortAttributes>,
    pub(crate) run_args: Vec<ResolvedRunArg>,
    pub(crate) init: Option<bool>,
    pub(crate) privileged: Option<bool>,
    pub(crate) cap_add: Vec<String>,
    pub(crate) security_opt: Vec<String>,
    pub(crate) entrypoints: Vec<String>,
    pub(crate) shutdown_action: ResolvedShutdownAction,
    pub(crate) lifecycle: Option<crate::devcontainer::lifecycle::LifecycleDefinition>,
}

impl Default for ResolvedDevcontainer {
    fn default() -> Self {
        Self {
            source: None,
            override_feature_install_order: Vec::new(),
            mounts: Vec::new(),
            workspace_mount: None,
            workspace_folder: None,
            container_env: BTreeMap::new(),
            remote_env: BTreeMap::new(),
            remote_user: None,
            container_user: None,
            update_remote_user_uid: true,
            override_command: true,
            user_env_probe: None,
            publish_ports: Vec::new(),
            port_attributes: BTreeMap::new(),
            other_ports_attributes: None,
            run_args: Vec::new(),
            init: None,
            privileged: None,
            cap_add: Vec::new(),
            security_opt: Vec::new(),
            entrypoints: Vec::new(),
            shutdown_action: ResolvedShutdownAction::StopContainer,
            lifecycle: None,
        }
    }
}

impl ResolvedDevcontainer {
    pub(crate) fn init_enabled(&self) -> bool {
        self.init.unwrap_or(false)
    }

    pub(crate) fn privileged_enabled(&self) -> bool {
        self.privileged.unwrap_or(false)
    }
}

pub(crate) type ResolvedDevcontainerSource = LayerDevcontainerSource;
pub(crate) type ResolvedShutdownAction = LayerShutdownAction;
pub(crate) type ResolvedDevcontainerMount = LayerDevcontainerMount;
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
