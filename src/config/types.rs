use crate::config::schema::{
    RawCommand, RawDotfileConflict, RawGitHttpsMode, RawGithubCredentialsMode, RawHookLocation,
    RawMountCreate, RawMountType, RawOnAutoForward, RawPortProtocol, RawSshAgentMode,
};

pub(crate) const DEFAULT_PORT_HOST_IP: &str = "127.0.0.1";
pub(crate) const DEFAULT_AUTO_PORT_MIN: u16 = 1024;
pub(crate) const DEFAULT_AUTO_PORT_MAX: u16 = 32768;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum PortProtocol {
    Tcp,
    // Kept for future UDP support; input parsers reject UDP explicitly.
    #[allow(dead_code)]
    Udp,
}

impl From<RawPortProtocol> for PortProtocol {
    fn from(value: RawPortProtocol) -> Self {
        match value {
            RawPortProtocol::Tcp => Self::Tcp,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GitHttpsMode {
    Off,
    HostHelper,
    HostHelperReadOnly,
}

impl From<RawGitHttpsMode> for GitHttpsMode {
    fn from(value: RawGitHttpsMode) -> Self {
        match value {
            RawGitHttpsMode::Off => Self::Off,
            RawGitHttpsMode::HostHelper => Self::HostHelper,
            RawGitHttpsMode::HostHelperReadOnly => Self::HostHelperReadOnly,
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
