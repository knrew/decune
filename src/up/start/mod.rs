use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read as _, Write as _},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use futures_util::{
    FutureExt,
    future::{Either, select},
};

use crate::{
    config::resolved::ResolvedDevcontainerSource,
    devcontainer::lifecycle::{LifecycleRunPath, run_host_initialize_lifecycle},
    docker::{
        build::{
            DockerBuildInput, FEATURE_ENTRYPOINT_SENTINEL, FEATURE_ENTRYPOINT_TOKEN,
            FEATURE_ENTRYPOINT_WRAPPER, build_image,
        },
        client::DockerClient,
        container::{
            ContainerCreateInput, ContainerCreateSpec, ContainerInspect, create_container,
            devcontainer_keepalive_command, remove_container, start_container, stop_container,
        },
        dotfiles::materialize_dotfile_skeletons,
        exec::{ExecCommandSpec, exec_capture_output},
        image::{PullPolicy, ensure_image, image_container_tool_platform, image_startup_command},
        mounts::{DockerMountSpec, normalize_container_path},
        user::{UidGidSyncPlan, uid_gid_sync_runtime_user},
    },
    host::{
        container_tools::ContainerToolPlatform,
        credentials::{
            DECUNE_RUNTIME_TARGET, GitCredentialRuntime, GithubCliRuntime, SshAgentRuntime,
            prepare_git_credential_runtime, prepare_github_cli_runtime, prepare_ssh_agent_runtime,
        },
        forward::{
            ForwardRuntime, ServiceForwardRuntime, prepare_forward_runtime,
            prepare_service_forward_runtimes,
        },
    },
    runtime::{
        compose_cli::{
            ComposeBuildOptions, ComposeConfigOutput, ComposeConfigService, ComposeIntrospector,
            ComposeLifecyclePlan, ComposeOverridePatch, ComposeOverrideServicePatch,
            ComposePrimaryImageResolver, ComposeProjectPlan, ComposePullOptions,
            ComposeServiceValidation, ComposeUpOptions, DockerComposeCli, write_compose_override,
        },
        compose_ports::{
            ComposePortProtocol, ComposePublishedPortEndpoint, ComposePublishedPortHostIpKind,
            ComposePublishedPortOverride, ComposePublishedPortPlan,
            ComposePublishedPortPlanningInput, ComposePublishedPortReservation,
            ComposePublishedPortStartupDiagnostics, compose_published_port_plan_has_relocations,
            compose_published_port_runtime_plan, validate_compose_published_port_diagnostics,
        },
    },
    state::{
        self, LifecycleState, PublishedPortActualBinding, PublishedPortEndpointState,
        PublishedPortHostIpKind, PublishedPortRuntimeState, PublishedPortRuntimeType,
        PublishedPortSource, PublishedPortTarget, StateContainerSnapshot, WorkspaceState,
    },
    ui,
    up::{
        build::{
            build_workspace_image_layers, plan_requires_final_image_layer,
            prepare_base_image_for_plan,
        },
        existing::{self, CredentialRuntimeMountPolicy, decide_existing_container},
        metadata::{
            ComposePublishedPortFinalization, FinalizeUpPlanMountsOptions,
            build_existing_container_decision_plan, existing_remote_user_image_for_decision,
            finalize_up_plan_mounts, prepare_compose_image_metadata, prepare_image_based_metadata,
            report_deferred_config_messages,
        },
        plan::build_preliminary_up_plan_with_forwarding_resolution,
        types::{
            ExistingContainerDecision, ForwardingResolution, StartupVerification,
            UpContainerSummary, UpMountSummary, UpOptions, UpOutcome, UpPlan, UpPlanResolution,
        },
    },
    workspace::Workspace,
};

const REBUILD_STOP_TIMEOUT_SECONDS: i32 = 10;
const KEEPALIVE_STARTUP_CHECK_DELAY: Duration = Duration::from_millis(200);
const ORIGINAL_COMMAND_STARTUP_MONITOR_WINDOW: Duration = Duration::from_secs(2);
const FEATURE_ENTRYPOINT_SENTINEL_POLL_INTERVAL: Duration = Duration::from_millis(50);
const FEATURE_ENTRYPOINT_TOKEN_BYTES: usize = 32;
const FEATURE_ENTRYPOINT_TOKEN_MODE: u32 = 0o600;
const FEATURE_ENTRYPOINT_TOKEN_COMPAT_MODE: u32 = 0o644;
// The wrapper may run as an image-defined non-root user before decune can rely on
// matching host/container ownership. Keep the sentinel broadly writable for now;
// readiness is bound to a per-run token so a stale or guessed startup id alone is
// not sufficient.
const FEATURE_ENTRYPOINT_SENTINEL_MODE: u32 = 0o666;
const FEATURE_ENTRYPOINT_RUNTIME_DIR_MODE: u32 = 0o711;

mod compose;
mod compose_override;
mod container;
mod credentials;
mod feature_entrypoint;
mod listing;
mod reuse;
mod state_sync;
#[cfg(test)]
mod test_support;

#[cfg(test)]
pub(in crate::up) use compose_override::generated_compose_override_content;
pub(in crate::up) use container::create_and_start_container;
pub(in crate::up) use container::wait_for_container_exit_code;
pub(in crate::up) use listing::list_workspace_containers;
pub(in crate::up) use state_sync::StartedUpContainer;

use compose::{start_compose_project, validate_compose_canonical_model};
use compose_override::warn_on_compose_published_port_relocations;
use compose_override::{
    attach_compose_interpolation_env_to_plan, compose_port_protocol_name,
    write_generated_compose_override,
};
use container::{
    container_exited_during_startup_error, ensure_container_running_after_start,
    ensure_container_running_now, prepare_image_for_create, start_container_and_verify_running,
    startup_verification_for_plan,
};
use credentials::{
    CredentialRuntime, add_credential_runtime_mounts, container_tool_platform_for_plan,
};
use feature_entrypoint::{
    ensure_feature_entrypoints_completed, prepare_feature_entrypoint_sentinel_runtime,
};
use listing::{
    list_compose_forwarding_service_containers, list_compose_primary_containers,
    list_compose_project_containers, list_existing_compose_project_published_ports,
};
use reuse::{
    ExistingContainerReusePolicy, compose_service_forward_requires_recreate,
    recreate_existing_containers, should_reuse_existing_container,
    start_stopped_existing_container,
};
use state_sync::{
    reusable_lifecycle_state, started_up_container, started_up_container_with_state,
    state_compose_project_name, state_container_snapshot, sync_started_compose_state,
    write_reused_started_state,
};

pub(in crate::up) async fn ensure_container_started(
    options: UpOptions,
    forwarding_resolution: ForwardingResolution,
) -> Result<StartedUpContainer> {
    let workspace = Workspace::resolve(&options.workspace)?;
    let preliminary_plan = build_preliminary_up_plan_with_forwarding_resolution(
        &workspace,
        options.config_path.as_deref(),
        options.cli_layer.clone(),
        forwarding_resolution,
        options.update_features,
        options.skip_global_config,
    )?;
    run_host_initialize_lifecycle(&preliminary_plan.config, workspace.root())?;
    if preliminary_plan.compose_project.is_some() {
        validate_compose_canonical_model(&preliminary_plan).await?;
        return start_compose_project(workspace, preliminary_plan, options, forwarding_resolution)
            .await;
    }
    let plan_resolution = UpPlanResolution::new(
        forwarding_resolution,
        options.update_features,
        options.skip_global_config,
    );

    let client = DockerClient::connect_from_env()?;
    let containers = list_workspace_containers(&client, workspace.id()).await?;
    if containers.is_empty() {
        state::reconcile_state_without_container(workspace.paths().state_dir())?;
    }

    if !options.rebuild && !containers.is_empty() {
        let existing_plan = build_existing_container_decision_plan(
            &client,
            &workspace,
            options.config_path.as_deref(),
            options.cli_layer.clone(),
            containers
                .first()
                .and_then(existing::existing_container_image_id),
            &preliminary_plan,
            plan_resolution,
        )
        .await?;
        let existing_container_image = containers
            .first()
            .and_then(existing::existing_container_image_id);
        let existing_remote_user_image = existing_remote_user_image_for_decision(
            &client,
            &existing_plan,
            existing_container_image,
        )
        .await?;
        let finalized = finalize_up_plan_mounts(
            &client,
            &workspace,
            existing_plan,
            existing_remote_user_image,
            containers
                .first()
                .and_then(existing::existing_container_config_hash),
            Some((options.pull, options.no_cache)),
            FinalizeUpPlanMountsOptions {
                forwarding: forwarding_resolution,
                update_features: options.update_features,
                compose_canonical_model: None,
                compose_primary_service_user: None,
                compose_primary_service: None,
                compose_published_ports: None,
            },
        )
        .await?;
        let existing_plan = finalized.plan;
        let platform =
            container_tool_platform_for_plan(&client, &existing_plan, existing_container_image)
                .await?;
        let (existing_plan, credentials) = add_credential_runtime_mounts(
            existing_plan,
            workspace.paths().runtime_dir(),
            platform,
        )?;

        match decide_existing_container(
            &containers,
            &existing_plan.resources.config_hash,
            credentials.mount_policy(),
            false,
        )? {
            ExistingContainerDecision::ReuseRunning { id, name } => {
                report_deferred_config_messages(&existing_plan.config);
                let outcome = UpOutcome {
                    container_id: id,
                    container_name: name,
                    reused: true,
                };
                return started_up_container(
                    client,
                    workspace,
                    existing_plan,
                    outcome,
                    LifecycleRunPath::Running,
                    credentials,
                );
            }
            ExistingContainerDecision::StartStopped { id, name } => {
                report_deferred_config_messages(&existing_plan.config);
                let (outcome, state) =
                    start_stopped_existing_container(&client, &workspace, &existing_plan, id, name)
                        .await?;
                return Ok(started_up_container_with_state(
                    client,
                    workspace,
                    existing_plan,
                    outcome,
                    LifecycleRunPath::Started,
                    credentials,
                    state,
                ));
            }
            ExistingContainerDecision::Create | ExistingContainerDecision::Recreate { .. } => {}
        }
    }

    let (plan, image_prepared) = prepare_image_based_metadata(
        &client,
        &workspace,
        options.config_path.as_deref(),
        options.cli_layer,
        preliminary_plan,
        options.pull,
        plan_resolution,
    )
    .await?;
    let finalized = finalize_up_plan_mounts(
        &client,
        &workspace,
        plan,
        None,
        None,
        Some((options.pull, options.no_cache)),
        FinalizeUpPlanMountsOptions {
            forwarding: forwarding_resolution,
            update_features: options.update_features,
            compose_canonical_model: None,
            compose_primary_service_user: None,
            compose_primary_service: None,
            compose_published_ports: None,
        },
    )
    .await?;
    let plan = finalized.plan;
    let mount_image_prepared = finalized.image_prepared;
    let image_prepared =
        mount_image_prepared || (image_prepared && !plan_requires_final_image_layer(&plan));
    if !image_prepared {
        prepare_image_for_create(
            &client,
            &plan,
            options.pull,
            options.no_cache,
            image_prepared,
        )
        .await?;
    }
    let image_prepared = true;
    let platform = image_container_tool_platform(&client, &plan.image).await?;
    let (mut plan, credentials) =
        add_credential_runtime_mounts(plan, workspace.paths().runtime_dir(), platform)?;
    attach_compose_interpolation_env_to_plan(&mut plan);
    report_deferred_config_messages(&plan.config);

    match decide_existing_container(
        &containers,
        &plan.resources.config_hash,
        credentials.mount_policy(),
        options.rebuild,
    )? {
        ExistingContainerDecision::Create => {
            let outcome = create_and_start_container(
                &client,
                &workspace,
                &plan,
                options.pull,
                options.no_cache,
                image_prepared,
            )
            .await?;
            started_up_container(
                client,
                workspace,
                plan,
                outcome,
                LifecycleRunPath::New,
                credentials,
            )
        }
        ExistingContainerDecision::Recreate { containers } => {
            recreate_existing_containers(&client, &containers).await?;
            let outcome = create_and_start_container(
                &client,
                &workspace,
                &plan,
                options.pull,
                options.no_cache,
                image_prepared,
            )
            .await?;
            started_up_container(
                client,
                workspace,
                plan,
                outcome,
                LifecycleRunPath::New,
                credentials,
            )
        }
        ExistingContainerDecision::ReuseRunning { id, name } => {
            let outcome = UpOutcome {
                container_id: id,
                container_name: name,
                reused: true,
            };
            started_up_container(
                client,
                workspace,
                plan,
                outcome,
                LifecycleRunPath::Running,
                credentials,
            )
        }
        ExistingContainerDecision::StartStopped { id, name } => {
            let (outcome, state) =
                start_stopped_existing_container(&client, &workspace, &plan, id, name).await?;
            Ok(started_up_container_with_state(
                client,
                workspace,
                plan,
                outcome,
                LifecycleRunPath::Started,
                credentials,
                state,
            ))
        }
    }
}
