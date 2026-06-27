use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct ExistingContainerReusePolicy {
    pub(super) pull: bool,
    pub(super) service_forward_requires_recreate: bool,
}

pub(super) fn should_reuse_existing_container(
    decision: &ExistingContainerDecision,
    policy: ExistingContainerReusePolicy,
) -> bool {
    matches!(
        decision,
        ExistingContainerDecision::ReuseRunning { .. }
            | ExistingContainerDecision::StartStopped { .. }
    ) && !policy.pull
        && !policy.service_forward_requires_recreate
}

pub(super) async fn start_stopped_existing_container(
    client: &DockerClient,
    workspace: &Workspace,
    plan: &UpPlan,
    container_id: String,
    container_name: String,
) -> Result<(UpOutcome, WorkspaceState)> {
    let container = state_container_snapshot(plan, container_id.clone());
    let existing_state = reusable_lifecycle_state(workspace, &container)?;

    materialize_dotfile_skeletons(&plan.dotfile_skeletons)?;
    start_container_and_verify_running(
        client,
        &container_name,
        startup_verification_for_plan(plan),
    )
    .await?;

    let state = write_reused_started_state(
        workspace,
        container,
        state_compose_project_name(plan),
        existing_state,
        true,
    )?;
    Ok((
        UpOutcome {
            container_id,
            container_name,
            reused: true,
        },
        state,
    ))
}

pub(super) async fn compose_service_forward_requires_recreate(
    client: &DockerClient,
    workspace_id: &str,
    project_name: &str,
    service_forward: &[ServiceForwardRuntime],
) -> Result<bool> {
    for runtime in service_forward {
        let containers = list_compose_forwarding_service_containers(
            client,
            workspace_id,
            project_name,
            runtime.service(),
        )
        .await?;
        let Some(container) = containers.first() else {
            continue;
        };
        if compose_service_forward_container_requires_recreate(container, runtime.mount()) {
            return Ok(true);
        }
    }

    Ok(false)
}

fn compose_service_forward_container_requires_recreate(
    container: &UpContainerSummary,
    required: &DockerMountSpec,
) -> bool {
    !container_has_mount(container, required)
}

fn container_has_mount(container: &UpContainerSummary, required: &DockerMountSpec) -> bool {
    let Some(existing_mounts) = &container.mounts else {
        return false;
    };
    existing_mounts.iter().any(|existing| {
        existing.source == required.source
            && existing.target == required.target
            && existing.mount_type == required.mount_type
            && existing.read_only == required.read_only
    })
}

pub(super) async fn recreate_existing_containers(
    client: &DockerClient,
    containers: &[UpContainerSummary],
) -> Result<()> {
    for container in containers {
        stop_container(client, &container.id, REBUILD_STOP_TIMEOUT_SECONDS).await?;
        remove_container(client, &container.id, true, false).await?;
        ui::done(&format!(
            "Removed existing dev container for rebuild: {}",
            container.name
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{mount_policy, reusable_container};
    use super::*;
    use crate::{
        config::types::MountType,
        up::{ExistingContainerDecision, UpMountSummary, decide_existing_container},
    };

    #[test]
    fn compose_service_forward_requires_recreate_when_runtime_mount_is_missing() {
        let required = DockerMountSpec {
            source: Some("/tmp/decune-runtime/forward/db".to_owned()),
            target: "/run/decune".to_owned(),
            mount_type: MountType::Bind,
            read_only: false,
            consistency: None,
            bind_options: None,
            volume_options: None,
        };
        let missing = UpContainerSummary {
            id: "db-id".to_owned(),
            name: "project-db-1".to_owned(),
            image_id: None,
            config_hash: None,
            config_file: None,
            mounts: None,
            running: true,
        };
        let present = UpContainerSummary {
            mounts: Some(vec![UpMountSummary {
                source: Some("/tmp/decune-runtime/forward/db".to_owned()),
                target: "/run/decune".to_owned(),
                mount_type: MountType::Bind,
                read_only: false,
            }]),
            ..missing.clone()
        };

        assert!(compose_service_forward_container_requires_recreate(
            &missing, &required
        ));
        assert!(!compose_service_forward_container_requires_recreate(
            &present, &required
        ));
    }

    #[test]
    fn compose_reuse_policy_allows_running_container_without_pull() {
        let container = reusable_container("stable-hash");
        let decision = decide_existing_container(
            std::slice::from_ref(&container),
            "stable-hash",
            &mount_policy(&[]),
            false,
        )
        .unwrap();

        assert_eq!(
            decision,
            ExistingContainerDecision::ReuseRunning {
                id: "container-id".to_owned(),
                name: "project-app-1".to_owned()
            }
        );
        assert!(should_reuse_existing_container(
            &decision,
            ExistingContainerReusePolicy::default()
        ));
    }

    #[test]
    fn compose_reuse_policy_blocks_running_container_when_pull_is_requested() {
        let container = reusable_container("stable-hash");
        let decision = decide_existing_container(
            std::slice::from_ref(&container),
            "stable-hash",
            &mount_policy(&[]),
            false,
        )
        .unwrap();

        assert!(!should_reuse_existing_container(
            &decision,
            ExistingContainerReusePolicy {
                pull: true,
                service_forward_requires_recreate: false,
            }
        ));
    }

    #[test]
    fn compose_reuse_policy_blocks_rebuild_decision() {
        let container = reusable_container("stable-hash");
        let decision = decide_existing_container(
            std::slice::from_ref(&container),
            "stable-hash",
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
        assert!(!should_reuse_existing_container(
            &decision,
            ExistingContainerReusePolicy::default()
        ));
    }

    #[test]
    fn compose_reuse_policy_rejects_changed_config_hash() {
        let container = reusable_container("old-hash");
        let error = decide_existing_container(
            std::slice::from_ref(&container),
            "stable-hash",
            &mount_policy(&[]),
            false,
        )
        .unwrap_err();

        assert!(error.to_string().contains("Run decune rebuild"));
    }

    #[test]
    fn compose_reuse_policy_blocks_credential_mount_recreate() {
        let container = UpContainerSummary {
            mounts: Some(Vec::new()),
            ..reusable_container("stable-hash")
        };
        let required_mount = UpMountSummary {
            source: Some("/tmp/decune/gh".to_owned()),
            target: "/run/decune/gh".to_owned(),
            mount_type: MountType::Bind,
            read_only: true,
        };
        let decision = decide_existing_container(
            std::slice::from_ref(&container),
            "stable-hash",
            &mount_policy(&[required_mount]),
            false,
        )
        .unwrap();

        assert_eq!(
            decision,
            ExistingContainerDecision::Recreate {
                containers: vec![container]
            }
        );
        assert!(!should_reuse_existing_container(
            &decision,
            ExistingContainerReusePolicy::default()
        ));
    }
}
