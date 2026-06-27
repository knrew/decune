use crate::{
    docker::mounts::normalize_container_path,
    host::credentials::{
        DECUNE_RUNTIME_TARGET, GITHUB_CLI_CONFIG_TARGET, GITHUB_CLI_LEGACY_TOKEN_DIR_TARGET,
        GITHUB_CLI_TOKEN_TARGET, SSH_AGENT_SOCKET_TARGET,
    },
};
use anyhow::{Result, bail};

#[cfg(test)]
use crate::{config::types::MountType, docker::container::ContainerInspect};

use super::{ExistingContainerDecision, UpContainerSummary, UpMountSummary};

const DECUNE_MANAGED_RUNTIME_MOUNT_TARGETS: &[&str] = &[
    DECUNE_RUNTIME_TARGET,
    SSH_AGENT_SOCKET_TARGET,
    GITHUB_CLI_LEGACY_TOKEN_DIR_TARGET,
    GITHUB_CLI_TOKEN_TARGET,
    GITHUB_CLI_CONFIG_TARGET,
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CredentialRuntimeMountPolicy {
    required_mounts: Vec<UpMountSummary>,
    managed_targets: Vec<String>,
}

impl CredentialRuntimeMountPolicy {
    pub(crate) fn new(required_mounts: Vec<UpMountSummary>) -> Self {
        Self {
            required_mounts,
            managed_targets: DECUNE_MANAGED_RUNTIME_MOUNT_TARGETS
                .iter()
                .map(|target| (*target).to_owned())
                .collect(),
        }
    }

    pub(crate) fn required_mounts(&self) -> &[UpMountSummary] {
        &self.required_mounts
    }

    fn required_mount_for_existing(&self, existing: &UpMountSummary) -> bool {
        self.required_mounts
            .iter()
            .any(|required| mount_matches_required(existing, required))
    }

    fn is_managed_target(&self, target: &str) -> bool {
        let target = normalize_container_path(target);
        self.managed_targets
            .iter()
            .any(|managed| target == normalize_container_path(managed))
    }
}

pub(crate) fn decide_existing_container(
    containers: &[UpContainerSummary],
    expected_config_hash: &str,
    mount_policy: &CredentialRuntimeMountPolicy,
    rebuild: bool,
) -> Result<ExistingContainerDecision> {
    if rebuild {
        return if containers.is_empty() {
            Ok(ExistingContainerDecision::Create)
        } else {
            Ok(ExistingContainerDecision::Recreate {
                containers: containers.to_vec(),
            })
        };
    }

    let Some(container) = containers.first() else {
        return Ok(ExistingContainerDecision::Create);
    };

    if container.config_hash.as_deref() != Some(expected_config_hash) {
        bail!("Dev container configuration changed. Run decune rebuild to recreate it.");
    }

    if !container_matches_credential_mount_policy(container, mount_policy) {
        return Ok(ExistingContainerDecision::Recreate {
            containers: containers.to_vec(),
        });
    }

    if container.running {
        Ok(ExistingContainerDecision::ReuseRunning {
            id: container.id.clone(),
            name: container.name.clone(),
        })
    } else {
        Ok(ExistingContainerDecision::StartStopped {
            id: container.id.clone(),
            name: container.name.clone(),
        })
    }
}

fn container_matches_credential_mount_policy(
    container: &UpContainerSummary,
    mount_policy: &CredentialRuntimeMountPolicy,
) -> bool {
    container_has_required_mounts(container, mount_policy.required_mounts())
        && !container_has_stale_managed_mount(container, mount_policy)
}

fn container_has_required_mounts(
    container: &UpContainerSummary,
    required_mounts: &[UpMountSummary],
) -> bool {
    if required_mounts.is_empty() {
        return true;
    }

    let Some(existing_mounts) = &container.mounts else {
        return false;
    };
    required_mounts.iter().all(|required| {
        existing_mounts
            .iter()
            .any(|mount| mount_matches_required(mount, required))
    })
}

fn container_has_stale_managed_mount(
    container: &UpContainerSummary,
    mount_policy: &CredentialRuntimeMountPolicy,
) -> bool {
    let Some(existing_mounts) = &container.mounts else {
        return false;
    };

    existing_mounts.iter().any(|mount| {
        mount_policy.is_managed_target(&mount.target)
            && !mount_policy.required_mount_for_existing(mount)
    })
}

fn mount_matches_required(existing: &UpMountSummary, required: &UpMountSummary) -> bool {
    if normalize_container_path(&existing.target) != normalize_container_path(&required.target) {
        return false;
    }
    if existing.mount_type != required.mount_type {
        return false;
    }
    if existing.read_only != required.read_only {
        return false;
    }

    match required.source.as_deref() {
        Some(required_source) => existing.source.as_deref() == Some(required_source),
        None => true,
    }
}

#[cfg(test)]
pub(crate) fn container_summary(container: ContainerInspect) -> Option<UpContainerSummary> {
    let id = container.id?;
    let name = container
        .name
        .map(|name| name.trim_start_matches('/').to_owned())
        .unwrap_or_else(|| id.clone());
    let config_hash = None;
    let config_file = container
        .config
        .as_ref()
        .and_then(|config| config.labels.as_ref())
        .and_then(|labels| labels.get("devcontainer.config_file"))
        .cloned();
    let mounts = container.mounts.map(|mounts| {
        mounts
            .into_iter()
            .filter_map(|mount| {
                let read_only = !mount.rw.unwrap_or(true);
                let mount_type = mount_type_from_summary(mount.typ.as_deref())?;
                mount.destination.map(|target| UpMountSummary {
                    source: mount.source,
                    target,
                    mount_type,
                    read_only,
                })
            })
            .collect()
    });
    let running = container
        .state
        .as_ref()
        .and_then(|state| state.running)
        .unwrap_or(false);

    Some(UpContainerSummary {
        id,
        name,
        image_id: container.image,
        config_hash,
        config_file,
        mounts,
        running,
    })
}

#[cfg(test)]
fn mount_type_from_summary(value: Option<&str>) -> Option<MountType> {
    match value {
        Some("bind") => Some(MountType::Bind),
        Some("volume") => Some(MountType::Volume),
        Some("tmpfs") => Some(MountType::Tmpfs),
        _ => None,
    }
}

pub(crate) fn existing_container_image_id(container: &UpContainerSummary) -> Option<&str> {
    container
        .image_id
        .as_deref()
        .filter(|image_id| !image_id.trim().is_empty())
}

pub(crate) fn existing_container_config_hash(container: &UpContainerSummary) -> Option<&str> {
    container
        .config_hash
        .as_deref()
        .filter(|config_hash| !config_hash.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::types::MountType,
        docker::container::{ContainerMount, ContainerState},
        up::test_support::{mount_summary_with_type, mount_summary_with_type_and_read_only},
        up::{ExistingContainerDecision, UpContainerSummary, UpMountSummary},
    };

    #[test]
    fn existing_container_decision_creates_when_no_container_exists() {
        let decision =
            decide_existing_container(&[], "hash123", &mount_policy(&[]), false).unwrap();

        assert_eq!(decision, ExistingContainerDecision::Create);
    }

    #[test]
    fn existing_container_decision_reuses_running_container_with_matching_hash() {
        let container = UpContainerSummary {
            id: "container-id".to_owned(),
            name: "decune-project-abc123".to_owned(),
            image_id: None,
            config_hash: Some("hash123".to_owned()),
            config_file: None,
            mounts: Some(Vec::new()),
            running: true,
        };

        let decision =
            decide_existing_container(&[container], "hash123", &mount_policy(&[]), false).unwrap();

        assert_eq!(
            decision,
            ExistingContainerDecision::ReuseRunning {
                id: "container-id".to_owned(),
                name: "decune-project-abc123".to_owned(),
            }
        );
    }

    #[test]
    fn existing_container_decision_recreates_when_required_mount_missing() {
        let container = UpContainerSummary {
            id: "container-id".to_owned(),
            name: "decune-project-abc123".to_owned(),
            image_id: None,
            config_hash: Some("hash123".to_owned()),
            config_file: None,
            mounts: Some(vec![mount_summary(None, "/workspaces/project")]),
            running: true,
        };

        let decision = decide_existing_container(
            std::slice::from_ref(&container),
            "hash123",
            &mount_policy(&[mount_summary(
                Some("/tmp/socket"),
                "/run/decune/ssh-agent.sock",
            )]),
            false,
        )
        .unwrap();

        assert_eq!(
            decision,
            ExistingContainerDecision::Recreate {
                containers: vec![container],
            }
        );
    }

    fn mount_policy(required_mounts: &[UpMountSummary]) -> CredentialRuntimeMountPolicy {
        CredentialRuntimeMountPolicy::new(required_mounts.to_vec())
    }

    fn mount_summary(source: Option<&str>, target: &str) -> UpMountSummary {
        UpMountSummary {
            source: source.map(ToOwned::to_owned),
            target: target.to_owned(),
            mount_type: MountType::Bind,
            read_only: false,
        }
    }
    #[test]
    fn existing_container_decision_starts_stopped_container_with_matching_hash() {
        let container = UpContainerSummary {
            id: "container-id".to_owned(),
            name: "decune-project-abc123".to_owned(),
            image_id: None,
            config_hash: Some("hash123".to_owned()),
            config_file: None,
            mounts: Some(Vec::new()),
            running: false,
        };

        let decision =
            decide_existing_container(&[container], "hash123", &mount_policy(&[]), false).unwrap();

        assert_eq!(
            decision,
            ExistingContainerDecision::StartStopped {
                id: "container-id".to_owned(),
                name: "decune-project-abc123".to_owned()
            }
        );
    }
    #[test]
    fn existing_container_decision_recreates_when_required_mount_source_changed() {
        let container = UpContainerSummary {
            id: "container-id".to_owned(),
            name: "decune-project-abc123".to_owned(),
            image_id: None,
            config_hash: Some("hash123".to_owned()),
            config_file: None,
            mounts: Some(vec![mount_summary(
                Some("/tmp/agent-a.sock"),
                SSH_AGENT_SOCKET_TARGET,
            )]),
            running: true,
        };

        let decision = decide_existing_container(
            std::slice::from_ref(&container),
            "hash123",
            &mount_policy(&[mount_summary(
                Some("/tmp/agent-b.sock"),
                SSH_AGENT_SOCKET_TARGET,
            )]),
            false,
        )
        .unwrap();

        assert_eq!(
            decision,
            ExistingContainerDecision::Recreate {
                containers: vec![container]
            }
        );
    }
    #[test]
    fn existing_container_decision_recreates_when_required_mount_type_changed_for_github_cli_tmpfs()
    {
        let container = UpContainerSummary {
            id: "container-id".to_owned(),
            name: "decune-project-abc123".to_owned(),
            image_id: None,
            config_hash: Some("hash123".to_owned()),
            config_file: None,
            mounts: Some(vec![mount_summary_with_type(
                Some("/tmp/gh-config"),
                GITHUB_CLI_CONFIG_TARGET,
                MountType::Bind,
            )]),
            running: true,
        };

        let decision = decide_existing_container(
            std::slice::from_ref(&container),
            "hash123",
            &mount_policy(&[mount_summary_with_type(
                None,
                GITHUB_CLI_CONFIG_TARGET,
                MountType::Tmpfs,
            )]),
            false,
        )
        .unwrap();

        assert_eq!(
            decision,
            ExistingContainerDecision::Recreate {
                containers: vec![container]
            }
        );
    }
    #[test]
    fn existing_container_decision_reuses_when_required_tmpfs_mount_is_present() {
        let container = UpContainerSummary {
            id: "container-id".to_owned(),
            name: "decune-project-abc123".to_owned(),
            image_id: None,
            config_hash: Some("hash123".to_owned()),
            config_file: None,
            mounts: Some(vec![mount_summary_with_type(
                None,
                GITHUB_CLI_CONFIG_TARGET,
                MountType::Tmpfs,
            )]),
            running: true,
        };

        let decision = decide_existing_container(
            std::slice::from_ref(&container),
            "hash123",
            &mount_policy(&[mount_summary_with_type(
                None,
                GITHUB_CLI_CONFIG_TARGET,
                MountType::Tmpfs,
            )]),
            false,
        )
        .unwrap();

        assert_eq!(
            decision,
            ExistingContainerDecision::ReuseRunning {
                id: "container-id".to_owned(),
                name: "decune-project-abc123".to_owned()
            }
        );
    }
    #[test]
    fn existing_container_decision_recreates_when_required_mount_read_only_changed() {
        let container = UpContainerSummary {
            id: "container-id".to_owned(),
            name: "decune-project-abc123".to_owned(),
            image_id: None,
            config_hash: Some("hash123".to_owned()),
            config_file: None,
            mounts: Some(vec![mount_summary_with_type_and_read_only(
                Some("/tmp/secrets/github-token"),
                GITHUB_CLI_TOKEN_TARGET,
                MountType::Bind,
                false,
            )]),
            running: true,
        };

        let decision = decide_existing_container(
            std::slice::from_ref(&container),
            "hash123",
            &mount_policy(&[mount_summary_with_type_and_read_only(
                Some("/tmp/secrets/github-token"),
                GITHUB_CLI_TOKEN_TARGET,
                MountType::Bind,
                true,
            )]),
            false,
        )
        .unwrap();

        assert_eq!(
            decision,
            ExistingContainerDecision::Recreate {
                containers: vec![container]
            }
        );
    }
    #[test]
    fn existing_container_decision_reuses_running_container_when_github_token_file_mount_matches() {
        let container = UpContainerSummary {
            id: "container-id".to_owned(),
            name: "decune-project-abc123".to_owned(),
            image_id: None,
            config_hash: Some("hash123".to_owned()),
            config_file: None,
            mounts: Some(vec![mount_summary_with_type_and_read_only(
                Some("/tmp/secrets/github-token"),
                GITHUB_CLI_TOKEN_TARGET,
                MountType::Bind,
                true,
            )]),
            running: true,
        };

        let decision = decide_existing_container(
            std::slice::from_ref(&container),
            "hash123",
            &mount_policy(&[mount_summary_with_type_and_read_only(
                Some("/tmp/secrets/github-token"),
                GITHUB_CLI_TOKEN_TARGET,
                MountType::Bind,
                true,
            )]),
            false,
        )
        .unwrap();

        assert_eq!(
            decision,
            ExistingContainerDecision::ReuseRunning {
                id: "container-id".to_owned(),
                name: "decune-project-abc123".to_owned()
            }
        );
    }
    #[test]
    fn existing_container_decision_recreates_when_ssh_agent_mount_is_stale() {
        let container = UpContainerSummary {
            id: "container-id".to_owned(),
            name: "decune-project-abc123".to_owned(),
            image_id: None,
            config_hash: Some("hash123".to_owned()),
            config_file: None,
            mounts: Some(vec![mount_summary(
                Some("/tmp/agent-a.sock"),
                SSH_AGENT_SOCKET_TARGET,
            )]),
            running: true,
        };

        let decision = decide_existing_container(
            std::slice::from_ref(&container),
            "hash123",
            &mount_policy(&[]),
            false,
        )
        .unwrap();

        assert_eq!(
            decision,
            ExistingContainerDecision::Recreate {
                containers: vec![container]
            }
        );
    }
    #[test]
    fn existing_container_decision_recreates_when_github_cli_mounts_are_stale() {
        let container = UpContainerSummary {
            id: "container-id".to_owned(),
            name: "decune-project-abc123".to_owned(),
            image_id: None,
            config_hash: Some("hash123".to_owned()),
            config_file: None,
            mounts: Some(vec![
                mount_summary_with_type_and_read_only(
                    Some("/tmp/gh-token"),
                    GITHUB_CLI_LEGACY_TOKEN_DIR_TARGET,
                    MountType::Bind,
                    true,
                ),
                mount_summary_with_type(None, GITHUB_CLI_CONFIG_TARGET, MountType::Tmpfs),
            ]),
            running: true,
        };

        let decision = decide_existing_container(
            std::slice::from_ref(&container),
            "hash123",
            &mount_policy(&[]),
            false,
        )
        .unwrap();

        assert_eq!(
            decision,
            ExistingContainerDecision::Recreate {
                containers: vec![container]
            }
        );
    }
    #[test]
    fn container_summary_restores_read_only_from_docker_mount_rw() {
        let summary = container_summary(ContainerInspect {
            id: Some("container-id".to_owned()),
            name: Some("/decune-project-abc123".to_owned()),
            state: Some(ContainerState {
                running: Some(true),
                exit_code: None,
                pid: None,
                ..ContainerState::default()
            }),
            mounts: Some(vec![
                ContainerMount {
                    typ: Some("bind".to_owned()),
                    source: Some("/tmp/secrets/github-token".to_owned()),
                    destination: Some(GITHUB_CLI_TOKEN_TARGET.to_owned()),
                    rw: Some(false),
                },
                ContainerMount {
                    typ: Some("bind".to_owned()),
                    source: Some("/tmp/agent.sock".to_owned()),
                    destination: Some(SSH_AGENT_SOCKET_TARGET.to_owned()),
                    rw: Some(true),
                },
            ]),
            ..ContainerInspect::default()
        })
        .unwrap();

        let mounts = summary.mounts.unwrap();
        assert!(mounts[0].read_only);
        assert!(!mounts[1].read_only);
    }
    #[test]
    fn existing_container_decision_rejects_changed_config_hash() {
        let container = UpContainerSummary {
            id: "container-id".to_owned(),
            name: "decune-project-abc123".to_owned(),
            image_id: None,
            config_hash: Some("old-hash".to_owned()),
            config_file: None,
            mounts: Some(Vec::new()),
            running: true,
        };

        let error = decide_existing_container(&[container], "new-hash", &mount_policy(&[]), false)
            .unwrap_err();

        assert!(error.to_string().contains("Run decune rebuild"));
    }
    #[test]
    fn existing_container_decision_recreates_when_rebuild_is_requested() {
        let container = UpContainerSummary {
            id: "container-id".to_owned(),
            name: "decune-project-abc123".to_owned(),
            image_id: None,
            config_hash: Some("old-hash".to_owned()),
            config_file: None,
            mounts: Some(Vec::new()),
            running: true,
        };

        let decision = decide_existing_container(
            std::slice::from_ref(&container),
            "new-hash",
            &mount_policy(&[]),
            true,
        )
        .unwrap();

        assert_eq!(
            decision,
            ExistingContainerDecision::Recreate {
                containers: vec![container]
            }
        );
    }
}
