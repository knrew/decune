use std::{
    future::Future,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use bollard::models::{ContainerSummary, MountBindOptions, MountVolumeOptions};

use crate::{
    config::{
        ConfigHashInput, ConfigLayer, ConfigMergeInput, MountBindOptionsHashInput, MountHashInput,
        MountVolumeDriverConfigHashInput, MountVolumeOptionsHashInput, config_hash,
        layer::LayerDevcontainerMount, load::load_config_file, resolve_config,
        resolved::ResolvedConfig, resolved::ResolvedDevcontainerSource, types::MountType,
    },
    devcontainer::{
        json::DevcontainerJson,
        lifecycle::{
            LifecycleRunContext, LifecycleRunPath, PreparedLifecycleRunContext,
            prepare_container_lifecycle, run_attach_lifecycle, run_container_start_lifecycle,
            run_host_initialize_lifecycle,
        },
        metadata::parse_metadata,
    },
    docker::{
        build::{
            DockerBuildInput, DockerBuildOptions, ResolvedBuildContext, build_hash_input,
            build_image, resolve_build_context,
        },
        client::DockerClient,
        container::{
            ContainerCreateInput, ContainerCreateSpec, create_container,
            devcontainer_keepalive_command, remove_container, start_container, stop_container,
            workspace_container_list_options,
        },
        dotfiles::dotfile_mount_specs,
        exec::{ExecCommandSpec, exec_attach, resolve_exec_env, run_attached_exec_stdio},
        image::{
            PullPolicy, ensure_image, image_devcontainer_metadata_layers,
            image_devcontainer_metadata_layers_if_present,
            image_has_devcontainer_metadata_label_if_present, remove_image, tag_image,
        },
        mounts::{
            DockerMountSpec, config_mount_specs, devcontainer_mount_spec, normalize_container_path,
        },
        resource::DockerResources,
        user::{RemoteUserResolveInput, resolve_remote_user, resolve_remote_user_from_image},
    },
    ui,
    workspace::Workspace,
};

const CONFIG_HASH_LABEL: &str = "decune.config_hash";
const REBUILD_STOP_TIMEOUT_SECONDS: i32 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MountResolution {
    Resolve,
    DeferConfigMounts,
}

struct WorkspaceLocation {
    workspace_folder: String,
    workspace_mount: DockerMountSpec,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UpContainerSummary {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) image_id: Option<String>,
    pub(crate) config_hash: Option<String>,
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
    pub(crate) build_context: Option<ResolvedBuildContext>,
    pub(crate) build_options: DockerBuildOptions,
    pub(crate) resources: DockerResources,
    pub(crate) config: ResolvedConfig,
    pub(crate) workspace_folder: String,
    pub(crate) mounts: Vec<DockerMountSpec>,
}

#[derive(Debug, Clone)]
pub(crate) struct UpOptions {
    pub(crate) workspace: PathBuf,
    pub(crate) config_path: Option<PathBuf>,
    pub(crate) cli_layer: ConfigLayer,
    pub(crate) pull: bool,
    pub(crate) rebuild: bool,
    pub(crate) no_cache: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UpOutcome {
    pub(crate) container_id: String,
    pub(crate) container_name: String,
    pub(crate) reused: bool,
}

struct StartedUpContainer {
    client: DockerClient,
    workspace: Workspace,
    plan: UpPlan,
    outcome: UpOutcome,
    lifecycle_path: LifecycleRunPath,
}

pub(crate) fn decide_existing_container(
    containers: &[UpContainerSummary],
    expected_config_hash: &str,
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

pub(crate) fn default_workspace_folder(workspace: &Workspace) -> String {
    format!("/workspaces/{}", workspace.basename())
}

pub(crate) fn build_up_plan(
    workspace: &Workspace,
    explicit_config_path: Option<&Path>,
    cli_layer: ConfigLayer,
) -> Result<UpPlan> {
    build_up_plan_inner(
        workspace,
        explicit_config_path,
        cli_layer,
        Vec::new(),
        MountResolution::Resolve,
    )
}

pub(crate) fn build_up_plan_with_image_metadata(
    workspace: &Workspace,
    explicit_config_path: Option<&Path>,
    cli_layer: ConfigLayer,
    image_metadata: Vec<ConfigLayer>,
) -> Result<UpPlan> {
    build_up_plan_inner(
        workspace,
        explicit_config_path,
        cli_layer,
        image_metadata,
        MountResolution::Resolve,
    )
}

fn build_preliminary_up_plan(
    workspace: &Workspace,
    explicit_config_path: Option<&Path>,
    cli_layer: ConfigLayer,
) -> Result<UpPlan> {
    build_up_plan_inner(
        workspace,
        explicit_config_path,
        cli_layer,
        Vec::new(),
        MountResolution::DeferConfigMounts,
    )
}

fn build_up_plan_inner(
    workspace: &Workspace,
    explicit_config_path: Option<&Path>,
    cli_layer: ConfigLayer,
    image_metadata: Vec<ConfigLayer>,
    mount_resolution: MountResolution,
) -> Result<UpPlan> {
    let devcontainer_json = DevcontainerJson::load(workspace.root(), explicit_config_path)?;
    let metadata = parse_metadata(devcontainer_json.value().clone())?;
    let devcontainer_layer = metadata.to_config_layer()?;
    let global_layer = ConfigLayer::from_raw_decune_with_origin(
        load_config_file(workspace.paths().global_config_path())?,
        crate::config::path::ConfigPathOrigin::Global,
    );
    let project_layer = ConfigLayer::from_raw_decune_with_origin(
        load_config_file(workspace.paths().project_config_path())?,
        crate::config::path::ConfigPathOrigin::Project,
    );
    let config = resolve_config(ConfigMergeInput {
        image_metadata,
        global: Some(global_layer),
        devcontainer: Some(devcontainer_layer),
        project: Some(project_layer),
        cli: Some(cli_layer),
    });
    let (build_context, build_options) =
        dockerfile_build_input(workspace.root(), devcontainer_json.path(), &config)?;
    let workspace_location = resolve_workspace_location(workspace, &config, |workspace_folder| {
        static_mount_variable_context(workspace, workspace_folder, &config)
    })?;
    let mount_variables =
        static_mount_variable_context(workspace, &workspace_location.workspace_folder, &config);
    let mounts = workspace_mounts_from_resolved(
        workspace_location.workspace_mount,
        workspace.root(),
        &config,
        &mount_variables,
        mount_resolution,
    )?;
    let mut hash_input = ConfigHashInput::new(&config);
    if let Some(context) = &build_context {
        hash_input.build = Some(build_hash_input(context)?);
    }
    if mount_resolution == MountResolution::Resolve {
        hash_input.resolved_mounts = mount_hash_inputs(&mounts);
    }
    let hash = config_hash(&hash_input);
    let resources = DockerResources::from_workspace(
        workspace,
        hash,
        devcontainer_json.path().display().to_string(),
    );
    let image = image_source(&config, &resources)?;

    Ok(UpPlan {
        image,
        build_context,
        build_options,
        resources,
        config,
        workspace_folder: workspace_location.workspace_folder,
        mounts,
    })
}

pub(crate) async fn run_detached_up(options: UpOptions) -> Result<UpOutcome> {
    let started = ensure_container_started(options).await?;
    {
        let lifecycle = prepare_up_lifecycle(&started).await?;
        run_container_start_lifecycle_for_up(&started, &lifecycle).await?;
    }
    report_up_success(&started);

    Ok(started.outcome)
}

pub(crate) async fn run_attached_up(options: UpOptions) -> Result<i32> {
    let started = ensure_container_started(options).await?;
    {
        let lifecycle = prepare_up_lifecycle(&started).await?;
        run_container_start_lifecycle_for_up(&started, &lifecycle).await?;
        run_attach_lifecycle_for_up(&lifecycle).await?;
    }
    report_up_success(&started);

    let exit_code = attach_shell(
        &started.client,
        &started.plan,
        &started.outcome.container_name,
    )
    .await?;

    Ok(clamp_exit_code(exit_code))
}

async fn ensure_container_started(options: UpOptions) -> Result<StartedUpContainer> {
    let workspace = Workspace::resolve(&options.workspace)?;
    let preliminary_plan = build_preliminary_up_plan(
        &workspace,
        options.config_path.as_deref(),
        options.cli_layer.clone(),
    )?;
    run_host_initialize_lifecycle(&preliminary_plan.config, workspace.root())?;

    let client = DockerClient::connect_from_env()?;
    let containers = list_workspace_containers(&client, workspace.id()).await?;

    if !options.rebuild && !containers.is_empty() {
        let existing_plan = build_existing_container_decision_plan(
            &client,
            &workspace,
            options.config_path.as_deref(),
            options.cli_layer.clone(),
            containers.first().and_then(existing_container_image_id),
            &preliminary_plan,
        )
        .await?;
        let (existing_plan, _) = finalize_up_plan_mounts(
            &client,
            &workspace,
            existing_plan,
            containers.first().and_then(existing_container_image_id),
            None,
        )
        .await?;

        match decide_existing_container(&containers, &existing_plan.resources.config_hash, false)? {
            ExistingContainerDecision::ReuseRunning { id, name } => {
                warn_about_deferred_features(&existing_plan.config);
                let outcome = UpOutcome {
                    container_id: id,
                    container_name: name,
                    reused: true,
                };
                return Ok(StartedUpContainer {
                    client,
                    workspace,
                    plan: existing_plan,
                    outcome,
                    lifecycle_path: LifecycleRunPath::Running,
                });
            }
            ExistingContainerDecision::StartStopped { id, name } => {
                warn_about_deferred_features(&existing_plan.config);
                start_container(&client, &name).await?;
                let outcome = UpOutcome {
                    container_id: id,
                    container_name: name,
                    reused: true,
                };
                return Ok(StartedUpContainer {
                    client,
                    workspace,
                    plan: existing_plan,
                    outcome,
                    lifecycle_path: LifecycleRunPath::Started,
                });
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
    )
    .await?;
    let (plan, mount_image_prepared) = finalize_up_plan_mounts(
        &client,
        &workspace,
        plan,
        None,
        Some((options.pull, options.no_cache)),
    )
    .await?;
    let image_prepared = image_prepared || mount_image_prepared;
    warn_about_deferred_features(&plan.config);

    match decide_existing_container(&containers, &plan.resources.config_hash, options.rebuild)? {
        ExistingContainerDecision::Create => {
            let outcome = create_and_start_container(
                &client,
                &plan,
                options.pull,
                options.no_cache,
                image_prepared,
            )
            .await?;
            Ok(StartedUpContainer {
                client,
                workspace,
                plan,
                outcome,
                lifecycle_path: LifecycleRunPath::New,
            })
        }
        ExistingContainerDecision::Recreate { containers } => {
            recreate_existing_containers(&client, &containers).await?;
            let outcome = create_and_start_container(
                &client,
                &plan,
                options.pull,
                options.no_cache,
                image_prepared,
            )
            .await?;
            Ok(StartedUpContainer {
                client,
                workspace,
                plan,
                outcome,
                lifecycle_path: LifecycleRunPath::New,
            })
        }
        ExistingContainerDecision::ReuseRunning { id, name } => {
            let outcome = UpOutcome {
                container_id: id,
                container_name: name,
                reused: true,
            };
            Ok(StartedUpContainer {
                client,
                workspace,
                plan,
                outcome,
                lifecycle_path: LifecycleRunPath::Running,
            })
        }
        ExistingContainerDecision::StartStopped { id, name } => {
            start_container(&client, &name).await?;
            let outcome = UpOutcome {
                container_id: id,
                container_name: name,
                reused: true,
            };
            Ok(StartedUpContainer {
                client,
                workspace,
                plan,
                outcome,
                lifecycle_path: LifecycleRunPath::Started,
            })
        }
    }
}

async fn build_existing_container_decision_plan(
    client: &DockerClient,
    workspace: &Workspace,
    explicit_config_path: Option<&Path>,
    cli_layer: ConfigLayer,
    existing_container_image_id: Option<&str>,
    preliminary_plan: &UpPlan,
) -> Result<UpPlan> {
    if preliminary_plan.build_context.is_some() {
        let image = existing_container_image_id.unwrap_or(&preliminary_plan.image);
        warn_about_unsupported_dockerfile_image_metadata(client, image).await?;
        return build_up_plan(workspace, explicit_config_path, cli_layer);
    }

    let image_metadata =
        match image_devcontainer_metadata_layers_if_present(client, &preliminary_plan.image).await?
        {
            Some(image_metadata) => image_metadata,
            None => {
                let Some(image_id) = existing_container_image_id else {
                    return build_up_plan(workspace, explicit_config_path, cli_layer);
                };
                let Some(image_metadata) =
                    image_devcontainer_metadata_layers_if_present(client, image_id).await?
                else {
                    return build_up_plan(workspace, explicit_config_path, cli_layer);
                };
                image_metadata
            }
        };

    if image_metadata.is_empty() {
        return build_up_plan(workspace, explicit_config_path, cli_layer);
    }

    build_up_plan_with_image_metadata(workspace, explicit_config_path, cli_layer, image_metadata)
}

async fn prepare_image_based_metadata(
    client: &DockerClient,
    workspace: &Workspace,
    explicit_config_path: Option<&Path>,
    cli_layer: ConfigLayer,
    preliminary_plan: UpPlan,
    pull: bool,
) -> Result<(UpPlan, bool)> {
    if preliminary_plan.build_context.is_some() {
        return Ok((
            build_up_plan(workspace, explicit_config_path, cli_layer)?,
            false,
        ));
    }

    ensure_image(
        client,
        &preliminary_plan.image,
        if pull {
            PullPolicy::Always
        } else {
            PullPolicy::Missing
        },
    )
    .await?;
    let image_metadata =
        image_devcontainer_metadata_layers(client, &preliminary_plan.image).await?;
    if image_metadata.is_empty() {
        return Ok((
            build_up_plan(workspace, explicit_config_path, cli_layer)?,
            true,
        ));
    }

    let plan = build_up_plan_with_image_metadata(
        workspace,
        explicit_config_path,
        cli_layer,
        image_metadata,
    )?;

    Ok((plan, true))
}

async fn finalize_up_plan_mounts(
    client: &DockerClient,
    workspace: &Workspace,
    mut plan: UpPlan,
    remote_user_image: Option<&str>,
    build_for_lookup: Option<(bool, bool)>,
) -> Result<(UpPlan, bool)> {
    let mut lookup_image = remote_user_image.map(ToOwned::to_owned);
    let mut image_prepared = false;
    if lookup_image.is_none() {
        if let Some(context) = plan.build_context.clone() {
            let Some((pull, no_cache)) = build_for_lookup else {
                return Ok((plan, false));
            };
            let mut build_options = plan.build_options.clone();
            build_options.pull = pull;
            build_options.no_cache = no_cache;
            build_image(
                client,
                DockerBuildInput {
                    image_tag: plan.image.clone(),
                    labels: plan.resources.labels.clone().into_iter().collect(),
                    context,
                    options: build_options,
                },
            )
            .await?;
            lookup_image = Some(plan.image.clone());
            image_prepared = true;
        } else {
            lookup_image = Some(plan.image.clone());
        }
    };
    let lookup_image = lookup_image.expect("lookup image must be set");
    let remote_user = resolve_remote_user_from_image(
        client,
        &lookup_image,
        RemoteUserResolveInput {
            explicit_remote_user: plan.config.devcontainer.remote_user.as_deref(),
            image_metadata_remote_user: None,
        },
    )
    .await?;
    let remote_user_name = remote_user.user;
    let remote_user_home = remote_user.home;
    let workspace_location =
        resolve_workspace_location(workspace, &plan.config, |workspace_folder| {
            mount_variable_context(
                workspace,
                workspace_folder,
                remote_user_name.clone(),
                remote_user_home.clone(),
            )
        })?;
    let mount_variables = mount_variable_context(
        workspace,
        &workspace_location.workspace_folder,
        remote_user_name,
        remote_user_home,
    );
    let mounts = workspace_mounts_from_resolved(
        workspace_location.workspace_mount,
        workspace.root(),
        &plan.config,
        &mount_variables,
        MountResolution::Resolve,
    )?;
    let mut hash_input = ConfigHashInput::new(&plan.config);
    if let Some(context) = &plan.build_context {
        hash_input.build = Some(build_hash_input(context)?);
    }
    hash_input.resolved_mounts = mount_hash_inputs(&mounts);
    let hash = config_hash(&hash_input);
    let resources = DockerResources::from_workspace(
        workspace,
        hash,
        plan.resources
            .labels
            .get("devcontainer.config_file")
            .cloned()
            .unwrap_or_default(),
    );
    let image = image_source(&plan.config, &resources)?;
    if image_prepared && image != lookup_image {
        tag_image(client, &lookup_image, &image).await?;
        remove_image(client, &lookup_image, false).await?;
    }

    plan.image = image;
    plan.resources = resources;
    plan.workspace_folder = workspace_location.workspace_folder;
    plan.mounts = mounts;

    Ok((plan, image_prepared))
}

async fn recreate_existing_containers(
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

async fn create_and_start_container(
    client: &DockerClient,
    plan: &UpPlan,
    pull: bool,
    no_cache: bool,
    image_prepared: bool,
) -> Result<UpOutcome> {
    if let Some(context) = plan.build_context.clone() {
        if !image_prepared {
            let mut build_options = plan.build_options.clone();
            build_options.pull = pull;
            build_options.no_cache = no_cache;
            build_image(
                client,
                DockerBuildInput {
                    image_tag: plan.image.clone(),
                    labels: plan.resources.labels.clone().into_iter().collect(),
                    context,
                    options: build_options,
                },
            )
            .await?;
        }
        warn_about_unsupported_dockerfile_image_metadata(client, &plan.image).await?;
    } else if !image_prepared {
        ensure_image(
            client,
            &plan.image,
            if pull {
                PullPolicy::Always
            } else {
                PullPolicy::Missing
            },
        )
        .await?;
    }

    let (entrypoint, command) = if plan.config.devcontainer.override_command {
        let (entrypoint, command) = devcontainer_keepalive_command();
        (Some(entrypoint), Some(command))
    } else {
        (None, None)
    };
    let spec = ContainerCreateSpec::from_resolved(ContainerCreateInput {
        image: &plan.image,
        resources: &plan.resources,
        config: &plan.config,
        entrypoint,
        command,
        working_dir: Some(plan.workspace_folder.clone()),
        mounts: plan.mounts.clone(),
    });
    let container_id = create_container(client, &spec).await?;
    start_new_container(client, &plan.resources.container_name).await?;

    Ok(UpOutcome {
        container_id,
        container_name: plan.resources.container_name.clone(),
        reused: false,
    })
}

fn report_up_success(started: &StartedUpContainer) {
    let name = &started.outcome.container_name;
    let message = match started.lifecycle_path {
        LifecycleRunPath::New => format!("Started dev container: {name}"),
        LifecycleRunPath::Started => format!("Started existing dev container: {name}"),
        LifecycleRunPath::Running => format!("Reusing running dev container: {name}"),
    };

    ui::done(&message);
}

async fn prepare_up_lifecycle(
    started: &StartedUpContainer,
) -> Result<PreparedLifecycleRunContext<'_>> {
    let remote_user = resolve_remote_user(
        &started.client,
        &started.outcome.container_name,
        RemoteUserResolveInput {
            explicit_remote_user: started.plan.config.devcontainer.remote_user.as_deref(),
            image_metadata_remote_user: None,
        },
    )
    .await?;

    prepare_container_lifecycle(LifecycleRunContext {
        client: &started.client,
        container: &started.outcome.container_name,
        config: &started.plan.config,
        workspace_root: started.workspace.root(),
        workspace_basename: started.workspace.basename(),
        workspace_id: started.workspace.id(),
        workspace_folder: &started.plan.workspace_folder,
        remote_user,
    })
    .await
}

async fn run_container_start_lifecycle_for_up(
    started: &StartedUpContainer,
    lifecycle: &PreparedLifecycleRunContext<'_>,
) -> Result<()> {
    run_container_start_lifecycle(started.lifecycle_path, lifecycle).await
}

async fn run_attach_lifecycle_for_up(lifecycle: &PreparedLifecycleRunContext<'_>) -> Result<()> {
    run_attach_lifecycle(lifecycle).await
}

async fn attach_shell(client: &DockerClient, plan: &UpPlan, container_name: &str) -> Result<i64> {
    let remote_user = resolve_remote_user(
        client,
        container_name,
        RemoteUserResolveInput {
            explicit_remote_user: plan.config.devcontainer.remote_user.as_deref(),
            image_metadata_remote_user: None,
        },
    )
    .await?;
    let env = resolve_exec_env(
        client,
        container_name,
        &remote_user.user,
        remote_user.shell.as_deref(),
        &plan.config.devcontainer.remote_env,
        plan.config.devcontainer.user_env_probe,
    )
    .await?;
    let candidates =
        shell_command_candidates(plan.config.shell.as_deref(), remote_user.shell.as_deref());
    let (spec, attached) = first_successful_shell_candidate(candidates, |command| {
        let env = env.clone();
        let user = remote_user.user.clone();
        let working_dir = plan.workspace_folder.clone();

        async move {
            let spec = ExecCommandSpec {
                command: vec![command],
                user: Some(user),
                working_dir: Some(working_dir),
                env,
                tty: true,
            };
            let attached = exec_attach(client, container_name, &spec).await?;

            Ok::<_, anyhow::Error>((spec, attached))
        }
    })
    .await
    .with_context(|| format!("Failed to start an attached shell in container: {container_name}"))?;

    run_attached_exec_stdio(client, container_name, &spec, attached).await
}

pub(crate) async fn first_successful_shell_candidate<T, F, Fut>(
    candidates: Vec<String>,
    mut start_candidate: F,
) -> Result<T>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    if candidates.is_empty() {
        bail!("No shell command candidate is available");
    }

    let mut failures = Vec::new();
    for command in candidates {
        match start_candidate(command.clone()).await {
            Ok(result) => return Ok(result),
            Err(error) => failures.push(format!("{command}: {error:#}")),
        }
    }

    bail!(
        "Failed to start any shell command candidate. Tried: {}",
        failures.join("; ")
    )
}

pub(crate) fn shell_command_candidates(
    config_shell: Option<&str>,
    remote_user_shell: Option<&str>,
) -> Vec<String> {
    if let Some(shell) = normalized_shell(config_shell) {
        return vec![shell];
    }

    let mut candidates = Vec::new();
    if let Some(shell) = normalized_shell(remote_user_shell) {
        candidates.push(shell);
    }
    candidates.push("/bin/bash".to_owned());
    candidates.push("/bin/sh".to_owned());
    candidates.dedup();
    candidates
}

fn normalized_shell(shell: Option<&str>) -> Option<String> {
    shell
        .map(str::trim)
        .filter(|shell| !shell.is_empty())
        .map(ToOwned::to_owned)
}

fn clamp_exit_code(exit_code: i64) -> i32 {
    match exit_code {
        0..=255 => exit_code as i32,
        _ => 1,
    }
}

async fn start_new_container(client: &DockerClient, container_name: &str) -> Result<()> {
    match start_container(client, container_name).await {
        Ok(()) => Ok(()),
        Err(start_error) => {
            let cleanup = remove_container(client, container_name, true, true).await;
            match cleanup {
                Ok(()) => Err(start_error),
                Err(cleanup_error) => Err(start_error.context(format!(
                    "Failed to remove Docker container after start failure: {container_name}: {cleanup_error:#}"
                ))),
            }
        }
    }
}

fn image_source(config: &ResolvedConfig, resources: &DockerResources) -> Result<String> {
    match &config.devcontainer.source {
        Some(ResolvedDevcontainerSource::Image(image)) => Ok(image.clone()),
        Some(ResolvedDevcontainerSource::Dockerfile(_)) => Ok(resources.image_tag.clone()),
        None => bail!("Devcontainer image is required"),
    }
}

fn dockerfile_build_input(
    workspace_root: &Path,
    devcontainer_file: &Path,
    config: &ResolvedConfig,
) -> Result<(Option<ResolvedBuildContext>, DockerBuildOptions)> {
    match &config.devcontainer.source {
        Some(ResolvedDevcontainerSource::Dockerfile(build)) => Ok((
            Some(resolve_build_context(
                workspace_root,
                devcontainer_file,
                build,
            )?),
            DockerBuildOptions {
                build_args: build.args.clone(),
                target: build.target.clone(),
                cache_from: build.cache_from.clone(),
                ..DockerBuildOptions::default()
            },
        )),
        _ => Ok((None, DockerBuildOptions::default())),
    }
}

fn workspace_mounts_from_resolved(
    workspace_mount: DockerMountSpec,
    workspace_root: &Path,
    config: &ResolvedConfig,
    variables: &crate::config::variables::VariableContext,
    mount_resolution: MountResolution,
) -> Result<Vec<DockerMountSpec>> {
    let workspace_target = workspace_mount.target.clone();
    let mut mounts = vec![workspace_mount];
    if mount_resolution == MountResolution::Resolve {
        let config_mounts = config_mount_specs(config, workspace_root, variables)?;
        reject_workspace_mount_target_conflicts(&workspace_target, &config_mounts)?;
        mounts.extend(config_mounts);

        let dotfile_mounts = dotfile_mount_specs(config, workspace_root, variables)?;
        reject_workspace_mount_target_conflicts(&workspace_target, &dotfile_mounts)?;
        mounts.extend(dotfile_mounts);
    }

    Ok(mounts)
}

fn reject_workspace_mount_target_conflicts(
    workspace_target: &str,
    mounts: &[DockerMountSpec],
) -> Result<()> {
    let workspace_target = normalize_container_path(workspace_target);
    if mounts
        .iter()
        .any(|mount| normalize_container_path(&mount.target) == workspace_target)
    {
        bail!("Mount target conflicts with workspace mount target: {workspace_target}");
    }

    Ok(())
}

fn resolve_workspace_location<F>(
    workspace: &Workspace,
    config: &ResolvedConfig,
    variables_for_workspace_folder: F,
) -> Result<WorkspaceLocation>
where
    F: Fn(&str) -> crate::config::variables::VariableContext,
{
    let seed_workspace_folder = config
        .devcontainer
        .workspace_folder
        .clone()
        .unwrap_or_else(|| default_workspace_folder(workspace));
    let variables = variables_for_workspace_folder(&seed_workspace_folder);
    let workspace_mount = workspace_mount_spec(workspace, config, &variables)?;
    let workspace_folder = config
        .devcontainer
        .workspace_folder
        .clone()
        .unwrap_or_else(|| workspace_mount.target.clone());

    Ok(WorkspaceLocation {
        workspace_folder,
        workspace_mount,
    })
}

fn workspace_mount_spec(
    workspace: &Workspace,
    config: &ResolvedConfig,
    variables: &crate::config::variables::VariableContext,
) -> Result<DockerMountSpec> {
    if let Some(workspace_mount) = &config.devcontainer.workspace_mount {
        return devcontainer_mount_spec(
            &LayerDevcontainerMount::String(workspace_mount.clone()),
            workspace.root(),
            variables,
        )
        .context("Failed to resolve workspaceMount");
    }

    Ok(DockerMountSpec {
        source: Some(workspace.root().display().to_string()),
        target: default_workspace_folder(workspace),
        mount_type: MountType::Bind,
        read_only: false,
        consistency: None,
        bind_options: None,
        volume_options: None,
    })
}

fn mount_hash_inputs(mounts: &[DockerMountSpec]) -> Vec<MountHashInput> {
    mounts
        .iter()
        .map(|mount| MountHashInput {
            source: mount.source.clone(),
            target: mount.target.clone(),
            mount_type: mount.mount_type,
            read_only: mount.read_only,
            consistency: mount.consistency.clone(),
            bind_options: mount.bind_options.as_ref().map(bind_options_hash_input),
            volume_options: mount.volume_options.as_ref().map(volume_options_hash_input),
        })
        .collect()
}

fn bind_options_hash_input(options: &MountBindOptions) -> MountBindOptionsHashInput {
    MountBindOptionsHashInput {
        propagation: options.propagation.map(|value| value.to_string()),
        non_recursive: options.non_recursive,
        create_mountpoint: options.create_mountpoint,
        read_only_non_recursive: options.read_only_non_recursive,
        read_only_force_recursive: options.read_only_force_recursive,
    }
}

fn volume_options_hash_input(options: &MountVolumeOptions) -> MountVolumeOptionsHashInput {
    MountVolumeOptionsHashInput {
        no_copy: options.no_copy,
        labels: options
            .labels
            .clone()
            .map(|labels| labels.into_iter().collect()),
        driver_config: options.driver_config.as_ref().map(|driver_config| {
            MountVolumeDriverConfigHashInput {
                name: driver_config.name.clone(),
                options: driver_config
                    .options
                    .clone()
                    .map(|options| options.into_iter().collect()),
            }
        }),
        subpath: options.subpath.clone(),
    }
}

fn static_mount_variable_context(
    workspace: &Workspace,
    workspace_folder: &str,
    config: &ResolvedConfig,
) -> crate::config::variables::VariableContext {
    let remote_user = config
        .devcontainer
        .remote_user
        .clone()
        .unwrap_or_else(|| "root".to_owned());

    mount_variable_context(workspace, workspace_folder, remote_user, "/root".to_owned())
}

fn mount_variable_context(
    workspace: &Workspace,
    workspace_folder: &str,
    remote_user: String,
    remote_user_home: String,
) -> crate::config::variables::VariableContext {
    crate::config::variables::VariableContext::new(
        workspace.root().to_path_buf(),
        workspace.basename().to_owned(),
        workspace_folder.to_owned(),
        container_workspace_folder_basename(workspace_folder, workspace),
        workspace.id().to_owned(),
        current_uid(),
        current_gid(),
        remote_user,
        remote_user_home,
    )
}

fn container_workspace_folder_basename(workspace_folder: &str, workspace: &Workspace) -> String {
    Path::new(workspace_folder)
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| workspace.basename())
        .to_owned()
}

#[cfg(unix)]
fn current_uid() -> u32 {
    unsafe { libc::getuid() }
}

#[cfg(not(unix))]
fn current_uid() -> u32 {
    0
}

#[cfg(unix)]
fn current_gid() -> u32 {
    unsafe { libc::getgid() }
}

#[cfg(not(unix))]
fn current_gid() -> u32 {
    0
}

async fn list_workspace_containers(
    client: &DockerClient,
    workspace_id: &str,
) -> Result<Vec<UpContainerSummary>> {
    let containers = client
        .raw()
        .list_containers(Some(workspace_container_list_options(workspace_id)))
        .await
        .with_context(|| {
            format!("Failed to list Docker containers for workspace: {workspace_id}")
        })?;

    Ok(containers
        .into_iter()
        .filter_map(container_summary)
        .collect())
}

fn container_summary(container: ContainerSummary) -> Option<UpContainerSummary> {
    let id = container.id?;
    let name = container
        .names
        .and_then(|names| names.into_iter().next())
        .map(|name| name.trim_start_matches('/').to_owned())
        .unwrap_or_else(|| id.clone());
    let config_hash = container
        .labels
        .and_then(|labels| labels.get(CONFIG_HASH_LABEL).cloned());
    let running = container
        .state
        .is_some_and(|state| state.to_string() == "running");

    Some(UpContainerSummary {
        id,
        name,
        image_id: container.image_id,
        config_hash,
        running,
    })
}

fn existing_container_image_id(container: &UpContainerSummary) -> Option<&str> {
    container
        .image_id
        .as_deref()
        .filter(|image_id| !image_id.trim().is_empty())
}

fn warn_about_deferred_features(config: &ResolvedConfig) {
    if !config.features.is_empty() {
        ui::warn("Dev Container Features are not applied yet in this milestone");
    }
}

async fn warn_about_unsupported_dockerfile_image_metadata(
    client: &DockerClient,
    image: &str,
) -> Result<()> {
    if image_has_devcontainer_metadata_label_if_present(client, image).await? == Some(true) {
        ui::warn(&format!(
            "Dockerfile image label devcontainer.metadata is not merged in decune v0.1: {image}. Move this metadata to devcontainer.json or use an image-based devcontainer."
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs, ops::Deref, path::PathBuf};

    use anyhow::Context;
    use bollard::models::{MountBindOptions, MountBindOptionsPropagationEnum, MountVolumeOptions};

    use crate::config::resolved::ResolvedDevcontainerSource;
    use crate::config::types::MountType;
    use crate::config::{ConfigHashInput, ConfigLayer, config_hash};
    use crate::docker::client::DockerClient;
    use crate::docker::container::{remove_container, stop_container};
    use crate::docker::exec::{ExecCommandSpec, exec_capture};
    use crate::docker::image::{PullPolicy, ensure_image, remove_image};
    use crate::docker::mounts::DockerMountSpec;
    use crate::workspace::Workspace;

    use super::{
        ExistingContainerDecision, UpContainerSummary, UpOptions, build_up_plan,
        build_up_plan_with_image_metadata, decide_existing_container, default_workspace_folder,
        first_successful_shell_candidate, list_workspace_containers, mount_hash_inputs,
        run_attached_up, run_detached_up, shell_command_candidates,
    };

    #[test]
    fn existing_container_decision_creates_when_no_container_exists() {
        let decision = decide_existing_container(&[], "hash123", false).unwrap();

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
            running: true,
        };

        let decision = decide_existing_container(&[container], "hash123", false).unwrap();

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
            running: false,
        };

        let decision = decide_existing_container(&[container], "hash123", false).unwrap();

        assert_eq!(
            decision,
            ExistingContainerDecision::StartStopped {
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
            running: true,
        };

        let error = decide_existing_container(&[container], "new-hash", false).unwrap_err();

        assert!(error.to_string().contains("Run decune rebuild"));
    }

    #[test]
    fn existing_container_decision_recreates_when_rebuild_is_requested() {
        let container = UpContainerSummary {
            id: "container-id".to_owned(),
            name: "decune-project-abc123".to_owned(),
            image_id: None,
            config_hash: Some("old-hash".to_owned()),
            running: true,
        };

        let decision = decide_existing_container(&[container.clone()], "new-hash", true).unwrap();

        assert_eq!(
            decision,
            ExistingContainerDecision::Recreate {
                containers: vec![container]
            }
        );
    }

    #[test]
    fn default_workspace_folder_uses_workspace_basename() {
        let workspace = test_workspace("project");

        assert_eq!(default_workspace_folder(&workspace), "/workspaces/project");
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
    fn build_up_plan_uses_workspace_mount_target_as_default_workspace_folder() {
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

        let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_eq!(plan.workspace_folder, "/workspace");
        assert_eq!(plan.mounts.len(), 1);
        assert_eq!(
            plan.mounts[0].source.as_deref(),
            Some(workspace.root().to_str().unwrap())
        );
        assert_eq!(plan.mounts[0].target, "/workspace");
        assert_eq!(plan.mounts[0].mount_type, MountType::Bind);
    }

    #[test]
    fn build_up_plan_does_not_expand_workspace_mount_target_twice_when_used_as_workspace_folder() {
        let workspace = test_workspace("workspace-mount-variable-plan");
        write_devcontainer(
            &workspace,
            r#"
            {
              "image": "alpine:3.20",
              "workspaceMount": "source=${localWorkspaceFolder},target=${containerWorkspaceFolder}/src,type=bind"
            }
            "#,
        );

        let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_eq!(
            plan.workspace_folder,
            "/workspaces/workspace-mount-variable-plan/src"
        );
        assert_eq!(plan.mounts.len(), 1);
        assert_eq!(
            plan.mounts[0].source.as_deref(),
            Some(workspace.root().to_str().unwrap())
        );
        assert_eq!(plan.mounts[0].target, plan.workspace_folder);
        assert_eq!(plan.mounts[0].mount_type, MountType::Bind);
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
              "workspaceMount": "source=${localWorkspaceFolder},target=/run/decune/workspace,type=bind"
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
            propagation: Some(MountBindOptionsPropagationEnum::RSHARED),
            ..MountBindOptions::default()
        });
        let mut rslave = test_mount();
        rslave.bind_options = Some(MountBindOptions {
            propagation: Some(MountBindOptionsPropagationEnum::RSLAVE),
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
    fn up_detach_creates_and_reuses_container_when_docker_tests_are_enabled() {
        if !docker_tests_enabled() {
            eprintln!("skipped: set DECUNE_DOCKER_TESTS=1 to run Docker integration tests");
            return;
        }

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
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                })
                .await?;
                assert_eq!(first.container_name, container_name);
                assert!(!first.reused);

                let inspect = client
                    .raw()
                    .inspect_container(&container_name, None)
                    .await?;
                assert_eq!(inspect.state.and_then(|state| state.running), Some(true));

                let second = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
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
    fn up_detach_reuses_container_when_built_image_tag_is_removed_if_docker_tests_are_enabled() {
        if !docker_tests_enabled() {
            eprintln!("skipped: set DECUNE_DOCKER_TESTS=1 to run Docker integration tests");
            return;
        }

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
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                })
                .await?;
                assert!(!first.reused);

                remove_image(&client, &image, true).await?;

                let second = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
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
        if !docker_tests_enabled() {
            eprintln!("skipped: set DECUNE_DOCKER_TESTS=1 to run Docker integration tests");
            return;
        }

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
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                })
                .await?;
                assert_eq!(first.container_name, container_name);
                assert!(!first.reused);

                remove_image(&client, &image, true).await?;

                let second = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
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
    fn up_detach_stops_lifecycle_after_failure_when_docker_tests_are_enabled() {
        if !docker_tests_enabled() {
            eprintln!("skipped: set DECUNE_DOCKER_TESTS=1 to run Docker integration tests");
            return;
        }

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
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
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
        if !docker_tests_enabled() {
            eprintln!("skipped: set DECUNE_DOCKER_TESTS=1 to run Docker integration tests");
            return;
        }

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
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
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
    fn up_detach_does_not_run_post_attach_when_docker_tests_are_enabled() {
        if !docker_tests_enabled() {
            eprintln!("skipped: set DECUNE_DOCKER_TESTS=1 to run Docker integration tests");
            return;
        }

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
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
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
    fn up_attached_runs_post_attach_before_shell_when_docker_tests_are_enabled() {
        if !docker_tests_enabled() {
            eprintln!("skipped: set DECUNE_DOCKER_TESTS=1 to run Docker integration tests");
            return;
        }

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
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
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
    fn up_running_attached_runs_post_attach_each_attach_when_docker_tests_are_enabled() {
        if !docker_tests_enabled() {
            eprintln!("skipped: set DECUNE_DOCKER_TESTS=1 to run Docker integration tests");
            return;
        }

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
                  "postAttachCommand": "count=0; if [ -f /tmp/decune-post-attach-count ]; then count=$(cat /tmp/decune-post-attach-count); fi; count=$((count + 1)); printf '%s' \"$count\" >/tmp/decune-post-attach-count"
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
                        cli_layer: ConfigLayer::default(),
                        pull: false,
                        rebuild: false,
                        no_cache: false,
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
    fn up_attached_stopped_runs_start_attach_shell_when_docker_tests_are_enabled() {
        if !docker_tests_enabled() {
            eprintln!("skipped: set DECUNE_DOCKER_TESTS=1 to run Docker integration tests");
            return;
        }

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
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                })
                .await?;
                stop_container(&client, &container_name, 10).await?;

                let exit_code = run_attached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
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
    fn rebuild_detach_recreates_without_post_attach_when_docker_tests_are_enabled() {
        if !docker_tests_enabled() {
            eprintln!("skipped: set DECUNE_DOCKER_TESTS=1 to run Docker integration tests");
            return;
        }

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
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                })
                .await?;
                assert!(!first.reused);

                let second = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: true,
                    no_cache: false,
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
    fn rebuild_attached_recreates_runs_post_attach_before_shell_when_docker_tests_are_enabled() {
        if !docker_tests_enabled() {
            eprintln!("skipped: set DECUNE_DOCKER_TESTS=1 to run Docker integration tests");
            return;
        }

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
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                })
                .await?;
                assert!(!first.reused);

                let exit_code = run_attached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: true,
                    no_cache: false,
                })
                .await?;
                assert_eq!(exit_code, 0);

                let inspect = client
                    .raw()
                    .inspect_container(&container_name, None)
                    .await?;
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
    fn up_detach_applies_remote_env_to_lifecycle_when_docker_tests_are_enabled() {
        if !docker_tests_enabled() {
            eprintln!("skipped: set DECUNE_DOCKER_TESTS=1 to run Docker integration tests");
            return;
        }

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
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
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
    fn up_detach_applies_user_env_probe_to_lifecycle_when_docker_tests_are_enabled() {
        if !docker_tests_enabled() {
            eprintln!("skipped: set DECUNE_DOCKER_TESTS=1 to run Docker integration tests");
            return;
        }

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
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
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
    fn up_detach_omits_remote_probe_env_for_root_post_start_hook_when_docker_tests_are_enabled() {
        if !docker_tests_enabled() {
            eprintln!("skipped: set DECUNE_DOCKER_TESTS=1 to run Docker integration tests");
            return;
        }

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
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
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
    fn up_detach_probes_env_with_remote_user_shell_when_docker_tests_are_enabled() {
        if !docker_tests_enabled() {
            eprintln!("skipped: set DECUNE_DOCKER_TESTS=1 to run Docker integration tests");
            return;
        }

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
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
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
    fn up_detach_removes_new_container_when_start_fails_if_docker_tests_are_enabled() {
        if !docker_tests_enabled() {
            eprintln!("skipped: set DECUNE_DOCKER_TESTS=1 to run Docker integration tests");
            return;
        }

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
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                })
                .await
                .unwrap_err();
                assert!(
                    error
                        .to_string()
                        .contains("Failed to start Docker container")
                );

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
        let config = crate::config::resolved::ResolvedConfig::default();
        let mut input = ConfigHashInput::new(&config);
        input.resolved_mounts = mount_hash_inputs(&[mount]);

        config_hash(&input)
    }

    fn docker_tests_enabled() -> bool {
        std::env::var_os("DECUNE_DOCKER_TESTS").is_some_and(|value| value == "1")
    }
}
