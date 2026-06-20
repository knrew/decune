use anyhow::Result;

mod attach;
mod build;
mod exec_target;
mod existing;
mod forwarding;
mod lifecycle;
mod metadata;
mod mounts;
mod plan;
mod shell;
mod start;
mod types;
mod uid_gid;

use crate::{
    config::resolved::{ResolvedDevcontainerSource, ResolvedShutdownAction},
    docker::container::stop_container,
    runtime::compose_cli::{ComposeLifecyclePlan, ComposeStopOptions, DockerComposeCli},
};
use attach::attach_shell;
use forwarding::{start_forwarding_for_up, stop_forwarding, warn_about_detached_forwarding};
use lifecycle::{
    prepare_up_lifecycle, report_up_success, run_attach_lifecycle_for_up,
    run_container_start_lifecycle_for_up, start_host_daemon_for_up,
};
use start::ensure_container_started;

#[cfg(test)]
use crate::host::credentials::DECUNE_RUNTIME_TARGET;
#[cfg(test)]
use build::feature_layer_image;
#[cfg(test)]
use existing::{CredentialRuntimeMountPolicy, container_summary, decide_existing_container};
#[cfg(test)]
use forwarding::{
    ForwardAgentStartDecision, decide_forward_agent_start, plan_forwarding_agent_targets,
};
#[cfg(test)]
use metadata::{
    add_github_cli_feature_to_plan, deferred_config_warnings, finalize_up_plan_mounts,
    security_notices, should_auto_add_github_cli_feature,
};
#[cfg(test)]
use mounts::default_workspace_folder;
pub(crate) use mounts::mount_hash_inputs;
pub(in crate::up) use mounts::{
    WorkspaceLocationValidation, resolve_workspace_location, static_mount_variable_context,
    workspace_mount_plan_from_resolved,
};
#[cfg(test)]
use plan::build_preliminary_up_plan_with_forwarding_resolution;
pub(crate) use plan::build_up_plan_with_forwarding_resolution;
#[cfg(test)]
use plan::{build_up_plan, build_up_plan_with_image_metadata, build_up_plan_with_update_features};
#[cfg(test)]
use shell::{first_successful_shell_candidate, shell_command_candidates};
#[cfg(test)]
use start::{
    add_credential_runtime_mounts_with_inputs, add_credential_runtime_mounts_with_ssh_socket,
    create_and_start_container, generated_compose_override_content, list_workspace_containers,
};
pub(crate) use types::{
    ExistingContainerDecision, ForwardingResolution, MountResolution, UpContainerSummary,
    UpMountSummary, UpOptions, UpOutcome, UpPlan, UpPlanResolution, WorkspaceLocation,
};
pub(in crate::up) use uid_gid::static_uid_gid_sync_hash_input;
#[cfg(test)]
use uid_gid::{uid_gid_sync_base_image, uid_gid_sync_warning};

const SHUTDOWN_STOP_TIMEOUT_SECONDS: i32 = 10;

pub(crate) async fn run_detached_up(options: UpOptions) -> Result<UpOutcome> {
    let start_time = std::time::Instant::now();
    let started = Box::pin(ensure_container_started(
        options,
        ForwardingResolution::IgnoreDetached,
    ))
    .await?;
    warn_about_detached_forwarding(&started.plan);
    let _host_daemon = start_host_daemon_for_up(&started).await?;
    {
        let lifecycle = prepare_up_lifecycle(&started).await?;
        run_container_start_lifecycle_for_up(&started, &lifecycle).await?;
    }
    report_up_success(&started, start_time.elapsed());

    Ok(started.outcome)
}

pub(crate) async fn run_attached_up(options: UpOptions) -> Result<i32> {
    let start_time = std::time::Instant::now();
    let started = Box::pin(ensure_container_started(
        options,
        ForwardingResolution::Resolve,
    ))
    .await?;
    let _host_daemon = start_host_daemon_for_up(&started).await?;
    let lifecycle = prepare_up_lifecycle(&started).await?;
    run_container_start_lifecycle_for_up(&started, &lifecycle).await?;
    let forwarding = start_forwarding_for_up(&started).await?;
    let attach_result = async {
        run_attach_lifecycle_for_up(&lifecycle).await?;
        report_up_success(&started, start_time.elapsed());

        attach_shell(
            &started.client,
            &started.workspace,
            &started.plan,
            &started.outcome.container_name,
        )
        .await
    }
    .await;
    stop_forwarding(forwarding).await;

    let exit_code = attach_result?;
    apply_shutdown_action_after_attached_up(&started).await?;
    Ok(shell::clamp_exit_code(exit_code))
}

async fn apply_shutdown_action_after_attached_up(
    started: &start::StartedUpContainer,
) -> Result<()> {
    match started.plan.config.devcontainer.shutdown_action {
        ResolvedShutdownAction::None => Ok(()),
        ResolvedShutdownAction::StopContainer => {
            stop_primary_container_after_attached_up(started).await
        }
        ResolvedShutdownAction::StopCompose => {
            let Some(compose_project) = &started.plan.compose_project else {
                return stop_primary_container_after_attached_up(started).await;
            };
            let Some(ResolvedDevcontainerSource::Compose(_)) =
                &started.plan.config.devcontainer.source
            else {
                return stop_primary_container_after_attached_up(started).await;
            };
            let lifecycle =
                ComposeLifecyclePlan::down(compose_project.command_plan_with_generated_override());
            DockerComposeCli::default()
                .stop(
                    &lifecycle.project,
                    ComposeStopOptions {
                        timeout_seconds: None,
                    },
                    &lifecycle.services,
                )
                .await
        }
    }
}

async fn stop_primary_container_after_attached_up(
    started: &start::StartedUpContainer,
) -> Result<()> {
    stop_container(
        &started.client,
        &started.outcome.container_id,
        SHUTDOWN_STOP_TIMEOUT_SECONDS,
    )
    .await
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap, fs, net::TcpListener, ops::Deref, os::unix::net::UnixListener,
        path::PathBuf,
    };

    use anyhow::Context;

    use super::metadata::FinalizeUpPlanMountsOptions;

    use crate::config::layer::{
        LayerDevcontainerCompose, LayerDevcontainerMetadata, LayerDevcontainerSource,
        LayerUserEnvProbe,
    };
    use crate::config::resolved::{
        ResolvedConfig, ResolvedDevcontainerSource, ResolvedPortAttributes, ResolvedPublishPort,
    };
    use crate::config::types::{
        GitHttpsMode, GithubCredentialsMode, MountType, PortProtocol, SshAgentMode,
    };
    use crate::config::{ConfigHashInput, ConfigLayer, ConfigMergeInput, config_hash};
    use crate::docker::build::{
        DockerBuildInput, DockerBuildOptions, ResolvedBuildContext, build_image,
    };
    use crate::docker::client::DockerClient;
    use crate::docker::container::{
        ContainerInspect, ContainerMount, ContainerState, remove_container, stop_container,
    };
    use crate::docker::exec::{ExecCommandSpec, exec_capture};
    use crate::docker::image::{PullPolicy, ensure_image, remove_image};
    use crate::docker::mounts::{
        DockerMountSpec, MountBindOptions, MountBindPropagation, MountVolumeOptions,
    };
    use crate::docker::ports::ResolvedForwardPort;
    use crate::docker::resource::DockerResources;
    use crate::docker::user::{
        EffectiveUserResolveInput, EffectiveUsers, HostPlatform, HostUserIds, ResolvedUserIds,
        UidGidSyncNoopReason, UidGidSyncPlan, UidGidSyncTarget, UidGidSyncTargetKind,
        current_host_user_ids, resolve_effective_users,
    };
    use crate::host::credentials::{
        GITHUB_CLI_CONFIG_TARGET, GITHUB_CLI_LEGACY_TOKEN_DIR_TARGET, GITHUB_CLI_TOKEN_TARGET,
        SSH_AGENT_SOCKET_TARGET,
    };
    use crate::workspace::Workspace;

    use super::{
        CredentialRuntimeMountPolicy, DECUNE_RUNTIME_TARGET, ExistingContainerDecision,
        ForwardingResolution, UpContainerSummary, UpMountSummary, UpOptions, UpPlan,
        add_credential_runtime_mounts_with_inputs, add_credential_runtime_mounts_with_ssh_socket,
        add_github_cli_feature_to_plan, build_preliminary_up_plan_with_forwarding_resolution,
        build_up_plan, build_up_plan_with_forwarding_resolution, build_up_plan_with_image_metadata,
        build_up_plan_with_update_features, container_summary, create_and_start_container,
        decide_existing_container, default_workspace_folder, deferred_config_warnings,
        feature_layer_image, finalize_up_plan_mounts, first_successful_shell_candidate,
        generated_compose_override_content, list_workspace_containers, mount_hash_inputs,
        plan_forwarding_agent_targets, run_attached_up, run_detached_up, security_notices,
        shell_command_candidates, should_auto_add_github_cli_feature, uid_gid_sync_base_image,
        uid_gid_sync_warning,
    };

    #[test]
    fn existing_container_decision_creates_when_no_container_exists() {
        let decision =
            decide_existing_container(&[], "hash123", &mount_policy(&[]), false).unwrap();

        assert_eq!(decision, ExistingContainerDecision::Create);
    }

    #[test]
    fn shell_candidates_use_only_explicit_config_shell() {
        assert_eq!(
            shell_command_candidates(Some(" /bin/zsh "), Some("/bin/fish")),
            vec!["/bin/zsh".to_owned()]
        );
    }

    #[test]
    fn auto_only_forwarding_skips_unsupported_container_architecture() {
        assert_eq!(
            super::decide_forward_agent_start(false, true, Some("riscv64")),
            super::ForwardAgentStartDecision::SkipAutoWithWarning(
                "Automatic port forwarding is disabled because the container architecture is not supported by the port forwarding agent: riscv64".to_owned()
            )
        );
        assert_eq!(
            super::decide_forward_agent_start(true, true, Some("riscv64")),
            super::ForwardAgentStartDecision::Start
        );
        assert_eq!(
            super::decide_forward_agent_start(false, true, Some("x86_64")),
            super::ForwardAgentStartDecision::Start
        );
        assert_eq!(
            super::decide_forward_agent_start(false, true, Some("aarch64")),
            super::ForwardAgentStartDecision::Start
        );
    }

    #[test]
    fn shell_candidates_use_remote_login_shell_before_fallbacks() {
        assert_eq!(
            shell_command_candidates(None, Some("/bin/fish")),
            vec![
                "/bin/fish".to_owned(),
                "/bin/bash".to_owned(),
                "/bin/sh".to_owned()
            ]
        );
    }

    #[test]
    fn shell_candidates_fall_back_to_bash_then_sh() {
        assert_eq!(
            shell_command_candidates(None, None),
            vec!["/bin/bash".to_owned(), "/bin/sh".to_owned()]
        );
    }

    #[test]
    fn shell_candidate_fallback_tries_next_auto_candidate_after_start_failure() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let selected = runtime
            .block_on(first_successful_shell_candidate(
                vec!["/bin/bash".to_owned(), "/bin/sh".to_owned()],
                |command| async move {
                    if command == "/bin/bash" {
                        anyhow::bail!("start failed");
                    }

                    Ok::<_, anyhow::Error>(command)
                },
            ))
            .unwrap();

        assert_eq!(selected, "/bin/sh");
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
                name: "decune-project-abc123".to_owned()
            }
        );
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
            &mount_policy(&[mount_summary(None, "/run/decune")]),
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
    fn credential_runtime_mounts_add_ssh_agent_without_hashing_socket_path() {
        let temp = tempfile::tempdir().unwrap();
        let socket_path_a = temp.path().join("agent-a.sock");
        let socket_path_b = temp.path().join("agent-b.sock");
        let _listener_a = UnixListener::bind(&socket_path_a).unwrap();
        let _listener_b = UnixListener::bind(&socket_path_b).unwrap();
        let runtime_dir = temp.path().join("runtime");
        let mut config = ResolvedConfig::default();
        config.credentials.git.https = GitHttpsMode::Off;
        let plan = test_up_plan_with_config(config);

        let (plan_a, _runtime_a) = add_credential_runtime_mounts_with_ssh_socket(
            plan.clone(),
            &runtime_dir,
            Some(&socket_path_a),
        )
        .unwrap();
        let (plan_b, _runtime_b) =
            add_credential_runtime_mounts_with_ssh_socket(plan, &runtime_dir, Some(&socket_path_b))
                .unwrap();

        assert_eq!(plan_a.resources.config_hash, "stable-hash");
        assert_eq!(plan_b.resources.config_hash, "stable-hash");
        assert_eq!(
            plan_a
                .config
                .devcontainer
                .container_env
                .get("SSH_AUTH_SOCK")
                .map(String::as_str),
            Some(SSH_AGENT_SOCKET_TARGET)
        );
        assert_eq!(
            plan_a
                .mounts
                .iter()
                .find(|mount| mount.target == SSH_AGENT_SOCKET_TARGET)
                .and_then(|mount| mount.source.as_deref()),
            socket_path_a.to_str()
        );
        assert_eq!(
            plan_b
                .mounts
                .iter()
                .find(|mount| mount.target == SSH_AGENT_SOCKET_TARGET)
                .and_then(|mount| mount.source.as_deref()),
            socket_path_b.to_str()
        );
    }

    #[test]
    fn credential_runtime_mounts_add_github_token_file_without_hashing_token_or_env() {
        let temp = tempfile::tempdir().unwrap();
        let runtime_dir = temp.path().join("runtime");
        let mut config = ResolvedConfig::default();
        config.credentials.git.https = GitHttpsMode::Off;
        let plan = test_up_plan_with_config(config);

        let (plan_a, _runtime_a) = add_credential_runtime_mounts_with_inputs(
            plan.clone(),
            &runtime_dir,
            None,
            Some("first-secret\n"),
        )
        .unwrap();
        let (plan_b, _runtime_b) = add_credential_runtime_mounts_with_inputs(
            plan,
            &runtime_dir,
            None,
            Some("second-secret\n"),
        )
        .unwrap();

        assert_eq!(plan_a.resources.config_hash, "stable-hash");
        assert_eq!(plan_b.resources.config_hash, "stable-hash");
        assert!(
            plan_a
                .config
                .devcontainer
                .container_env
                .values()
                .all(|value| !value.contains("first-secret"))
        );
        assert!(
            plan_a
                .resources
                .labels
                .values()
                .all(|value| !value.contains("first-secret"))
        );
        assert!(plan_a.mounts.iter().any(|mount| {
            mount.target == GITHUB_CLI_TOKEN_TARGET
                && mount
                    .source
                    .as_deref()
                    .is_some_and(|source| source.ends_with("secrets/github-token"))
                && mount.read_only
        }));
        assert_eq!(
            plan_a
                .config
                .devcontainer
                .container_env
                .get("GH_CONFIG_DIR")
                .map(String::as_str),
            Some(GITHUB_CLI_CONFIG_TARGET)
        );
        assert!(
            plan_a
                .mounts
                .iter()
                .any(|mount| mount.target == GITHUB_CLI_CONFIG_TARGET && !mount.read_only)
        );
    }

    #[test]
    fn credential_runtime_mounts_add_forward_agent_without_hashing_ports() {
        let temp = tempfile::tempdir().unwrap();
        let runtime_dir = temp.path().join("runtime");
        let mut config = ResolvedConfig::default();
        config.credentials.git.https = GitHttpsMode::Off;
        let mut plan = test_up_plan_with_config(config);
        plan.forward_ports = vec![ResolvedForwardPort {
            service: None,
            container: 4321,
            host: 54321,
            host_ip: "127.0.0.1".to_owned(),
            protocol: PortProtocol::Tcp,
            require_local: false,
            label: None,
        }];

        let (plan, runtime) =
            add_credential_runtime_mounts_with_inputs(plan, &runtime_dir, None, None).unwrap();

        assert_eq!(plan.resources.config_hash, "stable-hash");
        assert!(runtime_dir.join("decune-forward-agent").is_file());
        assert!(plan.mounts.iter().any(|mount| {
            mount.target == DECUNE_RUNTIME_TARGET
                && mount.source.as_deref() == Some(runtime_dir.to_str().unwrap())
                && !mount.read_only
        }));
        assert!(
            runtime
                .mount_policy()
                .required_mounts()
                .iter()
                .any(|mount| mount.target == DECUNE_RUNTIME_TARGET)
        );
    }

    #[test]
    fn credential_runtime_mounts_add_forward_runtime_without_ports() {
        let temp = tempfile::tempdir().unwrap();
        let runtime_dir = temp.path().join("runtime");
        let mut config = ResolvedConfig::default();
        config.credentials.git.enabled = false;
        config.credentials.github.enabled = false;
        config.credentials.github.mode = GithubCredentialsMode::Off;
        let plan = test_up_plan_with_config(config);

        let (plan, runtime) =
            add_credential_runtime_mounts_with_inputs(plan, &runtime_dir, None, None).unwrap();

        assert_eq!(plan.resources.config_hash, "stable-hash");
        assert!(runtime_dir.join("decune-forward-agent").is_file());
        assert!(plan.mounts.iter().any(|mount| {
            mount.target == DECUNE_RUNTIME_TARGET
                && mount.source.as_deref() == Some(runtime_dir.to_str().unwrap())
                && !mount.read_only
        }));
        assert!(
            runtime
                .mount_policy()
                .required_mounts()
                .iter()
                .any(|mount| mount.target == DECUNE_RUNTIME_TARGET)
        );
    }

    #[test]
    fn compose_credentials_secret_leak_generated_override_injects_primary_runtime_mounts() {
        let temp = tempfile::tempdir().unwrap();
        let runtime_dir = temp.path().join("runtime");
        let socket_path = temp.path().join("agent.sock");
        let _listener = UnixListener::bind(&socket_path).unwrap();
        let mut config = ResolvedConfig::default();
        config
            .devcontainer
            .container_env
            .insert("APP_ENV".to_owned(), "compose-credentials-test".to_owned());
        let mut plan = test_up_plan_with_config(config);
        plan.forward_ports = vec![ResolvedForwardPort {
            service: None,
            container: 4321,
            host: 54321,
            host_ip: "127.0.0.1".to_owned(),
            protocol: PortProtocol::Tcp,
            require_local: false,
            label: None,
        }];

        let (plan, _runtime) = add_credential_runtime_mounts_with_inputs(
            plan,
            &runtime_dir,
            Some(&socket_path),
            Some("compose-github-secret\n"),
        )
        .unwrap();
        let yaml = generated_compose_override_content("app", &plan).unwrap();

        assert!(yaml.contains("  'app':\n"));
        assert!(!yaml.contains("sidecar"));
        assert!(yaml.contains(DECUNE_RUNTIME_TARGET));
        assert!(yaml.contains(SSH_AGENT_SOCKET_TARGET));
        assert!(yaml.contains(GITHUB_CLI_TOKEN_TARGET));
        assert!(yaml.contains("read_only: true"));
        assert!(yaml.contains(GITHUB_CLI_CONFIG_TARGET));
        assert!(yaml.contains("type: tmpfs"));
        assert!(yaml.contains("'SSH_AUTH_SOCK': '/run/decune/ssh-agent.sock'"));
        assert!(yaml.contains("'GH_CONFIG_DIR': '/run/decune/gh'"));
        assert!(!yaml.contains("compose-github-secret"));
    }

    #[test]
    fn compose_credentials_generated_override_honors_disabled_credentials() {
        let temp = tempfile::tempdir().unwrap();
        let runtime_dir = temp.path().join("runtime");
        let socket_path = temp.path().join("agent.sock");
        let _listener = UnixListener::bind(&socket_path).unwrap();
        let mut config = ResolvedConfig::default();
        config.credentials.git.enabled = false;
        config.credentials.github.enabled = false;
        config.credentials.github.mode = GithubCredentialsMode::Off;
        let plan = test_up_plan_with_config(config);

        let (plan, _runtime) = add_credential_runtime_mounts_with_inputs(
            plan,
            &runtime_dir,
            Some(&socket_path),
            Some("disabled-github-secret\n"),
        )
        .unwrap();
        let yaml = generated_compose_override_content("app", &plan).unwrap();

        assert!(yaml.contains("  'app':\n"));
        assert!(yaml.contains(DECUNE_RUNTIME_TARGET));
        assert!(!yaml.contains(SSH_AGENT_SOCKET_TARGET));
        assert!(!yaml.contains(GITHUB_CLI_TOKEN_TARGET));
        assert!(!yaml.contains(GITHUB_CLI_CONFIG_TARGET));
        assert!(!yaml.contains("SSH_AUTH_SOCK"));
        assert!(!yaml.contains("GH_CONFIG_DIR"));
        assert!(!yaml.contains("disabled-github-secret"));
    }

    #[test]
    fn existing_container_decision_reuses_runtime_mount_when_forward_ports_are_added_later() {
        let runtime_dir = tempfile::tempdir().unwrap();
        let mut config = ResolvedConfig::default();
        config.credentials.git.enabled = false;
        config.credentials.github.enabled = false;
        config.credentials.github.mode = GithubCredentialsMode::Off;
        let mut plan = test_up_plan_with_config(config);
        plan.forward_ports = vec![ResolvedForwardPort {
            service: None,
            container: 4321,
            host: 54321,
            host_ip: "127.0.0.1".to_owned(),
            protocol: PortProtocol::Tcp,
            require_local: false,
            label: None,
        }];
        let (_plan, runtime) =
            add_credential_runtime_mounts_with_inputs(plan, runtime_dir.path(), None, None)
                .unwrap();
        let container = UpContainerSummary {
            id: "container-id".to_owned(),
            name: "decune-project-abc123".to_owned(),
            image_id: None,
            config_hash: Some("stable-hash".to_owned()),
            config_file: None,
            mounts: Some(vec![mount_summary(
                runtime_dir.path().to_str(),
                DECUNE_RUNTIME_TARGET,
            )]),
            running: true,
        };

        let decision =
            decide_existing_container(&[container], "stable-hash", runtime.mount_policy(), false)
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

    #[test]
    fn default_workspace_folder_uses_real_workspace_basename() {
        let workspace = test_workspace("Project Name!");

        assert_eq!(
            default_workspace_folder(&workspace),
            "/workspaces/Project Name!"
        );
    }

    #[test]
    fn build_up_plan_uses_image_source_and_default_workspace_mount() {
        let workspace = test_workspace("image-plan");
        write_devcontainer(
            &workspace,
            r#"
            {
              "image": "alpine:3.20",
              "workspaceFolder": "/workspace"
            }
            "#,
        );

        let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_eq!(plan.image, "alpine:3.20");
        assert!(plan.build_context.is_none());
        assert_eq!(plan.workspace_folder, "/workspace");
        assert_eq!(plan.mounts.len(), 1);
        assert_eq!(
            plan.mounts[0].source.as_deref(),
            Some(workspace.root().to_str().unwrap())
        );
        assert_eq!(plan.mounts[0].target, "/workspaces/image-plan");
        assert_eq!(plan.mounts[0].mount_type, MountType::Bind);
        assert!(!plan.mounts[0].read_only);
        assert!(matches!(
            plan.config.devcontainer.source,
            Some(ResolvedDevcontainerSource::Image(ref image)) if image == "alpine:3.20"
        ));
        assert_eq!(
            plan.resources.labels["devcontainer.config_file"],
            workspace
                .root()
                .join(".devcontainer/devcontainer.json")
                .display()
                .to_string()
        );
    }

    #[test]
    fn build_up_plan_includes_feature_lock_digest_in_config_hash() {
        let workspace = test_workspace("feature-lock-hash");
        write_devcontainer(
            &workspace,
            r#"
            {
              "image": "alpine:3.20",
              "features": {
                "ghcr.io/example/features/tool:1": {}
              }
            }
            "#,
        );
        let baseline = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
        fs::create_dir_all(workspace.root().join(".decune")).unwrap();
        fs::write(
            workspace.root().join(".decune/features.lock.toml"),
            r#"
version = 1

[[features]]
id = "ghcr.io/example/features/tool"
ref = "ghcr.io/example/features/tool:1"
digest = "sha256:locked"
"#,
        )
        .unwrap();

        let locked = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_ne!(baseline.resources.config_hash, locked.resources.config_hash);
    }

    #[test]
    fn build_up_plan_ignores_feature_lock_digest_when_features_are_updated() {
        let workspace = test_workspace("feature-lock-update-hash");
        write_devcontainer(
            &workspace,
            r#"
            {
              "image": "alpine:3.20",
              "features": {
                "ghcr.io/example/features/tool:1": {}
              }
            }
            "#,
        );
        let baseline = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
        fs::create_dir_all(workspace.root().join(".decune")).unwrap();
        fs::write(
            workspace.root().join(".decune/features.lock.toml"),
            r#"
version = 1

[[features]]
id = "ghcr.io/example/features/tool"
ref = "ghcr.io/example/features/tool:1"
digest = "sha256:locked"
"#,
        )
        .unwrap();

        let locked = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
        let updated =
            build_up_plan_with_update_features(&workspace, None, ConfigLayer::default(), true)
                .unwrap();

        assert_ne!(baseline.resources.config_hash, locked.resources.config_hash);
        assert_eq!(
            baseline.resources.config_hash,
            updated.resources.config_hash
        );
    }

    #[test]
    fn build_up_plan_rejects_invalid_feature_ref_with_ref_in_error() {
        let workspace = test_workspace("invalid-feature-ref");
        write_devcontainer(
            &workspace,
            r#"
            {
              "image": "alpine:3.20",
              "features": {
                "ghcr.io/features": {}
              }
            }
            "#,
        );

        let error = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap_err();

        assert!(error.to_string().contains("ghcr.io/features"), "{error:#}");
    }

    #[test]
    fn github_cli_auto_add_requires_token_and_missing_container_binary() {
        let mut config = ResolvedConfig::default();

        assert!(should_auto_add_github_cli_feature(&config, true, false));
        assert!(!should_auto_add_github_cli_feature(&config, false, false));
        assert!(!should_auto_add_github_cli_feature(&config, true, true));

        config.credentials.github.install_feature_if_missing = false;
        assert!(!should_auto_add_github_cli_feature(&config, true, false));

        config.credentials.github.install_feature_if_missing = true;
        config.credentials.github.enabled = false;
        assert!(!should_auto_add_github_cli_feature(&config, true, false));

        config.credentials.github.enabled = true;
        config.credentials.github.mode = GithubCredentialsMode::Off;
        assert!(!should_auto_add_github_cli_feature(&config, true, false));
    }

    #[test]
    fn github_cli_auto_add_injects_feature_once() {
        let plan = test_up_plan_with_image_source("alpine:3.20");

        let plan = add_github_cli_feature_to_plan(plan).unwrap();
        let plan = add_github_cli_feature_to_plan(plan).unwrap();

        let github_cli_features = plan
            .config
            .features
            .iter()
            .filter(|feature| feature.canonical_id == "ghcr.io/devcontainers/features/github-cli")
            .collect::<Vec<_>>();
        assert_eq!(github_cli_features.len(), 1);
        assert_eq!(
            github_cli_features[0].id,
            "ghcr.io/devcontainers/features/github-cli:1"
        );
    }

    #[test]
    fn github_cli_auto_add_retickets_image_sources_to_workspace_layer() {
        let plan = test_up_plan_with_image_source("ubuntu:24.04");

        let plan = add_github_cli_feature_to_plan(plan).unwrap();

        assert_eq!(plan.base_image, "ubuntu:24.04");
        assert_eq!(plan.image, plan.resources.image_tag);
        assert_ne!(plan.image, plan.base_image);
    }

    #[test]
    fn build_up_plan_separates_forward_ports_from_app_port_publish() {
        let workspace = test_workspace("port-plan");
        write_devcontainer(
            &workspace,
            r#"
            {
              "image": "alpine:3.20"
            }
            "#,
        );
        let baseline = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        write_devcontainer(
            &workspace,
            r#"
            {
              "image": "alpine:3.20",
              "forwardPorts": [3000]
            }
            "#,
        );
        let forwarding = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        write_devcontainer(
            &workspace,
            r#"
            {
              "image": "alpine:3.20",
              "forwardPorts": [3000],
              "appPort": ["127.0.0.1:18080:8080"]
            }
            "#,
        );
        let published = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_eq!(
            forwarding.forward_ports,
            vec![ResolvedForwardPort {
                service: None,
                container: 3000,
                host: 3000,
                host_ip: "127.0.0.1".to_owned(),
                protocol: PortProtocol::Tcp,
                require_local: false,
                label: None,
            }]
        );
        assert!(forwarding.config.devcontainer.publish_ports.is_empty());
        assert_eq!(
            published.config.devcontainer.publish_ports,
            vec![ResolvedPublishPort {
                container: 8080,
                host: Some(18080),
                host_ip: Some("127.0.0.1".to_owned()),
                protocol: PortProtocol::Tcp,
            }]
        );
        assert_eq!(
            baseline.resources.config_hash,
            forwarding.resources.config_hash
        );
        assert_ne!(
            forwarding.resources.config_hash,
            published.resources.config_hash
        );
    }

    #[test]
    fn build_up_plan_rejects_service_qualified_forward_ports_without_compose_source() {
        let workspace = test_workspace("service-forward-port-image-plan");
        write_devcontainer(
            &workspace,
            r#"
            {
              "image": "alpine:3.20",
              "forwardPorts": ["db:5432"]
            }
            "#,
        );

        let error = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap_err();

        assert!(error.to_string().contains(
            "Service-qualified port forwarding is only supported in Docker Compose mode"
        ));
        assert!(error.to_string().contains("db:5432"));
    }

    #[test]
    fn build_up_plan_rejects_service_qualified_forward_ports_with_dockerfile_source() {
        let workspace = test_workspace("service-forward-port-dockerfile-plan");
        fs::create_dir_all(workspace.root().join(".devcontainer")).unwrap();
        fs::write(
            workspace.root().join(".devcontainer/Dockerfile"),
            "FROM alpine:3.20\n",
        )
        .unwrap();
        write_devcontainer(
            &workspace,
            r#"
            {
              "build": {
                "dockerfile": "Dockerfile"
              },
              "forwardPorts": ["db:5432"]
            }
            "#,
        );

        let error = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap_err();

        assert!(error.to_string().contains(
            "Service-qualified port forwarding is only supported in Docker Compose mode"
        ));
        assert!(error.to_string().contains("db:5432"));
    }

    #[test]
    fn build_up_plan_rejects_service_qualified_decune_ports_without_compose_source() {
        let workspace = test_workspace("service-decune-port-image-plan");
        write_devcontainer(
            &workspace,
            r#"
            {
              "image": "alpine:3.20"
            }
            "#,
        );
        fs::create_dir_all(workspace.root().join(".decune")).unwrap();
        fs::write(
            workspace.root().join(".decune/config.toml"),
            r#"
version = 1

[[ports]]
service = "db"
container = 5432
"#,
        )
        .unwrap();

        let error = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap_err();

        assert!(error.to_string().contains(
            "Service-qualified port forwarding is only supported in Docker Compose mode"
        ));
        assert!(error.to_string().contains("db:5432"));
    }

    #[test]
    fn security_notices_are_empty_for_default_plan_security_surface() {
        let mut config = ResolvedConfig::default();
        config.credentials.git.enabled = false;
        config.credentials.github.enabled = false;
        let notices = security_notices(&config);

        assert!(notices.is_empty());
    }

    #[test]
    fn security_notices_report_risky_container_settings() {
        let mut config = ResolvedConfig::default();
        config.credentials.git.enabled = false;
        config.credentials.github.enabled = false;
        config.devcontainer.privileged = Some(true);
        config.devcontainer.cap_add = vec!["SYS_PTRACE".to_owned()];
        config.devcontainer.security_opt = vec!["seccomp=unconfined".to_owned()];
        config.devcontainer.mounts = vec![crate::config::layer::LayerDevcontainerMount::String(
            "type=bind,source=/tmp,target=/host-tmp".to_owned(),
        )];

        let notices = security_notices(&config);

        assert!(notices.iter().any(|notice| notice.contains("privileged")));
        assert!(notices.iter().any(|notice| notice.contains("SYS_PTRACE")));
        assert!(
            notices
                .iter()
                .any(|notice| notice.contains("seccomp=unconfined"))
        );
        assert!(
            notices
                .iter()
                .any(|notice| notice.contains("additional bind mounts"))
        );
        assert!(notices.iter().all(|notice| !notice.contains("/tmp")));
    }

    #[test]
    fn security_notices_skip_devcontainer_volume_mounts() {
        let mut config = ResolvedConfig::default();
        config.credentials.git.enabled = false;
        config.credentials.github.enabled = false;
        config.devcontainer.mounts = vec![
            crate::config::layer::LayerDevcontainerMount::String(
                "type=volume,source=project-cache,target=/cache".to_owned(),
            ),
            crate::config::layer::LayerDevcontainerMount::Object(
                [
                    ("type".to_owned(), serde_json::json!("volume")),
                    ("source".to_owned(), serde_json::json!("project-deps")),
                    ("target".to_owned(), serde_json::json!("/deps")),
                ]
                .into(),
            ),
        ];
        config.devcontainer.workspace_mount =
            Some("type=volume,source=project-workspace,target=/workspace".to_owned());

        let notices = security_notices(&config);

        assert!(
            notices
                .iter()
                .all(|notice| !notice.contains("additional bind mounts"))
        );
    }

    #[test]
    fn security_notices_report_devcontainer_workspace_bind_mount() {
        let mut config = ResolvedConfig::default();
        config.credentials.git.enabled = false;
        config.credentials.github.enabled = false;
        config.devcontainer.workspace_mount =
            Some("type=bind,source=${localWorkspaceFolder},target=/workspace".to_owned());

        let notices = security_notices(&config);

        assert!(
            notices
                .iter()
                .any(|notice| notice.contains("additional bind mounts"))
        );
    }

    #[test]
    fn security_notices_report_code_execution_surfaces() {
        let mut config = ResolvedConfig::default();
        config.credentials.git.enabled = false;
        config.credentials.github.enabled = false;
        config.features = vec![crate::config::resolved::ResolvedFeature {
            id: "tool".to_owned(),
            canonical_id: "ghcr.io/example/features/tool".to_owned(),
            options: BTreeMap::new(),
        }];
        config.devcontainer.source = Some(ResolvedDevcontainerSource::Dockerfile(
            crate::config::layer::LayerDevcontainerBuild {
                dockerfile: "Dockerfile".to_owned(),
                context: None,
                args: BTreeMap::new(),
                options: Vec::new(),
                target: None,
                cache_from: Vec::new(),
            },
        ));
        config.devcontainer.entrypoints = vec!["/usr/local/bin/start".to_owned()];
        config.devcontainer.lifecycle = Some(
            crate::devcontainer::lifecycle::parse_lifecycle_definition(&BTreeMap::from([(
                crate::devcontainer::metadata::LifecycleProperty::PostStartCommand,
                serde_json::json!("make setup"),
            )]))
            .unwrap(),
        );
        config.devcontainer.user_env_probe = Some(LayerUserEnvProbe::LoginShell);

        let notices = security_notices(&config);

        assert!(notices.iter().any(|notice| notice.contains("Dockerfile")));
        assert!(notices.iter().any(|notice| notice.contains("install.sh")));
        assert!(notices.iter().any(|notice| notice.contains("entrypoint")));
        assert!(notices.iter().any(|notice| notice.contains("lifecycle")));
        assert!(notices.iter().any(|notice| {
            notice.contains("userEnvProbe") && notice.contains("userEnvProbe to \"none\"")
        }));
    }

    #[test]
    fn security_notices_report_enabled_credentials() {
        let notices = security_notices(&ResolvedConfig::default());

        assert!(notices.iter().any(|notice| {
            notice.contains("Git credential forwarding")
                && notice.contains("[credentials.git].enabled = false")
        }));
        assert!(notices.iter().any(|notice| {
            notice.contains("SSH agent forwarding") && notice.contains("ssh_agent = \"off\"")
        }));
        assert!(notices.iter().any(|notice| {
            notice.contains("GitHub credential forwarding")
                && notice.contains("[credentials.github].enabled = false")
        }));

        let mut disabled = ResolvedConfig::default();
        disabled.credentials.git.enabled = false;
        disabled.credentials.github.enabled = false;
        let disabled_notices = security_notices(&disabled);
        assert!(
            disabled_notices
                .iter()
                .all(|notice| !notice.contains("credential forwarding"))
        );

        let mut ssh_off = ResolvedConfig::default();
        ssh_off.credentials.git.ssh_agent = SshAgentMode::Off;
        let ssh_off_notices = security_notices(&ssh_off);
        assert!(
            ssh_off_notices
                .iter()
                .all(|notice| !notice.contains("SSH agent forwarding"))
        );
    }

    #[test]
    fn deferred_config_warnings_report_app_port_without_explicit_host_ip() {
        let mut config = ResolvedConfig::default();
        config.devcontainer.publish_ports = vec![ResolvedPublishPort {
            container: 8080,
            host: Some(18080),
            host_ip: None,
            protocol: PortProtocol::Tcp,
        }];

        let warnings = deferred_config_warnings(&config);
        let warning = warnings
            .iter()
            .find(|warning| warning.contains("appPort"))
            .expect("expected appPort warning");

        assert!(warning.contains("forwardPorts"));
        assert!(warning.contains("[[ports]]"));
        assert!(warning.contains("localhost-only"));
    }

    #[test]
    fn deferred_config_warnings_skip_localhost_only_app_port() {
        let mut config = ResolvedConfig::default();
        config.devcontainer.publish_ports = vec![ResolvedPublishPort {
            container: 8080,
            host: Some(18080),
            host_ip: Some("127.0.0.1".to_owned()),
            protocol: PortProtocol::Tcp,
        }];

        let warnings = deferred_config_warnings(&config);

        assert!(!warnings.iter().any(|warning| warning.contains("appPort")));
    }

    #[test]
    fn deferred_config_warnings_report_unsupported_port_attributes() {
        let mut config = ResolvedConfig::default();
        config.devcontainer.port_attributes.insert(
            "3000".to_owned(),
            ResolvedPortAttributes {
                label: Some("web".to_owned()),
                on_auto_forward: None,
                require_local_port: Some(true),
                unsupported_protocol: Some("https".to_owned()),
                unsupported_elevate_if_needed: Some(true),
            },
        );
        config.devcontainer.other_ports_attributes = Some(ResolvedPortAttributes {
            label: None,
            on_auto_forward: None,
            require_local_port: None,
            unsupported_protocol: Some("http".to_owned()),
            unsupported_elevate_if_needed: None,
        });

        let warnings = deferred_config_warnings(&config);

        assert!(warnings.iter().any(|warning| {
            warning.contains("portsAttributes.3000.protocol")
                && warning.contains("ignored")
                && warning.contains("label")
        }));
        assert!(warnings.iter().any(|warning| {
            warning.contains("portsAttributes.3000.elevateIfNeeded") && warning.contains("ignored")
        }));
        assert!(warnings.iter().any(|warning| {
            warning.contains("otherPortsAttributes.protocol") && warning.contains("ignored")
        }));
    }

    #[test]
    fn detached_up_plan_keeps_config_hash_stable_when_forward_ports_are_ignored() {
        let workspace = test_workspace("detached-forward-port-hash-plan");
        write_devcontainer(
            &workspace,
            r#"
            {
              "image": "alpine:3.20",
              "forwardPorts": [3000]
            }
            "#,
        );

        let attached = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
        let detached = build_up_plan_with_forwarding_resolution(
            &workspace,
            None,
            ConfigLayer::default(),
            ForwardingResolution::IgnoreDetached,
            false,
            false,
        )
        .unwrap();

        assert_eq!(
            attached.forward_ports,
            vec![ResolvedForwardPort {
                service: None,
                container: 3000,
                host: 3000,
                host_ip: "127.0.0.1".to_owned(),
                protocol: PortProtocol::Tcp,
                require_local: false,
                label: None,
            }]
        );
        assert!(detached.forward_ports.is_empty());
        assert!(detached.ignored_detached_forwarding);
        assert_eq!(
            attached.resources.config_hash,
            detached.resources.config_hash
        );
    }

    #[test]
    fn detached_up_plan_ignores_forward_ports_without_binding_host_port() {
        let workspace = test_workspace("detached-port-plan");
        write_devcontainer(
            &workspace,
            r#"
            {
              "image": "alpine:3.20"
            }
            "#,
        );
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let host_port = listener.local_addr().unwrap().port();
        fs::create_dir_all(workspace.root().join(".decune")).unwrap();
        fs::write(
            workspace.root().join(".decune/config.toml"),
            format!(
                r#"
version = 1

[[ports]]
container = 4321
host = {host_port}
require_local = true
"#
            ),
        )
        .unwrap();

        let plan = build_up_plan_with_forwarding_resolution(
            &workspace,
            None,
            ConfigLayer::default(),
            ForwardingResolution::IgnoreDetached,
            false,
            false,
        )
        .unwrap();

        assert!(plan.forward_ports.is_empty());
        assert_eq!(plan.config.ports.entries.len(), 1);
        assert!(plan.ignored_detached_forwarding);
    }

    #[test]
    fn detached_up_plan_ignores_unsupported_devcontainer_forward_ports_before_conversion() {
        let workspace = test_workspace("detached-unsupported-forward-port-plan");
        write_devcontainer(
            &workspace,
            r#"
            {
              "image": "alpine:3.20",
              "forwardPorts": ["db:5432"]
            }
            "#,
        );

        let plan = build_up_plan_with_forwarding_resolution(
            &workspace,
            None,
            ConfigLayer::default(),
            ForwardingResolution::IgnoreDetached,
            false,
            false,
        )
        .unwrap();

        assert!(plan.forward_ports.is_empty());
        assert!(plan.config.ports.entries.is_empty());
        assert!(plan.ignored_detached_forwarding);
    }

    #[test]
    fn compose_forwarding_targets_split_primary_and_sidecar_services() {
        let mut plan = test_up_plan_with_config(compose_config("app"));
        plan.forward_ports = vec![
            forward_port_for_service(None, 3000),
            forward_port_for_service(Some("app"), 3001),
            forward_port_for_service(Some("db"), 5432),
        ];

        let targets =
            plan_forwarding_agent_targets(&plan, PathBuf::from("/tmp/decune-runtime").as_path())
                .unwrap();

        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].service.as_deref(), None);
        assert_eq!(
            targets[0]
                .forward_ports
                .iter()
                .map(|port| port.container)
                .collect::<Vec<_>>(),
            vec![3000, 3001]
        );
        assert!(targets[0].auto_forward.is_some());
        assert_eq!(targets[1].service.as_deref(), Some("db"));
        assert_eq!(targets[1].forward_ports[0].container, 5432);
        assert!(targets[1].auto_forward.is_none());
    }

    #[test]
    fn compose_automatic_forwarding_targets_primary_service_only() {
        let mut plan = test_up_plan_with_config(compose_config("app"));
        plan.forward_ports = vec![forward_port_for_service(Some("db"), 5432)];

        let targets =
            plan_forwarding_agent_targets(&plan, PathBuf::from("/tmp/decune-runtime").as_path())
                .unwrap();

        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].service.as_deref(), None);
        assert!(targets[0].forward_ports.is_empty());
        assert!(targets[0].auto_forward.is_some());
        assert_eq!(targets[1].service.as_deref(), Some("db"));
        assert_eq!(targets[1].forward_ports[0].container, 5432);
        assert!(targets[1].auto_forward.is_none());
    }

    #[test]
    fn build_up_plan_rejects_workspace_mount_without_workspace_folder() {
        let workspace = test_workspace("workspace-mount-plan");
        write_devcontainer(
            &workspace,
            r#"
            {
              "image": "alpine:3.20",
              "workspaceMount": "source=${localWorkspaceFolder},target=/workspace,type=bind"
            }
            "#,
        );

        let error = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap_err();

        assert_eq!(
            error.to_string(),
            "workspaceFolder is required when workspaceMount is specified"
        );
    }

    #[test]
    fn preliminary_up_plan_defers_workspace_mount_without_workspace_folder() {
        let workspace = test_workspace("preliminary-workspace-mount-plan");
        write_devcontainer(
            &workspace,
            r#"
            {
              "image": "alpine:3.20",
              "workspaceMount": "source=${localWorkspaceFolder},target=/workspace,type=bind"
            }
            "#,
        );

        let plan = build_preliminary_up_plan_with_forwarding_resolution(
            &workspace,
            None,
            ConfigLayer::default(),
            ForwardingResolution::Resolve,
            false,
            false,
        )
        .unwrap();

        assert_eq!(plan.workspace_folder, "/workspace");
        assert_eq!(plan.mounts[0].target, "/workspace");
    }

    #[test]
    fn build_up_plan_uses_explicit_workspace_folder_for_workspace_mount_variables() {
        let workspace = test_workspace("workspace-mount-variable-plan");
        write_devcontainer(
            &workspace,
            r#"
            {
              "image": "alpine:3.20",
              "workspaceMount": "source=${localWorkspaceFolder},target=${containerWorkspaceFolder},type=bind",
              "workspaceFolder": "/workspace"
            }
            "#,
        );

        let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_eq!(plan.workspace_folder, "/workspace");
        assert_eq!(plan.mounts.len(), 1);
        assert_eq!(
            plan.mounts[0].source.as_deref(),
            Some(workspace.root().to_str().unwrap())
        );
        assert_eq!(plan.mounts[0].target, plan.workspace_folder);
        assert_eq!(plan.mounts[0].mount_type, MountType::Bind);
    }

    #[test]
    fn build_up_plan_defers_workspace_folder_mount_target_check_until_runtime() {
        let workspace = test_workspace("workspace-folder-outside-mount-plan");
        write_devcontainer(
            &workspace,
            r#"
            {
              "image": "alpine:3.20",
              "workspaceMount": "source=${localWorkspaceFolder},target=/workspace,type=bind",
              "workspaceFolder": "/other"
            }
            "#,
        );

        let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_eq!(plan.workspace_folder, "/other");
        assert_eq!(plan.mounts[0].target, "/workspace");
    }

    #[test]
    fn build_up_plan_rejects_relative_workspace_folder() {
        let workspace = test_workspace("relative-workspace-folder-plan");
        write_devcontainer(
            &workspace,
            r#"
            {
              "image": "alpine:3.20",
              "workspaceFolder": "workspace"
            }
            "#,
        );

        let error = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap_err();

        assert_eq!(
            error.to_string(),
            "workspaceFolder must be an absolute container path: workspace"
        );
    }

    #[test]
    fn build_up_plan_uses_explicit_workspace_folder_with_workspace_mount() {
        let workspace = test_workspace("workspace-folder-plan");
        write_devcontainer(
            &workspace,
            r#"
            {
              "image": "alpine:3.20",
              "workspaceMount": "source=${localWorkspaceFolder},target=/workspace,type=bind",
              "workspaceFolder": "/workspace/app"
            }
            "#,
        );
        fs::create_dir_all(workspace.root().join(".decune")).unwrap();
        fs::write(
            workspace.root().join(".decune/config.toml"),
            r#"
version = 1

[[mounts]]
source = "project-cache"
target = "/opt/${containerWorkspaceFolderBasename}"
type = "volume"
"#,
        )
        .unwrap();

        let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_eq!(plan.workspace_folder, "/workspace/app");
        assert_eq!(plan.mounts[0].target, "/workspace");
        assert_eq!(plan.mounts[1].target, "/opt/app");
    }

    #[test]
    fn build_up_plan_rejects_mount_target_that_conflicts_with_workspace_mount() {
        let workspace = test_workspace("workspace-mount-conflict-plan");
        write_devcontainer(
            &workspace,
            r#"
            {
              "image": "alpine:3.20"
            }
            "#,
        );
        fs::create_dir_all(workspace.root().join(".decune")).unwrap();
        fs::write(
            workspace.root().join(".decune/config.toml"),
            format!(
                r#"
version = 1

[[mounts]]
source = "project-cache"
target = "{}"
type = "volume"
"#,
                default_workspace_folder(&workspace)
            ),
        )
        .unwrap();

        let error = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Mount target conflicts with workspace mount target")
        );
    }

    #[test]
    fn build_up_plan_rejects_mount_target_that_normalizes_to_workspace_mount() {
        let workspace = test_workspace("normalized-workspace-mount-conflict-plan");
        write_devcontainer(
            &workspace,
            r#"
            {
              "image": "alpine:3.20"
            }
            "#,
        );
        fs::create_dir_all(workspace.root().join(".decune")).unwrap();
        fs::write(
            workspace.root().join(".decune/config.toml"),
            format!(
                r#"
version = 1

[[mounts]]
source = "project-cache"
target = "{}/."
type = "volume"
"#,
                default_workspace_folder(&workspace)
            ),
        )
        .unwrap();

        let error = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Mount target conflicts with workspace mount target")
        );
    }

    #[test]
    fn build_up_plan_rejects_workspace_mount_under_reserved_decune_path() {
        let workspace = test_workspace("reserved-workspace-mount-plan");
        write_devcontainer(
            &workspace,
            r#"
            {
              "image": "alpine:3.20",
              "workspaceMount": "source=${localWorkspaceFolder},target=/run/decune/workspace,type=bind",
              "workspaceFolder": "/run/decune/workspace"
            }
            "#,
        );

        let error = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap_err();
        let message = format!("{error:#}");

        assert!(message.contains("Mount target is reserved for decune internal use"));
    }

    #[test]
    fn build_up_plan_merges_image_metadata_and_includes_it_in_config_hash() {
        let workspace = test_workspace("image-metadata-plan");
        write_devcontainer(
            &workspace,
            r#"
            {
              "image": "alpine:3.20"
            }
            "#,
        );
        let image_layer = ConfigLayer {
            devcontainer: Some(crate::config::layer::LayerDevcontainerMetadata {
                remote_user: Some("image-user".to_owned()),
                remote_env: [("FROM_IMAGE".to_owned(), "1".to_owned())].into(),
                ..crate::config::layer::LayerDevcontainerMetadata::default()
            }),
            ..ConfigLayer::default()
        };
        let changed_image_layer = ConfigLayer {
            devcontainer: Some(crate::config::layer::LayerDevcontainerMetadata {
                remote_user: Some("image-user".to_owned()),
                remote_env: [("FROM_IMAGE".to_owned(), "2".to_owned())].into(),
                ..crate::config::layer::LayerDevcontainerMetadata::default()
            }),
            ..ConfigLayer::default()
        };

        let plan = build_up_plan_with_image_metadata(
            &workspace,
            None,
            ConfigLayer::default(),
            vec![image_layer],
        )
        .unwrap();
        let changed = build_up_plan_with_image_metadata(
            &workspace,
            None,
            ConfigLayer::default(),
            vec![changed_image_layer],
        )
        .unwrap();

        assert_eq!(
            plan.config.devcontainer.remote_user.as_deref(),
            Some("image-user")
        );
        assert_eq!(
            plan.config
                .devcontainer
                .remote_env
                .get("FROM_IMAGE")
                .map(String::as_str),
            Some("1")
        );
        assert_ne!(plan.resources.config_hash, changed.resources.config_hash);
    }

    #[test]
    fn build_up_plan_uses_dockerfile_source_and_build_context() {
        let workspace = test_workspace("dockerfile-plan");
        fs::create_dir_all(workspace.root().join(".devcontainer")).unwrap();
        fs::write(
            workspace.root().join(".devcontainer/Dockerfile"),
            "FROM alpine\n",
        )
        .unwrap();
        write_devcontainer(
            &workspace,
            r#"
            {
              "build": {
                "dockerfile": "Dockerfile",
                "args": {
                  "VARIANT": "bookworm"
                },
                "options": [
                  "--platform=linux/amd64",
                  "--network",
                  "host"
                ],
                "target": "dev",
                "cacheFrom": "type=registry,ref=example.test/cache:latest"
              }
            }
            "#,
        );

        let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_eq!(plan.image, plan.resources.image_tag);
        let build_context = plan
            .build_context
            .expect("build context should be resolved");
        assert_eq!(
            build_context.context_dir,
            workspace.root().join(".devcontainer")
        );
        assert_eq!(
            build_context.dockerfile_path,
            workspace.root().join(".devcontainer/Dockerfile")
        );
        assert_eq!(
            build_context.dockerfile_in_context,
            PathBuf::from("Dockerfile")
        );
        assert_eq!(
            plan.build_options
                .build_args
                .get("VARIANT")
                .map(String::as_str),
            Some("bookworm")
        );
        assert_eq!(plan.build_options.target.as_deref(), Some("dev"));
        assert_eq!(
            plan.build_options.options,
            vec!["--platform=linux/amd64", "--network", "host"]
        );
        assert_eq!(
            plan.build_options.cache_from,
            vec!["type=registry,ref=example.test/cache:latest"]
        );
        assert!(!plan.build_options.no_cache);
        assert!(!plan.build_options.pull);
    }

    #[test]
    fn build_up_plan_hash_changes_when_dockerfile_content_changes() {
        let workspace = test_workspace("dockerfile-hash-plan");
        fs::create_dir_all(workspace.root().join(".devcontainer")).unwrap();
        fs::write(
            workspace.root().join(".devcontainer/Dockerfile"),
            "FROM alpine\n",
        )
        .unwrap();
        write_devcontainer(
            &workspace,
            r#"
            {
              "build": {
                "dockerfile": "Dockerfile"
              }
            }
            "#,
        );

        let first = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
        fs::write(
            workspace.root().join(".devcontainer/Dockerfile"),
            "FROM alpine\nRUN true\n",
        )
        .unwrap();
        let second = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_ne!(first.resources.config_hash, second.resources.config_hash);
        assert_ne!(first.image, second.image);
    }

    #[cfg(unix)]
    #[test]
    fn build_up_plan_hash_changes_when_resolved_mount_source_changes() {
        let workspace = test_workspace("mount-source-hash-plan");
        write_devcontainer(
            &workspace,
            r#"
            {
              "image": "alpine:3.20"
            }
            "#,
        );
        fs::create_dir_all(workspace.root().join("first-cache")).unwrap();
        fs::create_dir_all(workspace.root().join("second-cache")).unwrap();
        let link = workspace.root().join("host-cache");
        std::os::unix::fs::symlink(workspace.root().join("first-cache"), &link).unwrap();
        fs::create_dir_all(workspace.root().join(".decune")).unwrap();
        fs::write(
            workspace.root().join(".decune/config.toml"),
            r#"
version = 1

[[mounts]]
source = "host-cache"
target = "/cache"
type = "bind"
resolve_symlink = true
"#,
        )
        .unwrap();

        let first = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(workspace.root().join("second-cache"), &link).unwrap();
        let second = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_ne!(first.mounts[1].source, second.mounts[1].source);
        assert_ne!(first.resources.config_hash, second.resources.config_hash);
    }

    #[test]
    fn config_hash_changes_when_resolved_mount_options_change() {
        let mut cached = test_mount();
        cached.consistency = Some("cached".to_owned());
        let mut delegated = test_mount();
        delegated.consistency = Some("delegated".to_owned());
        assert_ne!(
            config_hash_for_mount(cached),
            config_hash_for_mount(delegated)
        );

        let mut rshared = test_mount();
        rshared.bind_options = Some(MountBindOptions {
            propagation: Some(MountBindPropagation::RShared),
            ..MountBindOptions::default()
        });
        let mut rslave = test_mount();
        rslave.bind_options = Some(MountBindOptions {
            propagation: Some(MountBindPropagation::RSlave),
            ..MountBindOptions::default()
        });
        assert_ne!(
            config_hash_for_mount(rshared),
            config_hash_for_mount(rslave)
        );

        let mut deps = test_volume_mount();
        deps.volume_options = Some(MountVolumeOptions {
            subpath: Some("deps".to_owned()),
            ..MountVolumeOptions::default()
        });
        let mut cache = test_volume_mount();
        cache.volume_options = Some(MountVolumeOptions {
            subpath: Some("cache".to_owned()),
            ..MountVolumeOptions::default()
        });
        assert_ne!(config_hash_for_mount(deps), config_hash_for_mount(cache));
    }

    #[test]
    fn build_up_plan_uses_container_workspace_folder_basename_variable() {
        let workspace = test_workspace("container-basename-plan");
        write_devcontainer(
            &workspace,
            r#"
            {
              "image": "alpine:3.20",
              "workspaceFolder": "/src"
            }
            "#,
        );
        fs::create_dir_all(workspace.root().join(".decune")).unwrap();
        fs::write(
            workspace.root().join(".decune/config.toml"),
            r#"
version = 1

[[mounts]]
source = "project-cache"
target = "/opt/${containerWorkspaceFolderBasename}"
type = "volume"
"#,
        )
        .unwrap();

        let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_eq!(plan.workspace_folder, "/src");
        assert_eq!(plan.mounts[0].target, default_workspace_folder(&workspace));
        assert_eq!(plan.mounts[1].target, "/opt/src");
    }

    #[cfg(unix)]
    #[test]
    fn build_up_plan_uses_current_uid_and_gid_variables() {
        let workspace = test_workspace("uid-gid-plan");
        write_devcontainer(
            &workspace,
            r#"
            {
              "image": "alpine:3.20"
            }
            "#,
        );
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        let cache = workspace.root().join(format!("{uid}-{gid}"));
        fs::create_dir_all(&cache).unwrap();
        fs::create_dir_all(workspace.root().join(".decune")).unwrap();
        fs::write(
            workspace.root().join(".decune/config.toml"),
            r#"
version = 1

[[mounts]]
source = "${uid}-${gid}"
target = "/cache"
type = "bind"
"#,
        )
        .unwrap();

        let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_eq!(
            plan.mounts[1].source.as_deref(),
            Some(cache.canonicalize().unwrap().to_str().unwrap())
        );
    }

    #[test]
    fn up_detach_creates_and_reuses_container() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-detach");
            write_devcontainer(
                &workspace,
                r#"
                {
                  "image": "alpine:3.20"
                }
                "#,
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;

                let first = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                assert_eq!(first.container_name, container_name);
                assert!(!first.reused);

                let inspect = client.cli().inspect_container(&container_name).await?;
                assert_eq!(inspect.state.and_then(|state| state.running), Some(true));

                let second = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                assert_eq!(second.container_name, container_name);
                assert!(second.reused);

                Ok(())
            }
            .await;

            let cleanup = remove_container(&client, &container_name, true, true).await;
            result.and(cleanup).unwrap();
        });
    }

    #[test]
    fn create_and_start_container_uses_effective_container_user() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let mut plan = test_up_plan_with_image_source("alpine:3.20");
            plan.workspace_folder = "/".to_owned();
            plan.effective_users = resolve_effective_users(EffectiveUserResolveInput {
                devcontainer_remote_user: None,
                devcontainer_container_user: Some("nobody"),
                image_metadata_remote_user: None,
                image_metadata_container_user: None,
                image_config_user: None,
            })
            .unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                let workspace = test_workspace("docker-up-effective-container-user-state");
                create_and_start_container(&client, &workspace, &plan, false, false, false).await?;

                let inspect = client.cli().inspect_container(&container_name).await?;
                assert_eq!(
                    inspect.config.and_then(|config| config.user),
                    Some("nobody".to_owned())
                );

                Ok(())
            }
            .await;

            let cleanup = remove_container(&client, &container_name, true, true).await;
            result.and(cleanup).unwrap();
        });
    }

    #[test]
    fn up_detach_recreates_legacy_container_missing_decune_runtime_mount() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-legacy-runtime-mount");
            write_devcontainer(
                &workspace,
                r#"
                {
                  "image": "alpine:3.20"
                }
                "#,
            );
            let legacy_plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = legacy_plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;

                let legacy = create_and_start_container(
                    &client,
                    &workspace,
                    &legacy_plan,
                    false,
                    false,
                    false,
                )
                .await?;
                let legacy_inspect = client.cli().inspect_container(&container_name).await?;
                assert!(!container_has_mount_target(
                    &legacy_inspect.mounts,
                    "/run/decune"
                ));

                let recreated = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                assert!(!recreated.reused);
                assert_ne!(legacy.container_id, recreated.container_id);

                let recreated_inspect = client.cli().inspect_container(&container_name).await?;
                assert!(container_has_mount_target(
                    &recreated_inspect.mounts,
                    "/run/decune"
                ));

                Ok(())
            }
            .await;

            let cleanup = remove_container(&client, &container_name, true, true).await;
            result.and(cleanup).unwrap();
        });
    }

    #[test]
    fn up_detach_reuses_container_when_built_image_tag_is_removed() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-reuse-missing-image-tag");
            fs::create_dir_all(workspace.root().join(".devcontainer")).unwrap();
            fs::write(
                workspace.root().join(".devcontainer/Dockerfile"),
                r#"
                FROM alpine:3.20
                RUN adduser -D vscode
                USER vscode
                "#,
            )
            .unwrap();
            write_devcontainer(
                &workspace,
                r#"
                {
                  "build": {
                    "dockerfile": "Dockerfile"
                  }
                }
                "#,
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let image = plan.image.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;

                let first = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                assert!(!first.reused);

                remove_image(&client, &image, true).await?;

                let second = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                assert_eq!(second.container_name, container_name);
                assert!(second.reused);

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }

    #[test]
    fn up_detach_reuses_image_container_when_source_image_tag_is_removed() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-reuse-missing-source-image");
            let image = format!(
                "localhost:9/decune-test/reuse-source-image-{}:latest",
                workspace.id()
            );
            write_devcontainer(
                &workspace,
                &format!(
                    r#"
                    {{
                      "image": "{image}",
                      "initializeCommand": "docker tag alpine:3.20 {image}"
                    }}
                    "#
                ),
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                ensure_image(&client, "alpine:3.20", PullPolicy::Missing).await?;
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;

                let first = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                assert_eq!(first.container_name, container_name);
                assert!(!first.reused);

                remove_image(&client, &image, true).await?;

                let second = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                assert_eq!(second.container_name, container_name);
                assert!(second.reused);

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }

    #[test]
    fn up_detach_reuses_existing_image_config_user_when_source_tag_user_changes() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-reuse-image-user-change");
            let image = format!(
                "decune-test/reuse-image-user-change-{}:latest",
                workspace.id()
            );
            write_devcontainer(
                &workspace,
                &format!(
                    r#"
                    {{
                      "image": "{image}"
                    }}
                    "#
                ),
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;

                build_user_image(&client, &image, "olduser").await?;
                let first = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                assert!(!first.reused);

                build_user_image(&client, &image, "newuser").await?;
                let second = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                assert_eq!(second.container_name, container_name);
                assert!(second.reused);

                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec!["id".to_owned(), "-un".to_owned()],
                        user: Some("olduser".to_owned()),
                        working_dir: None,
                        env: BTreeMap::new(),
                        redactions: Vec::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(String::from_utf8(output.stdout)?, "olduser\n");

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }

    #[cfg(unix)]
    #[test]
    fn up_detach_syncs_remote_user_uid_gid_on_linux_host() {
        if HostPlatform::current() != HostPlatform::Linux {
            return;
        }
        let host = current_host_user_ids();
        if host.uid == 0 || host.gid == 0 {
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-uid-gid-sync");
            let image = format!("decune-test/uid-gid-sync-{}:latest", workspace.id());
            write_devcontainer(
                &workspace,
                &format!(
                    r#"
                    {{
                      "image": "{image}",
                      "remoteUser": "syncuser"
                    }}
                    "#
                ),
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;
                build_uid_gid_user_image(&client, &image, "syncuser", 2001, 2001).await?;

                let outcome = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                assert!(!outcome.reused);

                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "id -u; id -g".to_owned(),
                        ],
                        user: Some("syncuser".to_owned()),
                        working_dir: None,
                        env: BTreeMap::new(),
                        redactions: Vec::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(
                    String::from_utf8(output.stdout)?,
                    format!("{}\n{}\n", host.uid, host.gid)
                );

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }

    #[cfg(unix)]
    #[test]
    fn up_detach_syncs_container_user_uid_gid_when_remote_user_is_not_set() {
        if HostPlatform::current() != HostPlatform::Linux {
            return;
        }
        let host = current_host_user_ids();
        if host.uid == 0 || host.gid == 0 || host.uid == 2001 || host.gid == 2001 {
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-uid-gid-sync-container-user");
            let image = format!(
                "decune-test/uid-gid-sync-container-user-{}:latest",
                workspace.id()
            );
            write_devcontainer(
                &workspace,
                &format!(
                    r#"
                    {{
                      "image": "{image}",
                      "containerUser": "syncuser",
                      "postCreateCommand": "id -u >/tmp/decune-container-user-ids; id -g >>/tmp/decune-container-user-ids"
                    }}
                    "#
                ),
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;
                build_uid_gid_user_image(&client, &image, "syncuser", 2001, 2001).await?;

                let outcome = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                assert!(!outcome.reused);

                let inspect = client.cli().inspect_container(&container_name).await?;
                assert_eq!(
                    inspect.config.and_then(|config| config.user),
                    Some("syncuser".to_owned())
                );
                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "cat /tmp/decune-container-user-ids".to_owned(),
                        ],
                        user: Some("root".to_owned()),
                        working_dir: None,
                        env: BTreeMap::new(),
            redactions: Vec::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(
                    String::from_utf8(output.stdout)?,
                    format!("{}\n{}\n", host.uid, host.gid)
                );

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }

    #[cfg(unix)]
    #[test]
    fn up_detach_syncs_remote_user_without_renumbering_distinct_container_user() {
        if HostPlatform::current() != HostPlatform::Linux {
            return;
        }
        let host = current_host_user_ids();
        if host.uid == 0
            || host.gid == 0
            || host.uid == 2001
            || host.gid == 2001
            || host.uid == 2002
            || host.gid == 2002
        {
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-uid-gid-sync-distinct-users");
            let image = format!(
                "decune-test/uid-gid-sync-distinct-users-{}:latest",
                workspace.id()
            );
            write_devcontainer(
                &workspace,
                &format!(
                    r#"
                    {{
                      "image": "{image}",
                      "containerUser": "containeruser",
                      "remoteUser": "remoteuser",
                      "postCreateCommand": "id -u >/tmp/decune-remote-user-ids; id -g >>/tmp/decune-remote-user-ids"
                    }}
                    "#
                ),
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;
                build_distinct_uid_gid_users_image(&client, &image).await?;

                let outcome = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                assert!(!outcome.reused);

                let remote_output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "cat /tmp/decune-remote-user-ids".to_owned(),
                        ],
                        user: Some("root".to_owned()),
                        working_dir: None,
                        env: BTreeMap::new(),
            redactions: Vec::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(
                    String::from_utf8(remote_output.stdout)?,
                    format!("{}\n{}\n", host.uid, host.gid)
                );

                let container_output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "id -u containeruser; id -g containeruser".to_owned(),
                        ],
                        user: Some("root".to_owned()),
                        working_dir: None,
                        env: BTreeMap::new(),
            redactions: Vec::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(String::from_utf8(container_output.stdout)?, "2002\n2002\n");

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }

    #[cfg(unix)]
    #[test]
    fn up_detach_does_not_sync_remote_user_when_update_remote_user_uid_is_false() {
        if HostPlatform::current() != HostPlatform::Linux {
            return;
        }
        let host = current_host_user_ids();
        if host.uid == 0 || host.gid == 0 || (host.uid == 2001 && host.gid == 2001) {
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-uid-gid-sync-disabled");
            let image = format!(
                "decune-test/uid-gid-sync-disabled-{}:latest",
                workspace.id()
            );
            write_devcontainer(
                &workspace,
                &format!(
                    r#"
                    {{
                      "image": "{image}",
                      "remoteUser": "syncuser",
                      "updateRemoteUserUID": false,
                      "postCreateCommand": "id -u >/tmp/decune-disabled-user-ids; id -g >>/tmp/decune-disabled-user-ids"
                    }}
                    "#
                ),
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;
                build_uid_gid_user_image(&client, &image, "syncuser", 2001, 2001).await?;

                let outcome = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                assert!(!outcome.reused);

                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "cat /tmp/decune-disabled-user-ids".to_owned(),
                        ],
                        user: Some("root".to_owned()),
                        working_dir: None,
                        env: BTreeMap::new(),
            redactions: Vec::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(String::from_utf8(output.stdout)?, "2001\n2001\n");

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }

    #[cfg(unix)]
    #[test]
    fn up_plan_finalization_noops_uid_gid_sync_for_root_remote_user() {
        if HostPlatform::current() != HostPlatform::Linux {
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-uid-gid-sync-root-noop");
            write_devcontainer(
                &workspace,
                r#"
                {
                  "image": "alpine:3.20",
                  "remoteUser": "root"
                }
                "#,
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let client = DockerClient::connect_from_env().unwrap();

            ensure_image(&client, "alpine:3.20", PullPolicy::Missing)
                .await
                .unwrap();
            let (plan, image_prepared) = finalize_up_plan_mounts(
                &client,
                &workspace,
                plan,
                None,
                None,
                Some((false, false)),
                FinalizeUpPlanMountsOptions {
                    forwarding: ForwardingResolution::Resolve,
                    update_features: false,
                    compose_canonical_model: None,
                    compose_primary_service_user: None,
                    compose_primary_service: None,
                },
            )
            .await
            .unwrap();

            assert!(!image_prepared);
            assert_eq!(plan.image, "alpine:3.20");
            assert_eq!(plan.base_image, "alpine:3.20");
            assert_eq!(
                plan.uid_gid_sync_plan,
                UidGidSyncPlan::Noop {
                    reason: UidGidSyncNoopReason::Root
                }
            );
            assert!(plan.pre_uid_gid_sync_resources.is_none());
            assert!(plan.uid_gid_sync_build_context_dir.is_none());
        });
    }

    #[cfg(unix)]
    #[test]
    fn up_plan_finalization_uses_image_user_without_uid_gid_sync_when_metadata_user_is_missing() {
        if HostPlatform::current() != HostPlatform::Linux {
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-uid-gid-sync-image-user-only");
            let image = format!(
                "decune-test/uid-gid-sync-image-user-only-{}:latest",
                workspace.id()
            );
            write_devcontainer(
                &workspace,
                &format!(
                    r#"
                    {{
                      "image": "{image}"
                    }}
                    "#
                ),
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;
                build_uid_gid_user_image(&client, &image, "imageuser", 2001, 2001).await?;

                let (plan, image_prepared) = finalize_up_plan_mounts(
                    &client,
                    &workspace,
                    plan,
                    None,
                    None,
                    Some((false, false)),
                    FinalizeUpPlanMountsOptions {
                        forwarding: ForwardingResolution::Resolve,
                        update_features: false,
                        compose_canonical_model: None,
                        compose_primary_service_user: None,
                        compose_primary_service: None,
                    },
                )
                .await?;

                assert!(!image_prepared);
                assert_eq!(plan.image, image);
                assert_eq!(plan.base_image, image);
                assert_eq!(plan.effective_users.remote_user.user, "imageuser");
                assert_eq!(
                    plan.uid_gid_sync_plan,
                    UidGidSyncPlan::Noop {
                        reason: UidGidSyncNoopReason::NoExplicitUser
                    }
                );
                assert!(plan.pre_uid_gid_sync_resources.is_none());
                assert!(plan.uid_gid_sync_build_context_dir.is_none());

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }

    #[cfg(unix)]
    #[test]
    fn up_plan_finalization_includes_uid_gid_sync_state_in_final_hash_and_image_tag() {
        if HostPlatform::current() != HostPlatform::Linux {
            return;
        }
        let host = current_host_user_ids();
        if host.uid == 0 || host.gid == 0 {
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-uid-gid-sync-hash-tag");
            let image = format!(
                "decune-test/uid-gid-sync-hash-tag-{}:latest",
                workspace.id()
            );
            write_devcontainer(
                &workspace,
                &format!(
                    r#"
                    {{
                      "image": "{image}",
                      "remoteUser": "syncuser"
                    }}
                    "#
                ),
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;
                build_uid_gid_user_image(&client, &image, "syncuser", 2001, 2001).await?;

                let (plan, image_prepared) = finalize_up_plan_mounts(
                    &client,
                    &workspace,
                    plan,
                    None,
                    None,
                    Some((false, false)),
                    FinalizeUpPlanMountsOptions {
                        forwarding: ForwardingResolution::Resolve,
                        update_features: false,
                        compose_canonical_model: None,
                        compose_primary_service_user: None,
                        compose_primary_service: None,
                    },
                )
                .await?;
                let pre_sync_resources = plan
                    .pre_uid_gid_sync_resources
                    .as_ref()
                    .expect("sync plan must preserve pre-sync resources");

                assert!(!image_prepared);
                assert!(matches!(
                    plan.uid_gid_sync_plan,
                    UidGidSyncPlan::Sync { .. }
                ));
                assert_eq!(plan.image, plan.resources.image_tag);
                assert_eq!(plan.base_image, image);
                assert_eq!(
                    plan.resources.labels["decune.config_hash"],
                    plan.resources.config_hash
                );
                assert_eq!(
                    pre_sync_resources.labels["decune.config_hash"],
                    pre_sync_resources.config_hash
                );
                assert_ne!(plan.resources.config_hash, pre_sync_resources.config_hash);
                assert_ne!(plan.resources.image_tag, pre_sync_resources.image_tag);
                assert!(plan.uid_gid_sync_build_context_dir.is_some());

                let UidGidSyncPlan::Sync { target, .. } = &plan.uid_gid_sync_plan else {
                    unreachable!("sync plan was checked above");
                };
                assert_eq!(target.host, host);
                assert_eq!(target.user, "syncuser");
                assert_eq!(target.kind, UidGidSyncTargetKind::RemoteUser);

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }

    #[cfg(unix)]
    #[test]
    fn up_detach_applies_uid_gid_sync_after_feature_layer() {
        if HostPlatform::current() != HostPlatform::Linux {
            return;
        }
        let host = current_host_user_ids();
        if host.uid == 0 || host.gid == 0 || host.uid == 2001 || host.gid == 2001 {
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-uid-gid-sync-after-feature");
            let image = format!(
                "decune-test/uid-gid-sync-after-feature-{}:latest",
                workspace.id()
            );
            fs::create_dir_all(workspace.root().join(".devcontainer/features/order-tool"))
                .unwrap();
            write_devcontainer(
                &workspace,
                &format!(
                    r#"
                    {{
                      "image": "{image}",
                      "features": {{
                        "./features/order-tool": {{}}
                      }},
                      "remoteUser": "syncuser",
                      "postCreateCommand": "test \"$(cat /usr/local/share/decune-feature-syncuser-uid)\" = 2001 && test \"$(id -u)\" = {host_uid} && test \"$(id -g)\" = {host_gid}"
                    }}
                    "#,
                    host_uid = host.uid,
                    host_gid = host.gid,
                ),
            );
            fs::write(
                workspace
                    .root()
                    .join(".devcontainer/features/order-tool/devcontainer-feature.json"),
                r#"{"id":"order-tool","version":"1.0.0","name":"Order Tool"}"#,
            )
            .unwrap();
            fs::write(
                workspace
                    .root()
                    .join(".devcontainer/features/order-tool/install.sh"),
                r#"
                set -eu
                mkdir -p /usr/local/share
                id -u syncuser >/usr/local/share/decune-feature-syncuser-uid
                test "$(cat /usr/local/share/decune-feature-syncuser-uid)" = 2001
                "#,
            )
            .unwrap();
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;
                build_uid_gid_user_image(&client, &image, "syncuser", 2001, 2001).await?;

                let outcome = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                assert!(!outcome.reused);

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }

    #[cfg(unix)]
    #[test]
    fn up_detach_rewrites_numeric_image_user_after_uid_gid_sync() {
        if HostPlatform::current() != HostPlatform::Linux {
            return;
        }
        let host = current_host_user_ids();
        if host.uid == 0 || host.gid == 0 {
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-uid-gid-sync-numeric-user");
            let image = format!(
                "decune-test/uid-gid-sync-numeric-user-{}:latest",
                workspace.id()
            );
            write_devcontainer(
                &workspace,
                &format!(
                    r#"
                    {{
                      "image": "{image}",
                      "remoteUser": "syncuser"
                    }}
                    "#
                ),
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;
                build_numeric_uid_gid_user_image(&client, &image).await?;

                let outcome = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                assert!(!outcome.reused);

                let inspect = client.cli().inspect_container(&container_name).await?;
                assert_eq!(
                    inspect.config.and_then(|config| config.user),
                    Some(format!("syncuser:{}", host.gid))
                );

                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "id -u; id -g".to_owned(),
                        ],
                        user: None,
                        working_dir: None,
                        env: BTreeMap::new(),
                        redactions: Vec::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(
                    String::from_utf8(output.stdout)?,
                    format!("{}\n{}\n", host.uid, host.gid)
                );

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }

    #[cfg(unix)]
    #[test]
    fn up_detach_rewrites_named_image_user_numeric_group_after_uid_gid_sync() {
        if HostPlatform::current() != HostPlatform::Linux {
            return;
        }
        let host = current_host_user_ids();
        if host.uid == 0 || host.gid == 0 || host.gid == 2001 {
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-uid-gid-sync-named-user-numeric-group");
            let image = format!(
                "decune-test/uid-gid-sync-named-user-numeric-group-{}:latest",
                workspace.id()
            );
            write_devcontainer(
                &workspace,
                &format!(
                    r#"
                    {{
                      "image": "{image}",
                      "remoteUser": "syncuser"
                    }}
                    "#
                ),
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;
                build_named_uid_numeric_gid_user_image(&client, &image).await?;

                let outcome = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                assert!(!outcome.reused);

                let inspect = client.cli().inspect_container(&container_name).await?;
                assert_eq!(
                    inspect.config.and_then(|config| config.user),
                    Some(format!("syncuser:{}", host.gid))
                );

                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "id -u; id -g".to_owned(),
                        ],
                        user: None,
                        working_dir: None,
                        env: BTreeMap::new(),
                        redactions: Vec::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(
                    String::from_utf8(output.stdout)?,
                    format!("{}\n{}\n", host.uid, host.gid)
                );

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }

    #[cfg(unix)]
    #[test]
    fn up_detach_rewrites_numeric_remote_user_after_uid_gid_sync() {
        if HostPlatform::current() != HostPlatform::Linux {
            return;
        }
        let host = current_host_user_ids();
        if host.uid == 0 || host.gid == 0 || host.uid == 2001 || host.gid == 2001 {
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-uid-gid-sync-numeric-remote-user");
            let image = format!(
                "decune-test/uid-gid-sync-numeric-remote-user-{}:latest",
                workspace.id()
            );
            write_devcontainer(
                &workspace,
                &format!(
                    r#"
                    {{
                      "image": "{image}",
                      "remoteUser": "2001:2001",
                      "postCreateCommand": "id -u >/tmp/decune-remote-user-ids; id -g >>/tmp/decune-remote-user-ids"
                    }}
                    "#
                ),
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;
                build_numeric_uid_gid_user_image(&client, &image).await?;

                let outcome = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                assert!(!outcome.reused);

                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "cat /tmp/decune-remote-user-ids".to_owned(),
                        ],
                        user: Some("root".to_owned()),
                        working_dir: None,
                        env: BTreeMap::new(),
            redactions: Vec::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(
                    String::from_utf8(output.stdout)?,
                    format!("{}\n{}\n", host.uid, host.gid)
                );

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }

    #[test]
    fn uid_gid_sync_warning_reports_only_explicit_true_on_non_linux() {
        let default_layers = ConfigMergeInput::default();
        let explicit_true = ConfigMergeInput {
            devcontainer: Some(ConfigLayer {
                devcontainer: Some(LayerDevcontainerMetadata {
                    update_remote_user_uid: Some(true),
                    ..LayerDevcontainerMetadata::default()
                }),
                ..ConfigLayer::default()
            }),
            ..ConfigMergeInput::default()
        };
        let explicit_false = ConfigMergeInput {
            devcontainer: Some(ConfigLayer {
                devcontainer: Some(LayerDevcontainerMetadata {
                    update_remote_user_uid: Some(false),
                    ..LayerDevcontainerMetadata::default()
                }),
                ..ConfigLayer::default()
            }),
            ..ConfigMergeInput::default()
        };
        let plan = UidGidSyncPlan::Noop {
            reason: UidGidSyncNoopReason::NonLinuxHost,
        };

        assert_eq!(
            uid_gid_sync_warning(&default_layers, &plan, true, HostPlatform::NonLinux),
            None
        );
        assert!(
            uid_gid_sync_warning(&explicit_true, &plan, true, HostPlatform::NonLinux)
                .is_some_and(|warning| warning.contains("skipping updateRemoteUserUID"))
        );
        assert_eq!(
            uid_gid_sync_warning(&explicit_false, &plan, false, HostPlatform::NonLinux),
            None
        );
    }

    #[test]
    fn uid_gid_sync_warning_reports_numeric_user_without_passwd_noop() {
        let warning = uid_gid_sync_warning(
            &ConfigMergeInput::default(),
            &UidGidSyncPlan::Noop {
                reason: UidGidSyncNoopReason::NumericUserWithoutPasswd,
            },
            true,
            HostPlatform::Linux,
        )
        .expect("numeric no-passwd sync no-op must be user-visible");

        assert!(warning.contains("numeric user has no passwd entry"));
    }

    #[test]
    fn up_detach_reports_missing_explicit_uid_gid_sync_target_user() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-uid-gid-sync-missing-target-user");
            write_devcontainer(
                &workspace,
                r#"
                {
                  "image": "alpine:3.20",
                  "remoteUser": "missing-sync-user"
                }
                "#,
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                ensure_image(&client, "alpine:3.20", PullPolicy::Missing).await?;
                remove_container(&client, &container_name, true, true).await?;

                let error = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await
                .unwrap_err();
                let message = format!("{error:#}");
                assert!(message.contains("Remote user does not exist in container"));
                assert!(message.contains("missing-sync-user"));

                let containers = list_workspace_containers(&client, workspace.id()).await?;
                assert!(
                    !containers
                        .iter()
                        .any(|container| container.name == container_name)
                );

                Ok(())
            }
            .await;

            let cleanup = remove_container(&client, &container_name, true, true).await;
            result.and(cleanup).unwrap();
        });
    }

    #[cfg(unix)]
    #[test]
    fn up_detach_fails_uid_gid_sync_when_host_ids_conflict() {
        if HostPlatform::current() != HostPlatform::Linux {
            return;
        }
        let host = current_host_user_ids();
        if host.uid == 0 || host.gid == 0 {
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-uid-gid-sync-conflict");
            let image = format!(
                "decune-test/uid-gid-sync-conflict-{}:latest",
                workspace.id()
            );
            write_devcontainer(
                &workspace,
                &format!(
                    r#"
                    {{
                      "image": "{image}",
                      "remoteUser": "syncuser"
                    }}
                    "#
                ),
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;
                build_uid_gid_conflict_user_image(&client, &image, host.uid, host.gid).await?;

                let error = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await
                .unwrap_err();
                let message = format!("{error:#}");
                assert!(
                    message.contains("Failed to build Docker image")
                        && message.contains("sync-uid-gid.sh"),
                    "{message}"
                );

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }

    #[cfg(unix)]
    #[test]
    fn up_detach_fails_uid_gid_sync_when_host_ids_already_match_but_duplicates_exist() {
        if HostPlatform::current() != HostPlatform::Linux {
            return;
        }
        let host = current_host_user_ids();
        if host.uid == 0 || host.gid == 0 {
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-uid-gid-sync-duplicate-matching-ids");
            let image = format!(
                "decune-test/uid-gid-sync-duplicate-matching-ids-{}:latest",
                workspace.id()
            );
            write_devcontainer(
                &workspace,
                &format!(
                    r#"
                    {{
                      "image": "{image}",
                      "remoteUser": "syncuser"
                    }}
                    "#
                ),
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;
                build_duplicate_matching_host_ids_image(&client, &image, host.uid, host.gid)
                    .await?;

                let error = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await
                .unwrap_err();
                let message = format!("{error:#}");
                assert!(
                    message.contains("Failed to build Docker image")
                        && message.contains("sync-uid-gid.sh"),
                    "{message}"
                );

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }

    #[cfg(unix)]
    #[test]
    fn up_detach_fails_uid_gid_sync_gid_conflict_without_target_group_entry() {
        if HostPlatform::current() != HostPlatform::Linux {
            return;
        }
        let host = current_host_user_ids();
        if host.uid == 0 || host.gid == 0 {
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-uid-gid-sync-missing-target-group");
            let image = format!(
                "decune-test/uid-gid-sync-missing-target-group-{}:latest",
                workspace.id()
            );
            write_devcontainer(
                &workspace,
                &format!(
                    r#"
                    {{
                      "image": "{image}",
                      "remoteUser": "syncuser"
                    }}
                    "#
                ),
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;
                build_missing_target_group_conflict_image(&client, &image, host.gid).await?;

                let error = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await
                .unwrap_err();
                let message = format!("{error:#}");
                assert!(
                    message.contains("Failed to build Docker image")
                        && message.contains("sync-uid-gid.sh"),
                    "{message}"
                );

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }

    #[cfg(unix)]
    #[test]
    fn up_detach_fails_uid_gid_sync_when_old_gid_matches_multiple_groups() {
        if HostPlatform::current() != HostPlatform::Linux {
            return;
        }
        let host = current_host_user_ids();
        if host.uid == 0 || host.gid == 0 || host.gid == 2001 {
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-uid-gid-sync-duplicate-old-gid");
            let image = format!(
                "decune-test/uid-gid-sync-duplicate-old-gid-{}:latest",
                workspace.id()
            );
            write_devcontainer(
                &workspace,
                &format!(
                    r#"
                    {{
                      "image": "{image}",
                      "remoteUser": "syncuser"
                    }}
                    "#
                ),
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;
                build_duplicate_old_gid_image(&client, &image).await?;

                let error = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await
                .unwrap_err();
                let message = format!("{error:#}");
                assert!(
                    message.contains("Failed to build Docker image")
                        && message.contains("sync-uid-gid.sh"),
                    "{message}"
                );

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }

    #[test]
    fn up_detach_stops_lifecycle_after_failure() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-lifecycle-failure");
            write_devcontainer(
                &workspace,
                r#"
                {
                  "image": "alpine:3.20",
                  "onCreateCommand": "printf on-create >/tmp/decune-lifecycle; exit 7",
                  "updateContentCommand": "printf update-content >>/tmp/decune-lifecycle"
                }
                "#,
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;

                let error = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await
                .unwrap_err();
                let message = format!("{error:#}");
                assert!(message.contains("Lifecycle stage onCreateCommand failed"));
                assert!(message.contains("exit code 7"));

                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "cat /tmp/decune-lifecycle".to_owned(),
                        ],
                        user: None,
                        working_dir: None,
                        env: BTreeMap::new(),
                        redactions: Vec::new(),
                        tty: false,
                    },
                )
                .await?;

                assert_eq!(String::from_utf8(output.stdout).unwrap(), "on-create");

                Ok(())
            }
            .await;

            let cleanup = remove_container(&client, &container_name, true, true).await;
            result.and(cleanup).unwrap();
        });
    }

    #[test]
    fn up_detach_waits_for_parallel_post_start_siblings() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-parallel-lifecycle-failure");
            write_devcontainer(
                &workspace,
                r#"
                {
                  "image": "alpine:3.20",
                  "postStartCommand": {
                    "a_slow": "sleep 1; printf done >/tmp/decune-parallel-lifecycle",
                    "z_fail": "exit 7"
                  }
                }
                "#,
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;

                let error = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await
                .unwrap_err();
                let message = format!("{error:#}");
                assert!(message.contains("Lifecycle stage postStartCommand.z_fail failed"));
                assert!(message.contains("exit code 7"));

                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "cat /tmp/decune-parallel-lifecycle".to_owned(),
                        ],
                        user: None,
                        working_dir: None,
                        env: BTreeMap::new(),
                        redactions: Vec::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(String::from_utf8(output.stdout).unwrap(), "done");

                Ok(())
            }
            .await;

            let cleanup = remove_container(&client, &container_name, true, true).await;
            result.and(cleanup).unwrap();
        });
    }

    #[test]
    fn up_detach_does_not_run_post_attach() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-detach-no-post-attach");
            write_devcontainer(
                &workspace,
                r#"
                {
                  "image": "alpine:3.20",
                  "postStartCommand": "printf post-start >/tmp/decune-post-start",
                  "postAttachCommand": "printf post-attach >/tmp/decune-post-attach"
                }
                "#,
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;

                run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;

                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "test -f /tmp/decune-post-start && test ! -e /tmp/decune-post-attach && cat /tmp/decune-post-start".to_owned(),
                        ],
                        user: None,
                        working_dir: None,
                        env: BTreeMap::new(),
            redactions: Vec::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(String::from_utf8(output.stdout).unwrap(), "post-start");

                Ok(())
            }
            .await;

            let cleanup = remove_container(&client, &container_name, true, true).await;
            result.and(cleanup).unwrap();
        });
    }

    #[test]
    fn up_attached_runs_post_attach_before_shell() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-attached-post-attach-before-shell");
            fs::create_dir_all(workspace.root().join(".devcontainer")).unwrap();
            fs::write(
                workspace.root().join(".devcontainer/Dockerfile"),
                r#"
                FROM alpine:3.20
                RUN printf '%s\n' \
                  '#!/bin/sh' \
                  'test -f /tmp/decune-post-attach-before-shell || exit 9' \
                  'exit 0' \
                  >/usr/local/bin/decune-shell-check \
                  && chmod +x /usr/local/bin/decune-shell-check
                "#,
            )
            .unwrap();
            write_devcontainer(
                &workspace,
                r#"
                {
                  "build": {
                    "dockerfile": "Dockerfile"
                  },
                  "postAttachCommand": "printf ready >/tmp/decune-post-attach-before-shell"
                }
                "#,
            );
            fs::create_dir_all(workspace.root().join(".decune")).unwrap();
            fs::write(
                workspace.root().join(".decune/config.toml"),
                r#"
version = 1
shell = "/usr/local/bin/decune-shell-check"
"#,
            )
            .unwrap();
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let image = plan.image.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;

                let exit_code = run_attached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                assert_eq!(exit_code, 0);

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }

    #[test]
    fn up_attached_runs_post_attach_each_attach_before_shutdown() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-attached-post-attach-each-attach");
            fs::create_dir_all(workspace.root().join(".devcontainer")).unwrap();
            fs::write(
                workspace.root().join(".devcontainer/Dockerfile"),
                r#"
                FROM alpine:3.20
                RUN printf '%s\n' '#!/bin/sh' 'exit 0' >/usr/local/bin/decune-exit-0 \
                  && chmod +x /usr/local/bin/decune-exit-0
                "#,
            )
            .unwrap();
            write_devcontainer(
                &workspace,
                r#"
                {
                  "build": {
                    "dockerfile": "Dockerfile"
                  },
                  "postAttachCommand": "count_file=.decune-post-attach-count; count=0; if [ -f \"$count_file\" ]; then count=$(cat \"$count_file\"); fi; count=$((count + 1)); printf '%s' \"$count\" >\"$count_file\""
                }
                "#,
            );
            fs::create_dir_all(workspace.root().join(".decune")).unwrap();
            fs::write(
                workspace.root().join(".decune/config.toml"),
                r#"
version = 1
shell = "/usr/local/bin/decune-exit-0"
"#,
            )
            .unwrap();
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let image = plan.image.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;

                for _ in 0..2 {
                    let exit_code = run_attached_up(UpOptions {
                        workspace: workspace.root().to_path_buf(),
                        config_path: None,
                        skip_global_config: false,
                        cli_layer: ConfigLayer::default(),
                        pull: false,
                        rebuild: false,
                        no_cache: false,
                    update_features: false,
                    })
                    .await?;
                    assert_eq!(exit_code, 0);
                }

                assert_eq!(
                    fs::read_to_string(workspace.root().join(".decune-post-attach-count"))
                        .unwrap(),
                    "2"
                );

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }

    #[test]
    fn up_running_attached_runs_post_attach_each_attach_when_shutdown_action_none() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-running-post-attach-each-attach");
            fs::create_dir_all(workspace.root().join(".devcontainer")).unwrap();
            fs::write(
                workspace.root().join(".devcontainer/Dockerfile"),
                r#"
                FROM alpine:3.20
                RUN printf '%s\n' '#!/bin/sh' 'exit 0' >/usr/local/bin/decune-exit-0 \
                  && chmod +x /usr/local/bin/decune-exit-0
                "#,
            )
            .unwrap();
            write_devcontainer(
                &workspace,
                r#"
                {
                  "build": {
                    "dockerfile": "Dockerfile"
                  },
                  "postAttachCommand": "count=0; if [ -f /tmp/decune-post-attach-count ]; then count=$(cat /tmp/decune-post-attach-count); fi; count=$((count + 1)); printf '%s' \"$count\" >/tmp/decune-post-attach-count",
                  "shutdownAction": "none"
                }
                "#,
            );
            fs::create_dir_all(workspace.root().join(".decune")).unwrap();
            fs::write(
                workspace.root().join(".decune/config.toml"),
                r#"
version = 1
shell = "/usr/local/bin/decune-exit-0"
"#,
            )
            .unwrap();
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let image = plan.image.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;

                for _ in 0..2 {
                    let exit_code = run_attached_up(UpOptions {
                        workspace: workspace.root().to_path_buf(),
                        config_path: None,
                        skip_global_config: false,
                        cli_layer: ConfigLayer::default(),
                        pull: false,
                        rebuild: false,
                        no_cache: false,
                        update_features: false,
                    })
                    .await?;
                    assert_eq!(exit_code, 0);
                }

                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "cat /tmp/decune-post-attach-count".to_owned(),
                        ],
                        user: None,
                        working_dir: None,
                        env: BTreeMap::new(),
            redactions: Vec::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(String::from_utf8(output.stdout).unwrap(), "2");

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }

    #[test]
    fn up_attached_stopped_runs_start_attach_shell() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-stopped-attached-lifecycle");
            fs::create_dir_all(workspace.root().join(".devcontainer")).unwrap();
            fs::write(
                workspace.root().join(".devcontainer/Dockerfile"),
                r#"
                FROM alpine:3.20
                RUN printf '%s\n' \
                  '#!/bin/sh' \
                  'test "$(cat /tmp/decune-stopped-attach-matrix)" = "ssa" || exit 9' \
                  'exit 0' \
                  >/usr/local/bin/decune-shell-check \
                  && chmod +x /usr/local/bin/decune-shell-check
                "#,
            )
            .unwrap();
            write_devcontainer(
                &workspace,
                r#"
                {
                  "build": {
                    "dockerfile": "Dockerfile"
                  },
                  "postStartCommand": "printf s >>/tmp/decune-stopped-attach-matrix",
                  "postAttachCommand": "printf a >>/tmp/decune-stopped-attach-matrix"
                }
                "#,
            );
            fs::create_dir_all(workspace.root().join(".decune")).unwrap();
            fs::write(
                workspace.root().join(".decune/config.toml"),
                r#"
version = 1
shell = "/usr/local/bin/decune-shell-check"
"#,
            )
            .unwrap();
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let image = plan.image.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;

                run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                stop_container(&client, &container_name, 10).await?;

                let exit_code = run_attached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                assert_eq!(exit_code, 0);

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }

    #[test]
    fn rebuild_detach_recreates_without_post_attach() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-rebuild-detach-no-post-attach");
            write_devcontainer(
                &workspace,
                r#"
                {
                  "image": "alpine:3.20",
                  "postStartCommand": "printf post-start >/tmp/decune-rebuild-post-start",
                  "postAttachCommand": "printf post-attach >/tmp/decune-rebuild-post-attach"
                }
                "#,
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;

                let first = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                assert!(!first.reused);

                let second = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: true,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                assert!(!second.reused);
                assert_ne!(first.container_id, second.container_id);

                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "test -f /tmp/decune-rebuild-post-start && test ! -e /tmp/decune-rebuild-post-attach && cat /tmp/decune-rebuild-post-start".to_owned(),
                        ],
                        user: None,
                        working_dir: None,
                        env: BTreeMap::new(),
            redactions: Vec::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(String::from_utf8(output.stdout).unwrap(), "post-start");

                Ok(())
            }
            .await;

            let cleanup = remove_container(&client, &container_name, true, true).await;
            result.and(cleanup).unwrap();
        });
    }

    #[test]
    fn rebuild_attached_recreates_runs_post_attach_before_shell() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-rebuild-attached-post-attach");
            fs::create_dir_all(workspace.root().join(".devcontainer")).unwrap();
            fs::write(
                workspace.root().join(".devcontainer/Dockerfile"),
                r#"
                FROM alpine:3.20
                RUN printf '%s\n' \
                  '#!/bin/sh' \
                  'test -f /tmp/decune-rebuild-post-attach-before-shell || exit 9' \
                  'exit 0' \
                  >/usr/local/bin/decune-shell-check \
                  && chmod +x /usr/local/bin/decune-shell-check
                "#,
            )
            .unwrap();
            write_devcontainer(
                &workspace,
                r#"
                {
                  "build": {
                    "dockerfile": "Dockerfile"
                  },
                  "postAttachCommand": "printf ready >/tmp/decune-rebuild-post-attach-before-shell"
                }
                "#,
            );
            fs::create_dir_all(workspace.root().join(".decune")).unwrap();
            fs::write(
                workspace.root().join(".decune/config.toml"),
                r#"
version = 1
shell = "/usr/local/bin/decune-shell-check"
"#,
            )
            .unwrap();
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let image = plan.image.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;

                let first = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                assert!(!first.reused);

                let exit_code = run_attached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: true,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                assert_eq!(exit_code, 0);

                let inspect = client.cli().inspect_container(&container_name).await?;
                let rebuilt_container_id = inspect
                    .id
                    .context("Docker inspect response did not include container id")?;
                assert_ne!(first.container_id, rebuilt_container_id);

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }

    #[test]
    fn up_detach_applies_remote_env_to_lifecycle() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-lifecycle-remote-env");
            write_devcontainer(
                &workspace,
                r#"
                {
                  "image": "alpine:3.20",
                  "remoteEnv": {
                    "DECUNE_REMOTE_ENV_SENTINEL": "from-remote-env"
                  },
                  "postStartCommand": "test \"$DECUNE_REMOTE_ENV_SENTINEL\" = from-remote-env && printf '%s' \"$DECUNE_REMOTE_ENV_SENTINEL\" >/tmp/decune-remote-env"
                }
                "#,
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;

                run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;

                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "cat /tmp/decune-remote-env".to_owned(),
                        ],
                        user: None,
                        working_dir: None,
                        env: BTreeMap::new(),
            redactions: Vec::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(String::from_utf8(output.stdout).unwrap(), "from-remote-env");

                Ok(())
            }
            .await;

            let cleanup = remove_container(&client, &container_name, true, true).await;
            result.and(cleanup).unwrap();
        });
    }

    #[test]
    fn up_detach_applies_user_env_probe_to_lifecycle() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-lifecycle-user-env-probe");
            fs::create_dir_all(workspace.root().join(".devcontainer")).unwrap();
            fs::write(
                workspace.root().join(".devcontainer/Dockerfile"),
                r#"
                FROM alpine:3.20
                RUN printf '%s\n' \
                  'export DECUNE_PROBED_ENV=from-profile' \
                  'export DECUNE_ENV_PRIORITY=from-profile' \
                  >/etc/profile.d/decune-probe.sh
                "#,
            )
            .unwrap();
            write_devcontainer(
                &workspace,
                r#"
                {
                  "build": {
                    "dockerfile": "Dockerfile"
                  },
                  "userEnvProbe": "loginShell",
                  "remoteEnv": {
                    "DECUNE_ENV_PRIORITY": "from-remote-env"
                  },
                  "postStartCommand": [
                    "/bin/sh",
                    "-c",
                    "test \"$DECUNE_PROBED_ENV\" = from-profile && test \"$DECUNE_ENV_PRIORITY\" = from-remote-env && printf '%s:%s' \"$DECUNE_PROBED_ENV\" \"$DECUNE_ENV_PRIORITY\" >/tmp/decune-user-env-probe"
                  ]
                }
                "#,
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let image = plan.image.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;

                run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;

                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "cat /tmp/decune-user-env-probe".to_owned(),
                        ],
                        user: None,
                        working_dir: None,
                        env: BTreeMap::new(),
            redactions: Vec::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(
                    String::from_utf8(output.stdout).unwrap(),
                    "from-profile:from-remote-env"
                );

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }

    #[test]
    fn up_detach_omits_remote_probe_env_for_root_post_start_hook() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-root-hook-user-env-probe");
            fs::create_dir_all(workspace.root().join(".devcontainer")).unwrap();
            fs::write(
                workspace.root().join(".devcontainer/Dockerfile"),
                r#"
                FROM alpine:3.20
                RUN printf '%s\n' \
                  '#!/bin/sh' \
                  'export DECUNE_REMOTE_ONLY=from-decune' \
                  'exec /bin/sh "$@"' \
                  >/usr/local/bin/decune-probe-shell \
                  && chmod +x /usr/local/bin/decune-probe-shell \
                  && adduser -D -s /usr/local/bin/decune-probe-shell decune
                "#,
            )
            .unwrap();
            write_devcontainer(
                &workspace,
                r#"
                {
                  "build": {
                    "dockerfile": "Dockerfile"
                  },
                  "remoteUser": "decune",
                  "userEnvProbe": "loginShell",
                  "remoteEnv": {
                    "DECUNE_REMOTE_ENV": "from-remote-env"
                  }
                }
                "#,
            );
            fs::create_dir_all(workspace.root().join(".decune")).unwrap();
            fs::write(
                workspace.root().join(".decune/config.toml"),
                r#"
version = 1

[[hooks.before_post_start]]
command = "test -z \"${DECUNE_REMOTE_ONLY+x}\" && test \"$DECUNE_REMOTE_ENV\" = from-remote-env && printf '%s' root-hook-clean >/tmp/decune-root-hook-env"
user = "root"
"#,
            )
            .unwrap();
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let image = plan.image.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;

                run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;

                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "cat /tmp/decune-root-hook-env".to_owned(),
                        ],
                        user: None,
                        working_dir: None,
                        env: BTreeMap::new(),
            redactions: Vec::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(String::from_utf8(output.stdout).unwrap(), "root-hook-clean");

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }

    #[test]
    fn up_detach_probes_env_with_remote_user_shell() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-lifecycle-user-env-probe-login-shell");
            fs::create_dir_all(workspace.root().join(".devcontainer")).unwrap();
            fs::write(
                workspace.root().join(".devcontainer/Dockerfile"),
                r#"
                FROM alpine:3.20
                RUN printf '%s\n' \
                  '#!/bin/sh' \
                  'export DECUNE_LOGIN_SHELL_ENV=from-login-shell' \
                  'exec /bin/sh "$@"' \
                  >/usr/local/bin/decune-probe-shell \
                  && chmod +x /usr/local/bin/decune-probe-shell \
                  && adduser -D -s /usr/local/bin/decune-probe-shell decune
                "#,
            )
            .unwrap();
            write_devcontainer(
                &workspace,
                r#"
                {
                  "build": {
                    "dockerfile": "Dockerfile"
                  },
                  "remoteUser": "decune",
                  "userEnvProbe": "loginShell",
                  "postStartCommand": [
                    "/bin/sh",
                    "-c",
                    "test \"$DECUNE_LOGIN_SHELL_ENV\" = from-login-shell && printf '%s' \"$DECUNE_LOGIN_SHELL_ENV\" >/tmp/decune-login-shell-env-probe"
                  ]
                }
                "#,
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let image = plan.image.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;

                run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;

                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "cat /tmp/decune-login-shell-env-probe".to_owned(),
                        ],
                        user: None,
                        working_dir: None,
                        env: BTreeMap::new(),
            redactions: Vec::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(String::from_utf8(output.stdout).unwrap(), "from-login-shell");

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }

    #[test]
    fn up_detach_removes_new_container_when_start_fails() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-start-failure-cleanup");
            write_devcontainer(
                &workspace,
                r#"
                {
                  "image": "alpine:3.20",
                  "containerUser": "decune-missing-user"
                }
                "#,
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;

                let error = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await
                .unwrap_err();
                assert!(
                    error
                        .to_string()
                        .contains("Remote user does not exist in container")
                );
                assert!(error.to_string().contains("decune-missing-user"));

                let containers = list_workspace_containers(&client, workspace.id()).await?;
                assert!(
                    !containers
                        .iter()
                        .any(|container| container.name == container_name)
                );

                Ok(())
            }
            .await;

            let cleanup = remove_container(&client, &container_name, true, true).await;
            result.and(cleanup).unwrap();
        });
    }

    struct TestWorkspace {
        _directory: tempfile::TempDir,
        workspace: Workspace,
    }

    impl Deref for TestWorkspace {
        type Target = Workspace;

        fn deref(&self) -> &Self::Target {
            &self.workspace
        }
    }

    fn test_workspace(name: &str) -> TestWorkspace {
        let directory = tempfile::Builder::new()
            .prefix(&format!("decune-up-test-{name}-"))
            .tempdir()
            .unwrap();
        let root = directory.path().join(name);
        fs::create_dir_all(&root).unwrap();
        let workspace = Workspace::resolve(&root).unwrap();
        TestWorkspace {
            _directory: directory,
            workspace,
        }
    }

    fn write_devcontainer(workspace: &Workspace, contents: &str) {
        let path = workspace.root().join(".devcontainer/devcontainer.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn test_mount() -> DockerMountSpec {
        DockerMountSpec {
            source: Some("/host/cache".to_owned()),
            target: "/cache".to_owned(),
            mount_type: MountType::Bind,
            read_only: false,
            consistency: None,
            bind_options: None,
            volume_options: None,
        }
    }

    fn test_volume_mount() -> DockerMountSpec {
        DockerMountSpec {
            source: Some("project-cache".to_owned()),
            target: "/cache".to_owned(),
            mount_type: MountType::Volume,
            read_only: false,
            consistency: None,
            bind_options: None,
            volume_options: None,
        }
    }

    fn config_hash_for_mount(mount: DockerMountSpec) -> String {
        let config = ResolvedConfig::default();
        let mut input = ConfigHashInput::new(&config);
        input.resolved_mounts = mount_hash_inputs(&[mount]);

        config_hash(&input)
    }

    async fn build_user_image(
        client: &DockerClient,
        image: &str,
        user: &str,
    ) -> anyhow::Result<()> {
        build_uid_gid_user_image(client, image, user, 2001, 2001).await
    }

    async fn build_uid_gid_user_image(
        client: &DockerClient,
        image: &str,
        user: &str,
        uid: u32,
        gid: u32,
    ) -> anyhow::Result<()> {
        let context = tempfile::Builder::new()
            .prefix("decune-up-user-image-")
            .tempdir()
            .unwrap();
        let dockerfile_path = context.path().join("Dockerfile");
        fs::write(
            &dockerfile_path,
            format!(
                "FROM alpine:3.20\nRUN addgroup -g {gid} {user} && adduser -D -u {uid} -G {user} -h /home/{user} {user}\nUSER {user}\n"
            ),
        )
        .unwrap();
        build_image(
            client,
            DockerBuildInput {
                image_tag: image.to_owned(),
                labels: std::collections::HashMap::new(),
                context: ResolvedBuildContext {
                    context_dir: context.path().to_path_buf(),
                    dockerfile_path,
                    dockerfile_in_context: "Dockerfile".into(),
                    dockerignore_path: None,
                },
                options: DockerBuildOptions::default(),
            },
        )
        .await
    }

    async fn build_distinct_uid_gid_users_image(
        client: &DockerClient,
        image: &str,
    ) -> anyhow::Result<()> {
        let context = tempfile::Builder::new()
            .prefix("decune-up-distinct-users-image-")
            .tempdir()
            .unwrap();
        let dockerfile_path = context.path().join("Dockerfile");
        fs::write(
            &dockerfile_path,
            "FROM alpine:3.20\nRUN addgroup -g 2001 remoteuser && adduser -D -u 2001 -G remoteuser -h /home/remoteuser remoteuser && addgroup -g 2002 containeruser && adduser -D -u 2002 -G containeruser -h /home/containeruser containeruser\nUSER containeruser\n",
        )
        .unwrap();
        build_image(
            client,
            DockerBuildInput {
                image_tag: image.to_owned(),
                labels: std::collections::HashMap::new(),
                context: ResolvedBuildContext {
                    context_dir: context.path().to_path_buf(),
                    dockerfile_path,
                    dockerfile_in_context: "Dockerfile".into(),
                    dockerignore_path: None,
                },
                options: DockerBuildOptions::default(),
            },
        )
        .await
    }

    async fn build_numeric_uid_gid_user_image(
        client: &DockerClient,
        image: &str,
    ) -> anyhow::Result<()> {
        let context = tempfile::Builder::new()
            .prefix("decune-up-numeric-user-image-")
            .tempdir()
            .unwrap();
        let dockerfile_path = context.path().join("Dockerfile");
        fs::write(
            &dockerfile_path,
            "FROM alpine:3.20\nRUN addgroup -g 2001 syncuser && adduser -D -u 2001 -G syncuser -h /home/syncuser syncuser\nUSER 2001:2001\n",
        )
        .unwrap();
        build_image(
            client,
            DockerBuildInput {
                image_tag: image.to_owned(),
                labels: std::collections::HashMap::new(),
                context: ResolvedBuildContext {
                    context_dir: context.path().to_path_buf(),
                    dockerfile_path,
                    dockerfile_in_context: "Dockerfile".into(),
                    dockerignore_path: None,
                },
                options: DockerBuildOptions::default(),
            },
        )
        .await
    }

    async fn build_named_uid_numeric_gid_user_image(
        client: &DockerClient,
        image: &str,
    ) -> anyhow::Result<()> {
        let context = tempfile::Builder::new()
            .prefix("decune-up-named-user-numeric-group-image-")
            .tempdir()
            .unwrap();
        let dockerfile_path = context.path().join("Dockerfile");
        fs::write(
            &dockerfile_path,
            "FROM alpine:3.20\nRUN addgroup -g 2001 syncuser && adduser -D -u 2001 -G syncuser -h /home/syncuser syncuser\nUSER syncuser:2001\n",
        )
        .unwrap();
        build_image(
            client,
            DockerBuildInput {
                image_tag: image.to_owned(),
                labels: std::collections::HashMap::new(),
                context: ResolvedBuildContext {
                    context_dir: context.path().to_path_buf(),
                    dockerfile_path,
                    dockerfile_in_context: "Dockerfile".into(),
                    dockerignore_path: None,
                },
                options: DockerBuildOptions::default(),
            },
        )
        .await
    }

    async fn build_uid_gid_conflict_user_image(
        client: &DockerClient,
        image: &str,
        conflict_uid: u32,
        conflict_gid: u32,
    ) -> anyhow::Result<()> {
        let context = tempfile::Builder::new()
            .prefix("decune-up-user-image-")
            .tempdir()
            .unwrap();
        let dockerfile_path = context.path().join("Dockerfile");
        fs::write(
            &dockerfile_path,
            format!(
                "FROM alpine:3.20\nRUN addgroup -g {conflict_gid} conflictuser && adduser -D -u {conflict_uid} -G conflictuser -h /home/conflictuser conflictuser && addgroup -g 2001 syncuser && adduser -D -u 2001 -G syncuser -h /home/syncuser syncuser\nUSER syncuser\n"
            ),
        )
        .unwrap();
        build_image(
            client,
            DockerBuildInput {
                image_tag: image.to_owned(),
                labels: std::collections::HashMap::new(),
                context: ResolvedBuildContext {
                    context_dir: context.path().to_path_buf(),
                    dockerfile_path,
                    dockerfile_in_context: "Dockerfile".into(),
                    dockerignore_path: None,
                },
                options: DockerBuildOptions::default(),
            },
        )
        .await
    }

    async fn build_duplicate_matching_host_ids_image(
        client: &DockerClient,
        image: &str,
        host_uid: u32,
        host_gid: u32,
    ) -> anyhow::Result<()> {
        let context = tempfile::Builder::new()
            .prefix("decune-up-duplicate-matching-ids-image-")
            .tempdir()
            .unwrap();
        let dockerfile_path = context.path().join("Dockerfile");
        fs::write(
            &dockerfile_path,
            format!(
                "FROM alpine:3.20\nRUN addgroup -g {host_gid} syncgroup && adduser -D -u {host_uid} -G syncgroup -h /home/syncuser syncuser && echo 'other:x:{host_uid}:{host_gid}::/home/other:/bin/sh' >> /etc/passwd && echo 'othergroup:x:{host_gid}:' >> /etc/group\nUSER syncuser\n"
            ),
        )
        .unwrap();
        build_image(
            client,
            DockerBuildInput {
                image_tag: image.to_owned(),
                labels: std::collections::HashMap::new(),
                context: ResolvedBuildContext {
                    context_dir: context.path().to_path_buf(),
                    dockerfile_path,
                    dockerfile_in_context: "Dockerfile".into(),
                    dockerignore_path: None,
                },
                options: DockerBuildOptions::default(),
            },
        )
        .await
    }

    async fn build_missing_target_group_conflict_image(
        client: &DockerClient,
        image: &str,
        conflict_gid: u32,
    ) -> anyhow::Result<()> {
        let context = tempfile::Builder::new()
            .prefix("decune-up-missing-group-image-")
            .tempdir()
            .unwrap();
        let dockerfile_path = context.path().join("Dockerfile");
        fs::write(
            &dockerfile_path,
            format!(
                "FROM alpine:3.20\nRUN addgroup -g {conflict_gid} conflictgroup && addgroup -g 2001 syncuser && adduser -D -u 2001 -G syncuser -h /home/syncuser syncuser && awk -F: '$3 != 2001 {{ print }}' /etc/group >/tmp/group && cat /tmp/group >/etc/group\nUSER syncuser\n"
            ),
        )
        .unwrap();
        build_image(
            client,
            DockerBuildInput {
                image_tag: image.to_owned(),
                labels: std::collections::HashMap::new(),
                context: ResolvedBuildContext {
                    context_dir: context.path().to_path_buf(),
                    dockerfile_path,
                    dockerfile_in_context: "Dockerfile".into(),
                    dockerignore_path: None,
                },
                options: DockerBuildOptions::default(),
            },
        )
        .await
    }

    async fn build_duplicate_old_gid_image(
        client: &DockerClient,
        image: &str,
    ) -> anyhow::Result<()> {
        let context = tempfile::Builder::new()
            .prefix("decune-up-duplicate-old-gid-image-")
            .tempdir()
            .unwrap();
        let dockerfile_path = context.path().join("Dockerfile");
        fs::write(
            &dockerfile_path,
            "FROM alpine:3.20\nRUN addgroup -g 2001 syncgroup && adduser -D -u 2001 -G syncgroup -h /home/syncuser syncuser && { echo 'othergroup:x:2001:'; cat /etc/group; } >/tmp/group && cat /tmp/group >/etc/group\nUSER syncuser\n",
        )
        .unwrap();
        build_image(
            client,
            DockerBuildInput {
                image_tag: image.to_owned(),
                labels: std::collections::HashMap::new(),
                context: ResolvedBuildContext {
                    context_dir: context.path().to_path_buf(),
                    dockerfile_path,
                    dockerfile_in_context: "Dockerfile".into(),
                    dockerignore_path: None,
                },
                options: DockerBuildOptions::default(),
            },
        )
        .await
    }

    fn mount_summary(source: Option<&str>, target: &str) -> UpMountSummary {
        mount_summary_with_type(source, target, MountType::Bind)
    }

    fn mount_policy(required_mounts: &[UpMountSummary]) -> CredentialRuntimeMountPolicy {
        CredentialRuntimeMountPolicy::new(required_mounts.to_vec())
    }

    fn mount_summary_with_type(
        source: Option<&str>,
        target: &str,
        mount_type: MountType,
    ) -> UpMountSummary {
        mount_summary_with_type_and_read_only(source, target, mount_type, false)
    }

    fn mount_summary_with_type_and_read_only(
        source: Option<&str>,
        target: &str,
        mount_type: MountType,
        read_only: bool,
    ) -> UpMountSummary {
        UpMountSummary {
            source: source.map(ToOwned::to_owned),
            target: target.to_owned(),
            mount_type,
            read_only,
        }
    }

    #[test]
    fn feature_layer_image_uses_pre_uid_gid_sync_resources_when_sync_layer_is_needed() {
        let mut config = ResolvedConfig::default();
        config.devcontainer.entrypoints = vec!["/usr/local/share/decune/feature.sh".to_owned()];
        let mut plan = test_up_plan_with_config(config);
        plan.image = "decune/test:final-sync-hash".to_owned();
        plan.resources.image_tag = plan.image.clone();
        plan.resources.config_hash = "final-sync-hash".to_owned();
        plan.pre_uid_gid_sync_resources = Some(test_resources("pre-sync-hash"));
        plan.base_image = "alpine:3.20".to_owned();
        plan.uid_gid_sync_plan = sync_plan();

        assert_eq!(feature_layer_image(&plan), "decune/test:pre-sync-hash");
        assert_eq!(uid_gid_sync_base_image(&plan), "decune/test:pre-sync-hash");
    }

    #[test]
    fn dockerfile_uid_gid_sync_base_uses_resolved_base_image_without_workspace_layer() {
        let mut config = ResolvedConfig::default();
        config.devcontainer.source = Some(ResolvedDevcontainerSource::Dockerfile(
            crate::config::layer::LayerDevcontainerBuild {
                dockerfile: "Dockerfile".to_owned(),
                context: Some(".".to_owned()),
                args: BTreeMap::new(),
                options: Vec::new(),
                target: None,
                cache_from: Vec::new(),
            },
        ));
        let mut plan = test_up_plan_with_config(config);
        plan.image = "decune/test:final-sync-hash".to_owned();
        plan.base_image = "decune/test:pre-sync-hash".to_owned();
        plan.resources.image_tag = plan.image.clone();
        plan.resources.config_hash = "final-sync-hash".to_owned();
        plan.pre_uid_gid_sync_resources = Some(test_resources("pre-sync-hash"));
        plan.uid_gid_sync_plan = sync_plan();

        assert_eq!(uid_gid_sync_base_image(&plan), "decune/test:pre-sync-hash");
    }

    #[test]
    fn image_uid_gid_sync_base_uses_original_image_without_workspace_layer() {
        let mut plan = test_up_plan_with_image_source("alpine:3.20");
        plan.image = "decune/test:final-sync-hash".to_owned();
        plan.resources.image_tag = plan.image.clone();
        plan.resources.config_hash = "final-sync-hash".to_owned();
        plan.pre_uid_gid_sync_resources = Some(test_resources("pre-sync-hash"));
        plan.uid_gid_sync_plan = sync_plan();

        assert_eq!(uid_gid_sync_base_image(&plan), "alpine:3.20");
    }

    fn test_up_plan_with_config(config: ResolvedConfig) -> UpPlan {
        UpPlan {
            image: "alpine:3.20".to_owned(),
            base_image: "alpine:3.20".to_owned(),
            build_context: None,
            build_options: crate::docker::build::DockerBuildOptions::default(),
            feature_install: None,
            feature_build_context_dir: None,
            uid_gid_sync_build_context_dir: None,
            resources: DockerResources {
                container_name: "decune-test".to_owned(),
                image_tag: "decune/test:stable-hash".to_owned(),
                workspace_volume_name: "decune-test-workspace".to_owned(),
                labels: BTreeMap::new(),
                config_hash: "stable-hash".to_owned(),
            },
            pre_uid_gid_sync_resources: None,
            compose_project: None,
            config_layers: ConfigMergeInput::default(),
            config,
            sensitive_container_env: Default::default(),
            sensitive_build_args: Default::default(),
            compose_interpolation_env: Default::default(),
            compose_interpolation_redactions: Vec::new(),
            effective_users: EffectiveUsers::root(),
            uid_gid_sync_plan: UidGidSyncPlan::default(),
            workspace_folder: "/workspaces/project".to_owned(),
            mounts: Vec::new(),
            dotfile_skeletons: Vec::new(),
            forward_ports: Vec::new(),
            ignored_detached_forwarding: false,
        }
    }

    fn compose_config(primary_service: &str) -> ResolvedConfig {
        let mut config = ResolvedConfig::default();
        config.devcontainer.source = Some(ResolvedDevcontainerSource::Compose(
            LayerDevcontainerCompose {
                files: vec!["compose.yml".to_owned()],
                service: primary_service.to_owned(),
                run_services: None,
            },
        ));
        config
    }

    fn forward_port_for_service(service: Option<&str>, container: u16) -> ResolvedForwardPort {
        ResolvedForwardPort {
            service: service.map(str::to_owned),
            container,
            host: container,
            host_ip: "127.0.0.1".to_owned(),
            protocol: PortProtocol::Tcp,
            require_local: false,
            label: None,
        }
    }

    fn sync_plan() -> UidGidSyncPlan {
        UidGidSyncPlan::Sync {
            target: UidGidSyncTarget {
                kind: UidGidSyncTargetKind::RemoteUser,
                user: "vscode".to_owned(),
                host: HostUserIds {
                    uid: 1000,
                    gid: 1000,
                },
            },
            container: ResolvedUserIds {
                name: "vscode".to_owned(),
                uid: 2001,
                gid: 2001,
            },
        }
    }

    fn test_resources(config_hash: &str) -> DockerResources {
        DockerResources {
            container_name: "decune-test".to_owned(),
            image_tag: format!("decune/test:{config_hash}"),
            workspace_volume_name: "decune-test-workspace".to_owned(),
            labels: BTreeMap::from([
                ("decune.workspace_id".to_owned(), "workspace-id".to_owned()),
                ("decune.config_hash".to_owned(), config_hash.to_owned()),
            ]),
            config_hash: config_hash.to_owned(),
        }
    }

    fn test_up_plan_with_image_source(image: &str) -> UpPlan {
        let mut config = ResolvedConfig::default();
        config.devcontainer.source = Some(ResolvedDevcontainerSource::Image(image.to_owned()));
        let mut plan = test_up_plan_with_config(config);
        plan.image = image.to_owned();
        plan.base_image = image.to_owned();
        plan.config_layers.devcontainer = Some(ConfigLayer {
            devcontainer: Some(LayerDevcontainerMetadata {
                source: Some(LayerDevcontainerSource::Image(image.to_owned())),
                ..LayerDevcontainerMetadata::default()
            }),
            ..ConfigLayer::default()
        });
        plan
    }

    fn container_has_mount_target(mounts: &Option<Vec<ContainerMount>>, target: &str) -> bool {
        mounts.as_ref().is_some_and(|mounts| {
            mounts
                .iter()
                .any(|mount| mount.destination.as_deref() == Some(target))
        })
    }
}
