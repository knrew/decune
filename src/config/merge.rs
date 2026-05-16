#![allow(dead_code)]

use std::collections::BTreeMap;

use toml::Value;

use crate::config::schema::{
    RawAutoPortsConfig, RawCommand, RawCredentialsConfig, RawDecuneConfig, RawDotfileConfig,
    RawDotfileConflict, RawFeatureConfig, RawGitCredentialsConfig, RawGitHttpsMode,
    RawGithubCredentialsConfig, RawGithubCredentialsMode, RawHookConfig, RawHookLocation,
    RawHooksConfig, RawMountConfig, RawMountCreate, RawMountType, RawOnAutoForward, RawPortConfig,
    RawPortProtocol, RawSshAgentMode,
};

#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct ConfigMergeInput {
    pub(crate) image_metadata: Option<ConfigLayer>,
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
    pub(crate) auto_ports: Option<LayerAutoPorts>,
    pub(crate) credentials: LayerCredentials,
    pub(crate) hooks: LayerHooks,
}

impl ConfigLayer {
    pub(crate) fn from_raw_decune(raw: RawDecuneConfig) -> Self {
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
                .map(LayerDotfile::from_raw)
                .collect(),
            mounts: raw.mounts.into_iter().map(LayerMount::from_raw).collect(),
            ports: raw
                .ports
                .entries
                .into_iter()
                .map(LayerPort::from_raw)
                .collect(),
            auto_ports: raw.ports.auto.map(LayerAutoPorts::from_raw),
            credentials: LayerCredentials::from_raw(raw.credentials),
            hooks: LayerHooks::from_raw(raw.hooks),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct ResolvedConfig {
    pub(crate) shell: Option<String>,
    pub(crate) features: Vec<ResolvedFeature>,
    pub(crate) dotfiles: Vec<ResolvedDotfile>,
    pub(crate) mounts: Vec<ResolvedMount>,
    pub(crate) ports: ResolvedPorts,
    pub(crate) credentials: ResolvedCredentials,
    pub(crate) hooks: ResolvedHooks,
}

pub(crate) fn resolve_config(input: ConfigMergeInput) -> ResolvedConfig {
    let mut accumulator = MergeAccumulator::default();

    for layer in [
        input.image_metadata,
        input.global,
        input.devcontainer,
        input.project,
        input.cli,
    ]
    .into_iter()
    .flatten()
    {
        accumulator.apply_layer(layer);
    }

    accumulator.into_resolved()
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
        Self {
            canonical_id: canonical_feature_id(&id),
            id,
            enabled: true,
            options: BTreeMap::new(),
        }
    }

    fn from_raw(id: String, raw: RawFeatureConfig) -> Self {
        Self {
            canonical_id: canonical_feature_id(&id),
            id,
            enabled: raw.enabled.unwrap_or(true),
            options: raw.options,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ResolvedFeature {
    pub(crate) id: String,
    pub(crate) canonical_id: String,
    pub(crate) options: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LayerDotfile {
    pub(crate) enabled: bool,
    pub(crate) source: Option<String>,
    pub(crate) target: String,
    pub(crate) read_only: bool,
    pub(crate) resolve_symlink: bool,
    pub(crate) on_conflict: DotfileConflict,
}

impl LayerDotfile {
    fn from_raw(raw: RawDotfileConfig) -> Self {
        Self {
            enabled: raw.enabled.unwrap_or(true),
            source: raw.source,
            target: raw.target,
            read_only: raw.read_only.unwrap_or(true),
            resolve_symlink: raw.resolve_symlink.unwrap_or(true),
            on_conflict: raw.on_conflict.map(Into::into).unwrap_or_default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedDotfile {
    pub(crate) source: String,
    pub(crate) target: String,
    pub(crate) read_only: bool,
    pub(crate) resolve_symlink: bool,
    pub(crate) on_conflict: DotfileConflict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum DotfileConflict {
    #[default]
    Fail,
    ReplaceSymlink,
    Backup,
}

impl From<RawDotfileConflict> for DotfileConflict {
    fn from(value: RawDotfileConflict) -> Self {
        match value {
            RawDotfileConflict::Fail => Self::Fail,
            RawDotfileConflict::ReplaceSymlink => Self::ReplaceSymlink,
            RawDotfileConflict::Backup => Self::Backup,
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
}

impl LayerMount {
    fn from_raw(raw: RawMountConfig) -> Self {
        Self {
            enabled: raw.enabled.unwrap_or(true),
            source: raw.source,
            target: raw.target,
            mount_type: raw.mount_type.map(Into::into),
            read_only: raw.read_only.unwrap_or(false),
            resolve_symlink: raw.resolve_symlink.unwrap_or(true),
            create: raw.create.map(Into::into),
        }
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MountType {
    Bind,
    Volume,
    Tmpfs,
}

impl From<RawMountType> for MountType {
    fn from(value: RawMountType) -> Self {
        match value {
            RawMountType::Bind => Self::Bind,
            RawMountType::Volume => Self::Volume,
            RawMountType::Tmpfs => Self::Tmpfs,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MountCreate {
    Directory,
}

impl From<RawMountCreate> for MountCreate {
    fn from(value: RawMountCreate) -> Self {
        match value {
            RawMountCreate::Directory => Self::Directory,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ResolvedPorts {
    pub(crate) entries: Vec<ResolvedPort>,
    pub(crate) auto: ResolvedAutoPorts,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LayerPort {
    pub(crate) enabled: bool,
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
            container: raw.container,
            host: raw.host,
            host_ip: raw.host_ip.unwrap_or_else(|| "127.0.0.1".to_owned()),
            protocol: raw.protocol.unwrap_or(RawPortProtocol::Tcp).into(),
            require_local: raw.require_local.unwrap_or(false),
            label: raw.label,
        }
    }
}

pub(crate) type ResolvedPort = LayerPort;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum PortProtocol {
    Tcp,
}

impl From<RawPortProtocol> for PortProtocol {
    fn from(value: RawPortProtocol) -> Self {
        match value {
            RawPortProtocol::Tcp => Self::Tcp,
        }
    }
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
            min: 1024,
            max: 32768,
            ignore: Vec::new(),
            on_auto_forward: OnAutoForward::Notify,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OnAutoForward {
    Notify,
    Silent,
    Ignore,
}

impl From<RawOnAutoForward> for OnAutoForward {
    fn from(value: RawOnAutoForward) -> Self {
        match value {
            RawOnAutoForward::Notify => Self::Notify,
            RawOnAutoForward::Silent => Self::Silent,
            RawOnAutoForward::Ignore => Self::Ignore,
            RawOnAutoForward::OpenBrowser => Self::Notify,
        }
    }
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
pub(crate) struct ResolvedCredentials {
    pub(crate) git: ResolvedGitCredentials,
    pub(crate) github: ResolvedGithubCredentials,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GitHttpsMode {
    Off,
    HostHelper,
}

impl From<RawGitHttpsMode> for GitHttpsMode {
    fn from(value: RawGitHttpsMode) -> Self {
        match value {
            RawGitHttpsMode::Off => Self::Off,
            RawGitHttpsMode::HostHelper => Self::HostHelper,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SshAgentMode {
    Off,
    Auto,
    Required,
}

impl From<RawSshAgentMode> for SshAgentMode {
    fn from(value: RawSshAgentMode) -> Self {
        match value {
            RawSshAgentMode::Off => Self::Off,
            RawSshAgentMode::Auto => Self::Auto,
            RawSshAgentMode::Required => Self::Required,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GithubCredentialsMode {
    Off,
    GhTokenFile,
}

impl From<RawGithubCredentialsMode> for GithubCredentialsMode {
    fn from(value: RawGithubCredentialsMode) -> Self {
        match value {
            RawGithubCredentialsMode::Off => Self::Off,
            RawGithubCredentialsMode::GhTokenFile => Self::GhTokenFile,
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

    fn append(&mut self, other: Self) {
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

pub(crate) type ResolvedHooks = LayerHooks;

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

pub(crate) type ResolvedHook = LayerHook;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Command {
    Shell(String),
    Args(Vec<String>),
}

impl From<RawCommand> for Command {
    fn from(value: RawCommand) -> Self {
        match value {
            RawCommand::Shell(command) => Self::Shell(command),
            RawCommand::Args(args) => Self::Args(args),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HookLocation {
    Host,
    Container,
}

impl From<RawHookLocation> for HookLocation {
    fn from(value: RawHookLocation) -> Self {
        match value {
            RawHookLocation::Host => Self::Host,
            RawHookLocation::Container => Self::Container,
        }
    }
}

#[derive(Debug, Default)]
struct MergeAccumulator {
    shell: Option<String>,
    features: Vec<ResolvedFeature>,
    dotfiles: Vec<ResolvedDotfile>,
    mounts: Vec<ResolvedMount>,
    ports: Vec<ResolvedPort>,
    auto_ports: ResolvedAutoPorts,
    credentials: ResolvedCredentials,
    hooks: ResolvedHooks,
}

impl MergeAccumulator {
    fn apply_layer(&mut self, layer: ConfigLayer) {
        if let Some(shell) = layer.shell {
            self.shell = Some(shell);
        }

        for feature in layer.features {
            self.merge_feature(feature);
        }

        for dotfile in layer.dotfiles {
            self.merge_dotfile(dotfile);
        }

        for mount in layer.mounts {
            self.merge_mount(mount);
        }

        for port in layer.ports {
            self.merge_port(port);
        }

        if let Some(auto_ports) = layer.auto_ports {
            self.merge_auto_ports(auto_ports);
        }

        self.merge_credentials(layer.credentials);
        self.hooks.append(layer.hooks);
    }

    fn merge_feature(&mut self, feature: LayerFeature) {
        if let Some(position) = self
            .features
            .iter()
            .position(|entry| entry.canonical_id == feature.canonical_id)
        {
            if feature.enabled {
                let existing = &mut self.features[position];
                existing.id = feature.id;
                existing.options.extend(feature.options);
            } else {
                self.features.remove(position);
            }
            return;
        }

        if feature.enabled {
            self.features.push(ResolvedFeature {
                id: feature.id,
                canonical_id: feature.canonical_id,
                options: feature.options,
            });
        }
    }

    fn merge_dotfile(&mut self, dotfile: LayerDotfile) {
        if !dotfile.enabled {
            remove_by_identity(&mut self.dotfiles, |existing| {
                existing.target == dotfile.target
            });
            return;
        }

        if let Some(source) = dotfile.source {
            replace_by_identity(
                &mut self.dotfiles,
                ResolvedDotfile {
                    source,
                    target: dotfile.target,
                    read_only: dotfile.read_only,
                    resolve_symlink: dotfile.resolve_symlink,
                    on_conflict: dotfile.on_conflict,
                },
                |left, right| left.target == right.target,
            );
        }
    }

    fn merge_mount(&mut self, mount: LayerMount) {
        if !mount.enabled {
            remove_by_identity(&mut self.mounts, |existing| existing.target == mount.target);
            return;
        }

        if let Some(mount_type) = mount.mount_type {
            replace_by_identity(
                &mut self.mounts,
                ResolvedMount {
                    source: mount.source,
                    target: mount.target,
                    mount_type,
                    read_only: mount.read_only,
                    resolve_symlink: mount.resolve_symlink,
                    create: mount.create,
                },
                |left, right| left.target == right.target,
            );
        }
    }

    fn merge_port(&mut self, port: LayerPort) {
        if !port.enabled {
            remove_by_identity(&mut self.ports, |existing| {
                existing.protocol == port.protocol
                    && existing.container == port.container
                    && existing.host_ip == port.host_ip
            });
            return;
        }

        replace_by_identity(&mut self.ports, port, same_port_identity);
    }

    fn merge_auto_ports(&mut self, auto_ports: LayerAutoPorts) {
        if let Some(enabled) = auto_ports.enabled {
            self.auto_ports.enabled = enabled;
        }
        if let Some(min) = auto_ports.min {
            self.auto_ports.min = min;
        }
        if let Some(max) = auto_ports.max {
            self.auto_ports.max = max;
        }
        if let Some(ignore) = auto_ports.ignore {
            self.auto_ports.ignore = ignore;
        }
        if let Some(on_auto_forward) = auto_ports.on_auto_forward {
            self.auto_ports.on_auto_forward = on_auto_forward;
        }
    }

    fn merge_credentials(&mut self, credentials: LayerCredentials) {
        if let Some(enabled) = credentials.git.enabled {
            self.credentials.git.enabled = enabled;
        }
        if let Some(copy_user) = credentials.git.copy_user {
            self.credentials.git.copy_user = copy_user;
        }
        if let Some(copy_global_config) = credentials.git.copy_global_config {
            self.credentials.git.copy_global_config = copy_global_config;
        }
        if let Some(https) = credentials.git.https {
            self.credentials.git.https = https;
        }
        if let Some(ssh_agent) = credentials.git.ssh_agent {
            self.credentials.git.ssh_agent = ssh_agent;
        }

        if let Some(enabled) = credentials.github.enabled {
            self.credentials.github.enabled = enabled;
        }
        if let Some(mode) = credentials.github.mode {
            self.credentials.github.mode = mode;
        }
        if let Some(install_feature_if_missing) = credentials.github.install_feature_if_missing {
            self.credentials.github.install_feature_if_missing = install_feature_if_missing;
        }
    }

    fn into_resolved(self) -> ResolvedConfig {
        ResolvedConfig {
            shell: self.shell,
            features: self.features,
            dotfiles: self.dotfiles,
            mounts: self.mounts,
            ports: ResolvedPorts {
                entries: self.ports,
                auto: self.auto_ports,
            },
            credentials: self.credentials,
            hooks: self.hooks,
        }
    }
}

fn replace_by_identity<T, F>(entries: &mut Vec<T>, entry: T, same_identity: F)
where
    F: Fn(&T, &T) -> bool,
{
    if let Some(position) = entries
        .iter()
        .position(|existing| same_identity(existing, &entry))
    {
        entries[position] = entry;
    } else {
        entries.push(entry);
    }
}

fn remove_by_identity<T, F>(entries: &mut Vec<T>, same_identity: F)
where
    F: Fn(&T) -> bool,
{
    entries.retain(|existing| !same_identity(existing));
}

fn same_port_identity(left: &ResolvedPort, right: &ResolvedPort) -> bool {
    left.protocol == right.protocol
        && left.container == right.container
        && left.host_ip == right.host_ip
}

fn canonical_feature_id(id: &str) -> String {
    let without_digest = id.split_once('@').map_or(id, |(base, _)| base);
    let last_slash = without_digest.rfind('/');
    let last_colon = without_digest.rfind(':');

    match (last_slash, last_colon) {
        (_, Some(colon)) if last_slash.is_none_or(|slash| colon > slash) => {
            without_digest[..colon].to_owned()
        }
        _ => without_digest.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw_config(contents: &str) -> RawDecuneConfig {
        toml::from_str(contents).expect("test config should parse")
    }

    fn raw_layer(contents: &str) -> ConfigLayer {
        ConfigLayer::from_raw_decune(raw_config(contents))
    }

    fn shell_commands(hooks: &[LayerHook]) -> Vec<&str> {
        hooks
            .iter()
            .map(|hook| match &hook.command {
                Command::Shell(command) => command.as_str(),
                Command::Args(_) => panic!("expected shell command"),
            })
            .collect()
    }

    #[test]
    fn empty_input_resolves_documented_defaults() {
        let config = resolve_config(ConfigMergeInput::default());

        assert_eq!(config.shell, None);
        assert!(config.features.is_empty());
        assert!(config.dotfiles.is_empty());
        assert!(config.mounts.is_empty());
        assert!(config.ports.entries.is_empty());
        assert!(config.ports.auto.enabled);
        assert_eq!(config.ports.auto.min, 1024);
        assert_eq!(config.ports.auto.max, 32768);
        assert!(config.ports.auto.ignore.is_empty());
        assert_eq!(config.ports.auto.on_auto_forward, OnAutoForward::Notify);
        assert!(config.credentials.git.enabled);
        assert!(config.credentials.git.copy_user);
        assert!(!config.credentials.git.copy_global_config);
        assert_eq!(config.credentials.git.https, GitHttpsMode::HostHelper);
        assert_eq!(config.credentials.git.ssh_agent, SshAgentMode::Auto);
        assert!(config.credentials.github.enabled);
        assert_eq!(
            config.credentials.github.mode,
            GithubCredentialsMode::GhTokenFile
        );
        assert!(config.credentials.github.install_feature_if_missing);
    }

    #[test]
    fn hooks_follow_documented_layer_order() {
        let config = resolve_config(ConfigMergeInput {
            image_metadata: Some(raw_layer(
                r#"
version = 1

[[hooks.before_initialize]]
command = "image.sh"
"#,
            )),
            global: Some(raw_layer(
                r#"
version = 1

[[hooks.before_initialize]]
command = "global.sh"
"#,
            )),
            devcontainer: Some(raw_layer(
                r#"
version = 1

[[hooks.before_initialize]]
command = "devcontainer.sh"
"#,
            )),
            project: Some(raw_layer(
                r#"
version = 1

[[hooks.before_initialize]]
command = "project.sh"
"#,
            )),
            cli: Some(raw_layer(
                r#"
version = 1

[[hooks.before_initialize]]
command = "cli.sh"
"#,
            )),
        });

        assert_eq!(
            shell_commands(&config.hooks.before_initialize),
            vec![
                "image.sh",
                "global.sh",
                "devcontainer.sh",
                "project.sh",
                "cli.sh"
            ]
        );
    }

    #[test]
    fn project_can_disable_global_feature_by_canonical_id() {
        let config = resolve_config(ConfigMergeInput {
            global: Some(raw_layer(
                r#"
version = 1

[features."ghcr.io/devcontainers/features/github-cli:1"]
version = "latest"
"#,
            )),
            project: Some(raw_layer(
                r#"
version = 1

[features."ghcr.io/devcontainers/features/github-cli:2"]
enabled = false
"#,
            )),
            ..ConfigMergeInput::default()
        });

        assert!(config.features.is_empty());
    }

    #[test]
    fn feature_can_be_readded_after_disable_in_later_layer() {
        let config = resolve_config(ConfigMergeInput {
            global: Some(raw_layer(
                r#"
version = 1

[features."ghcr.io/example/features/tool:1"]
version = "global"
"#,
            )),
            devcontainer: Some(raw_layer(
                r#"
version = 1

[features."ghcr.io/example/features/tool:2"]
enabled = false
"#,
            )),
            project: Some(raw_layer(
                r#"
version = 1

[features."ghcr.io/example/features/tool:3"]
version = "project"
"#,
            )),
            ..ConfigMergeInput::default()
        });

        assert_eq!(config.features.len(), 1);
        assert_eq!(config.features[0].id, "ghcr.io/example/features/tool:3");
        assert_eq!(
            config.features[0].options.get("version"),
            Some(&Value::String("project".to_owned()))
        );
    }

    #[test]
    fn canonical_feature_id_preserves_registry_port() {
        assert_eq!(
            canonical_feature_id("localhost:5000/devcontainers/features/tool:1"),
            "localhost:5000/devcontainers/features/tool"
        );
        assert_eq!(
            canonical_feature_id("localhost:5000/devcontainers/features/tool@sha256:abcd"),
            "localhost:5000/devcontainers/features/tool"
        );
    }

    #[test]
    fn feature_options_merge_by_canonical_id() {
        let config = resolve_config(ConfigMergeInput {
            global: Some(raw_layer(
                r#"
version = 1

[features."ghcr.io/example/features/tool:1"]
version = "1"
channel = "stable"
"#,
            )),
            project: Some(raw_layer(
                r#"
version = 1

[features."ghcr.io/example/features/tool@sha256:abcd"]
version = "2"
"#,
            )),
            ..ConfigMergeInput::default()
        });

        assert_eq!(config.features.len(), 1);
        assert_eq!(
            config.features[0].id,
            "ghcr.io/example/features/tool@sha256:abcd"
        );
        assert_eq!(
            config.features[0].options.get("version"),
            Some(&Value::String("2".to_owned()))
        );
        assert_eq!(
            config.features[0].options.get("channel"),
            Some(&Value::String("stable".to_owned()))
        );
    }

    #[test]
    fn project_mount_replaces_same_target() {
        let config = resolve_config(ConfigMergeInput {
            global: Some(raw_layer(
                r#"
version = 1

[[mounts]]
source = "/global"
target = "/work"
type = "bind"
read_only = true
"#,
            )),
            project: Some(raw_layer(
                r#"
version = 1

[[mounts]]
source = "/project"
target = "/work"
type = "bind"
"#,
            )),
            ..ConfigMergeInput::default()
        });

        assert_eq!(config.mounts.len(), 1);
        assert_eq!(config.mounts[0].source.as_deref(), Some("/project"));
        assert!(!config.mounts[0].read_only);
    }

    #[test]
    fn project_dotfile_replaces_same_target() {
        let config = resolve_config(ConfigMergeInput {
            global: Some(raw_layer(
                r#"
version = 1

[[dotfiles]]
source = "~/.config/nvim"
target = ".config/nvim"
read_only = true
"#,
            )),
            project: Some(raw_layer(
                r#"
version = 1

[[dotfiles]]
source = ".decune/nvim"
target = ".config/nvim"
read_only = false
resolve_symlink = false
on_conflict = "backup"
"#,
            )),
            ..ConfigMergeInput::default()
        });

        assert_eq!(config.dotfiles.len(), 1);
        assert_eq!(config.dotfiles[0].source, ".decune/nvim");
        assert!(!config.dotfiles[0].read_only);
        assert!(!config.dotfiles[0].resolve_symlink);
        assert_eq!(config.dotfiles[0].on_conflict, DotfileConflict::Backup);
    }

    #[test]
    fn dotfile_and_mount_defaults_are_documented() {
        let config = resolve_config(ConfigMergeInput {
            project: Some(raw_layer(
                r#"
version = 1

[[dotfiles]]
source = "~/.config/git"
target = ".config/git"

[[mounts]]
source = "/workspace"
target = "/workspace"
type = "bind"
"#,
            )),
            ..ConfigMergeInput::default()
        });

        assert!(config.dotfiles[0].read_only);
        assert!(config.dotfiles[0].resolve_symlink);
        assert_eq!(config.dotfiles[0].on_conflict, DotfileConflict::Fail);
        assert!(!config.mounts[0].read_only);
        assert!(config.mounts[0].resolve_symlink);
        assert_eq!(config.mounts[0].create, None);
    }

    #[test]
    fn project_can_disable_global_mount_by_target() {
        let config = resolve_config(ConfigMergeInput {
            global: Some(raw_layer(
                r#"
version = 1

[[mounts]]
source = "/global"
target = "/work"
type = "bind"
"#,
            )),
            project: Some(raw_layer(
                r#"
version = 1

[[mounts]]
target = "/work"
enabled = false
"#,
            )),
            ..ConfigMergeInput::default()
        });

        assert!(config.mounts.is_empty());
    }

    #[test]
    fn project_can_disable_global_dotfile_by_target() {
        let config = resolve_config(ConfigMergeInput {
            global: Some(raw_layer(
                r#"
version = 1

[[dotfiles]]
source = "~/.config/nvim"
target = ".config/nvim"
"#,
            )),
            project: Some(raw_layer(
                r#"
version = 1

[[dotfiles]]
target = ".config/nvim"
enabled = false
"#,
            )),
            ..ConfigMergeInput::default()
        });

        assert!(config.dotfiles.is_empty());
    }

    #[test]
    fn project_can_disable_global_port_by_identity() {
        let config = resolve_config(ConfigMergeInput {
            global: Some(raw_layer(
                r#"
version = 1

[[ports]]
container = 3000
host = 3000
label = "global"
"#,
            )),
            project: Some(raw_layer(
                r#"
version = 1

[[ports]]
container = 3000
enabled = false
"#,
            )),
            ..ConfigMergeInput::default()
        });

        assert!(config.ports.entries.is_empty());
    }

    #[test]
    fn disabled_port_identity_ignores_host_port_but_keeps_host_ip_boundary() {
        let config = resolve_config(ConfigMergeInput {
            global: Some(raw_layer(
                r#"
version = 1

[[ports]]
container = 3000
host = 3000
host_ip = "127.0.0.1"
label = "loopback"

[[ports]]
container = 3000
host = 3000
host_ip = "0.0.0.0"
label = "public"
"#,
            )),
            project: Some(raw_layer(
                r#"
version = 1

[[ports]]
container = 3000
host = 13000
host_ip = "127.0.0.1"
enabled = false
"#,
            )),
            ..ConfigMergeInput::default()
        });

        assert_eq!(config.ports.entries.len(), 1);
        assert_eq!(config.ports.entries[0].host_ip, "0.0.0.0");
        assert_eq!(config.ports.entries[0].label.as_deref(), Some("public"));
    }

    #[test]
    fn hooks_append_in_merge_order() {
        let config = resolve_config(ConfigMergeInput {
            global: Some(raw_layer(
                r#"
version = 1

[[hooks.before_post_create]]
command = "global.sh"
"#,
            )),
            project: Some(raw_layer(
                r#"
version = 1

[[hooks.before_post_create]]
command = "project.sh"
"#,
            )),
            ..ConfigMergeInput::default()
        });

        assert_eq!(
            config.hooks.before_post_create,
            vec![
                LayerHook {
                    command: Command::Shell("global.sh".to_owned()),
                    location: None,
                    user: None,
                    shell: true,
                    workdir: None,
                },
                LayerHook {
                    command: Command::Shell("project.sh".to_owned()),
                    location: None,
                    user: None,
                    shell: true,
                    workdir: None,
                },
            ]
        );
    }

    #[test]
    fn hook_shell_defaults_follow_command_form() {
        let config = resolve_config(ConfigMergeInput {
            project: Some(raw_layer(
                r#"
version = 1

[[hooks.before_post_create]]
command = "shell-form.sh"

[[hooks.before_post_create]]
command = ["bash", "args-form.sh"]
"#,
            )),
            ..ConfigMergeInput::default()
        });

        assert!(config.hooks.before_post_create[0].shell);
        assert!(!config.hooks.before_post_create[1].shell);
    }

    #[test]
    fn explicit_hook_shell_overrides_default() {
        let config = resolve_config(ConfigMergeInput {
            project: Some(raw_layer(
                r#"
version = 1

[[hooks.before_post_create]]
command = "shell-form.sh"
shell = false

[[hooks.before_post_create]]
command = ["bash", "args-form.sh"]
shell = true
"#,
            )),
            ..ConfigMergeInput::default()
        });

        assert!(!config.hooks.before_post_create[0].shell);
        assert!(config.hooks.before_post_create[1].shell);
    }

    #[test]
    fn scalar_and_cli_layer_win_by_precedence() {
        let config = resolve_config(ConfigMergeInput {
            global: Some(raw_layer("version = 1\nshell = '/bin/bash'\n")),
            project: Some(raw_layer("version = 1\nshell = '/bin/zsh'\n")),
            cli: Some(ConfigLayer {
                shell: Some("/usr/bin/fish".to_owned()),
                ..ConfigLayer::default()
            }),
            ..ConfigMergeInput::default()
        });

        assert_eq!(config.shell.as_deref(), Some("/usr/bin/fish"));
    }

    #[test]
    fn scalar_without_cli_follows_documented_layer_precedence() {
        let config = resolve_config(ConfigMergeInput {
            image_metadata: Some(raw_layer("version = 1\nshell = '/bin/image'\n")),
            global: Some(raw_layer("version = 1\nshell = '/bin/global'\n")),
            devcontainer: Some(raw_layer("version = 1\nshell = '/bin/devcontainer'\n")),
            project: Some(raw_layer("version = 1\nshell = '/bin/project'\n")),
            ..ConfigMergeInput::default()
        });

        assert_eq!(config.shell.as_deref(), Some("/bin/project"));
    }

    #[test]
    fn credentials_merge_fieldwise_by_precedence() {
        let config = resolve_config(ConfigMergeInput {
            global: Some(raw_layer(
                r#"
version = 1

[credentials.git]
enabled = false
copy_user = false
copy_global_config = true
https = "off"
ssh_agent = "required"

[credentials.github]
enabled = false
mode = "off"
install_feature_if_missing = false
"#,
            )),
            project: Some(raw_layer(
                r#"
version = 1

[credentials.git]
https = "host-helper"

[credentials.github]
enabled = true
"#,
            )),
            ..ConfigMergeInput::default()
        });

        assert!(!config.credentials.git.enabled);
        assert!(!config.credentials.git.copy_user);
        assert!(config.credentials.git.copy_global_config);
        assert_eq!(config.credentials.git.https, GitHttpsMode::HostHelper);
        assert_eq!(config.credentials.git.ssh_agent, SshAgentMode::Required);
        assert!(config.credentials.github.enabled);
        assert_eq!(config.credentials.github.mode, GithubCredentialsMode::Off);
        assert!(!config.credentials.github.install_feature_if_missing);
    }

    #[test]
    fn port_identity_replaces_protocol_container_and_host_ip() {
        let config = resolve_config(ConfigMergeInput {
            global: Some(raw_layer(
                r#"
version = 1

[[ports]]
container = 3000
host = 3000
host_ip = "127.0.0.1"
label = "global"
"#,
            )),
            project: Some(raw_layer(
                r#"
version = 1

[[ports]]
container = 3000
host = 13000
host_ip = "127.0.0.1"
label = "project"

[[ports]]
container = 3000
host = 23000
host_ip = "0.0.0.0"
label = "public"
"#,
            )),
            ..ConfigMergeInput::default()
        });

        assert_eq!(config.ports.entries.len(), 2);
        assert_eq!(config.ports.entries[0].host, Some(13000));
        assert_eq!(config.ports.entries[0].label.as_deref(), Some("project"));
        assert_eq!(config.ports.entries[1].host, Some(23000));
    }

    #[test]
    fn auto_ports_ignore_is_preserved_when_upper_layer_omits_it() {
        let config = resolve_config(ConfigMergeInput {
            global: Some(raw_layer(
                r#"
version = 1

[ports.auto]
ignore = [22]
"#,
            )),
            project: Some(raw_layer(
                r#"
version = 1

[ports.auto]
enabled = false
"#,
            )),
            ..ConfigMergeInput::default()
        });

        assert!(!config.ports.auto.enabled);
        assert_eq!(config.ports.auto.ignore, vec![22]);
    }

    #[test]
    fn open_browser_on_auto_forward_resolves_to_notify() {
        let config = resolve_config(ConfigMergeInput {
            project: Some(raw_layer(
                r#"
version = 1

[ports.auto]
on_auto_forward = "openBrowser"
"#,
            )),
            ..ConfigMergeInput::default()
        });

        assert_eq!(config.ports.auto.on_auto_forward, OnAutoForward::Notify);
    }
}
