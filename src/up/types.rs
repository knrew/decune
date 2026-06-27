use std::path::PathBuf;

use std::collections::BTreeMap;

use crate::{
    config::{
        ConfigLayer, ConfigMergeInput, resolved::ResolvedConfig, types::MountType,
        variables::SensitiveEnvMap,
    },
    devcontainer::features::PreparedFeatureInstallPlan,
    docker::{
        build::{DockerBuildOptions, ResolvedBuildContext},
        dotfiles::DotfileSkeletonPlan,
        mounts::DockerMountSpec,
        ports::ResolvedForwardPort,
        resource::DockerResources,
        user::{EffectiveUsers, UidGidSyncPlan},
    },
    runtime::compose_cli::ComposeProjectPlan,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MountResolution {
    Resolve,
    ReadOnly,
    DeferConfigMounts,
}

impl MountResolution {
    pub(crate) const fn resolves_config_mounts(self) -> bool {
        matches!(self, Self::Resolve | Self::ReadOnly)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ForwardingResolution {
    Resolve,
    IgnoreDetached,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StartupVerification {
    Keepalive,
    OriginalCommand,
    FeatureEntrypoints { monitor_delegated_command: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UpPlanResolution {
    pub(crate) forwarding: ForwardingResolution,
    pub(crate) update_features: bool,
    pub(crate) skip_global_config: bool,
}

impl UpPlanResolution {
    pub(crate) const fn new(
        forwarding: ForwardingResolution,
        update_features: bool,
        skip_global_config: bool,
    ) -> Self {
        Self {
            forwarding,
            update_features,
            skip_global_config,
        }
    }
}

pub(crate) struct WorkspaceLocation {
    pub(crate) workspace_folder: String,
    pub(crate) workspace_mount: DockerMountSpec,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UpMountSummary {
    pub(crate) source: Option<String>,
    pub(crate) target: String,
    pub(crate) mount_type: MountType,
    pub(crate) read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UpContainerSummary {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) image_id: Option<String>,
    pub(crate) config_hash: Option<String>,
    pub(crate) config_file: Option<String>,
    pub(crate) mounts: Option<Vec<UpMountSummary>>,
    pub(crate) running: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExistingContainerDecision {
    Create,
    Recreate { containers: Vec<UpContainerSummary> },
    ReuseRunning { id: String, name: String },
    StartStopped { id: String, name: String },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct UpPlan {
    pub(crate) image: String,
    pub(crate) base_image: String,
    pub(crate) build_context: Option<ResolvedBuildContext>,
    pub(crate) build_options: DockerBuildOptions,
    pub(crate) feature_install: Option<PreparedFeatureInstallPlan>,
    pub(crate) feature_build_context_dir: Option<PathBuf>,
    pub(crate) uid_gid_sync_build_context_dir: Option<PathBuf>,
    pub(crate) resources: DockerResources,
    pub(crate) pre_uid_gid_sync_resources: Option<DockerResources>,
    pub(crate) compose_project: Option<ComposeProjectPlan>,
    pub(crate) config_layers: ConfigMergeInput,
    pub(crate) config: ResolvedConfig,
    pub(crate) sensitive_container_env: SensitiveEnvMap,
    pub(crate) sensitive_build_args: SensitiveEnvMap,
    pub(crate) compose_interpolation_env: BTreeMap<String, String>,
    pub(crate) compose_interpolation_redactions: Vec<String>,
    pub(crate) effective_users: EffectiveUsers,
    pub(crate) uid_gid_sync_plan: UidGidSyncPlan,
    pub(crate) workspace_folder: String,
    pub(crate) mounts: Vec<DockerMountSpec>,
    pub(crate) dotfile_skeletons: Vec<DotfileSkeletonPlan>,
    pub(crate) forward_ports: Vec<ResolvedForwardPort>,
    pub(crate) ignored_detached_forwarding: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct UpOptions {
    pub(crate) workspace: PathBuf,
    pub(crate) config_path: Option<PathBuf>,
    pub(crate) cli_layer: ConfigLayer,
    pub(crate) config: UpConfigOptions,
    pub(crate) build: UpBuildOptions,
    pub(crate) reuse: UpReuseOptions,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct UpConfigOptions {
    pub(crate) skip_global_config: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct UpBuildOptions {
    pub(crate) pull: bool,
    pub(crate) no_cache: bool,
    pub(crate) update_features: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct UpReuseOptions {
    pub(crate) rebuild: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UpOutcome {
    pub(crate) container_id: String,
    pub(crate) container_name: String,
    pub(crate) reused: bool,
}
