use std::{collections::BTreeMap, path::Path};

use crate::{
    config::{resolved::ResolvedConfig, variables::SensitiveEnvMap},
    docker::{client::DockerClient, user::ResolvedRemoteUser},
};

#[derive(Clone)]
pub(crate) struct LifecycleRunContext<'a> {
    pub(crate) client: &'a DockerClient,
    pub(crate) container: String,
    pub(crate) config: &'a ResolvedConfig,
    pub(crate) sensitive_container_env: &'a SensitiveEnvMap,
    pub(crate) workspace_root: &'a Path,
    pub(crate) workspace_basename: &'a str,
    pub(crate) workspace_id: &'a str,
    pub(crate) workspace_folder: &'a str,
    pub(crate) runtime_dir: &'a Path,
    pub(crate) remote_user: ResolvedRemoteUser,
}

pub(crate) struct PreparedLifecycleRunContext<'a> {
    pub(in crate::devcontainer::lifecycle) client: &'a DockerClient,
    pub(in crate::devcontainer::lifecycle) container: String,
    pub(in crate::devcontainer::lifecycle) config: &'a ResolvedConfig,
    pub(in crate::devcontainer::lifecycle) workspace_root: &'a Path,
    pub(in crate::devcontainer::lifecycle) workspace_folder: &'a str,
    pub(in crate::devcontainer::lifecycle) remote_user: ResolvedRemoteUser,
    pub(in crate::devcontainer::lifecycle) remote_env: BTreeMap<String, String>,
    pub(in crate::devcontainer::lifecycle) remote_process_env: BTreeMap<String, String>,
    pub(in crate::devcontainer::lifecycle) lifecycle_redactions: Vec<String>,
}

impl PreparedLifecycleRunContext<'_> {
    /// Container that the lifecycle commands run in: the container name in image and
    /// Dockerfile modes, and the primary service container ID in Compose mode.
    pub(crate) fn container(&self) -> &str {
        &self.container
    }

    pub(crate) const fn remote_user(&self) -> &ResolvedRemoteUser {
        &self.remote_user
    }
}
