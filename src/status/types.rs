use std::path::PathBuf;

use serde::Serialize;

pub(crate) struct StatusOptions {
    pub(crate) workspace: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct StatusInventory {
    pub(crate) workspaces: Vec<WorkspaceStatus>,
    pub(crate) issues: Vec<StatusIssue>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct WorkspaceStatus {
    pub(crate) workspace_id: String,
    pub(crate) workspace_path: Option<String>,
    pub(crate) mode: WorkspaceMode,
    pub(crate) config_file: Option<String>,
    pub(crate) created_at: Option<String>,
    pub(crate) last_started_at: Option<String>,
    pub(crate) last_used_at: Option<String>,
    pub(crate) containers: Vec<ContainerStatusSummary>,
    pub(crate) volumes: Vec<VolumeStatusSummary>,
    pub(crate) environment_status: EnvironmentStatus,
    pub(crate) config_status: ConfigStatus,
    pub(crate) health_status: HealthStatus,
    pub(crate) lifecycle_status: LifecycleStatus,
    pub(crate) lifecycle: Option<crate::state::LifecycleState>,
    pub(crate) issues: Vec<StatusIssue>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum WorkspaceMode {
    Image,
    Dockerfile,
    Compose,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ContainerStatusSummary {
    pub(crate) id: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) service: Option<String>,
    pub(crate) run_state: RuntimeRunState,
    pub(crate) health_status: HealthStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct VolumeStatusSummary {
    pub(crate) name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum RuntimeRunState {
    Running,
    Stopped,
    Unknown,
}

impl WorkspaceMode {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Dockerfile => "dockerfile",
            Self::Compose => "compose",
            Self::Unknown => "unknown",
        }
    }
}

impl EnvironmentStatus {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Partial => "partial",
            Self::Missing => "missing",
            Self::NotCreated => "not-created",
            Self::Unknown => "unknown",
        }
    }
}

impl ConfigStatus {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::NeedsRebuild => "needs-rebuild",
            Self::Missing => "missing",
            Self::Unreadable => "unreadable",
            Self::Unknown => "unknown",
        }
    }
}

impl HealthStatus {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Unhealthy => "unhealthy",
            Self::Starting => "starting",
            Self::None => "none",
            Self::Mixed => "mixed",
            Self::Unknown => "unknown",
        }
    }
}

impl RuntimeRunState {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Unknown => "unknown",
        }
    }
}

impl StatusIssueSeverity {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum EnvironmentStatus {
    Running,
    Stopped,
    Partial,
    Missing,
    NotCreated,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ConfigStatus {
    Current,
    NeedsRebuild,
    Missing,
    Unreadable,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum HealthStatus {
    Healthy,
    Unhealthy,
    Starting,
    None,
    Mixed,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum LifecycleStatus {
    Complete,
    Incomplete,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct StatusIssue {
    pub(crate) code: &'static str,
    pub(crate) severity: StatusIssueSeverity,
    pub(crate) message: String,
    pub(crate) action: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum StatusIssueSeverity {
    Info,
    Warning,
    Error,
}
