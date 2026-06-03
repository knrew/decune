use std::{
    collections::BTreeMap,
    future::Future,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result, bail};
use bollard::models::{ContainerSummary, MountBindOptions, MountVolumeOptions};
use bollard::query_parameters::WaitContainerOptionsBuilder;
use futures_util::TryStreamExt;

use crate::{
    config::{
        ConfigHashInput, ConfigLayer, ConfigMergeInput, FeatureLockHashEntry,
        MountBindOptionsHashInput, MountHashInput, MountVolumeDriverConfigHashInput,
        MountVolumeOptionsHashInput, config_hash,
        layer::{LayerDevcontainerMount, LayerFeature},
        load::load_config_file,
        resolve_config,
        resolved::ResolvedConfig,
        resolved::ResolvedDevcontainerSource,
        types::{GithubCredentialsMode, MountType},
    },
    devcontainer::{
        features::{
            FeatureRef, PreparedFeatureInstallPlan, parse_feature_ref_from_devcontainer_dir,
            prepare_feature_install_plan, read_feature_lock_file, remove_feature_lock_file,
            resolve_locked_feature_ref,
        },
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
            DockerBuildInput, DockerBuildOptions, FEATURE_ENTRYPOINT_WRAPPER,
            FeatureLayerBuildFeature, FeatureLayerBuildInput, ResolvedBuildContext,
            build_hash_input, build_image, prepare_feature_layer_build_context,
            resolve_build_context,
        },
        client::DockerClient,
        container::{
            ContainerCreateInput, ContainerCreateSpec, ContainerHostConfig, create_container,
            devcontainer_keepalive_command, remove_container, start_container, stop_container,
            workspace_container_list_options,
        },
        dotfiles::dotfile_mount_specs,
        exec::{
            ExecCommandSpec, exec_attach, exec_capture, exec_detached, inspect_exec,
            resolve_exec_env, run_attached_exec_stdio,
        },
        image::{
            LocalImagePresence, PullPolicy, ensure_image,
            image_devcontainer_metadata_layers_if_present_with_forward_ports,
            image_devcontainer_metadata_layers_with_forward_ports,
            image_has_devcontainer_metadata_label_if_present, image_startup_command,
            local_image_presence, remove_image, tag_image,
        },
        mounts::{
            DockerMountSpec, config_mount_specs, devcontainer_mount_spec, normalize_container_path,
        },
        ports::{ResolvedForwardPort, resolve_forward_ports},
        resource::DockerResources,
        user::{
            RemoteUserResolveInput, image_config_user, resolve_remote_user,
            resolve_remote_user_from_image,
        },
    },
    host::{
        credentials::{
            DECUNE_RUNTIME_TARGET, GITHUB_CLI_CONFIG_TARGET, GITHUB_CLI_TOKEN_DIR_TARGET,
            GitCredentialRuntime, GithubCliRuntime, SSH_AGENT_SOCKET_TARGET, SshAgentRuntime,
            host_github_auth_token_available, prepare_git_credential_runtime,
            prepare_github_cli_runtime, prepare_ssh_agent_runtime,
        },
        daemon::HostDaemon,
        forward::{
            AutoForwardConfig, ForwardAgentStatus, ForwardRuntime, ForwardSession,
            forward_agent_command, new_forward_agent_secret, prepare_forward_runtime,
            start_forward_session_with_auto, wait_for_forward_agent_with_status,
        },
    },
    ui,
    workspace::Workspace,
};

const CONFIG_HASH_LABEL: &str = "decune.config_hash";
const REBUILD_STOP_TIMEOUT_SECONDS: i32 = 10;
const GITHUB_CLI_FEATURE_REF: &str = "ghcr.io/devcontainers/features/github-cli:1";
const GITHUB_CLI_FEATURE_CANONICAL_ID: &str = "ghcr.io/devcontainers/features/github-cli";
static IMAGE_COMMAND_PROBE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const DECUNE_MANAGED_RUNTIME_MOUNT_TARGETS: &[&str] = &[
    DECUNE_RUNTIME_TARGET,
    SSH_AGENT_SOCKET_TARGET,
    GITHUB_CLI_TOKEN_DIR_TARGET,
    GITHUB_CLI_CONFIG_TARGET,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MountResolution {
    Resolve,
    DeferConfigMounts,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForwardingResolution {
    Resolve,
    IgnoreDetached,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UpPlanResolution {
    forwarding: ForwardingResolution,
    update_features: bool,
}

impl UpPlanResolution {
    fn new(forwarding: ForwardingResolution, update_features: bool) -> Self {
        Self {
            forwarding,
            update_features,
        }
    }
}

struct ImageLookupPreparation<'a> {
    image: &'a mut String,
    remote_user_image: Option<&'a str>,
    base_image: &'a mut Option<String>,
    image_prepared: &'a mut bool,
    build_options: Option<(bool, bool)>,
    command_probe_build_options: Option<(bool, bool)>,
}

struct CommandProbeImage {
    image: String,
    uses_existing_image: bool,
}

struct WorkspaceLocation {
    workspace_folder: String,
    workspace_mount: DockerMountSpec,
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
    pub(crate) resources: DockerResources,
    pub(crate) config_layers: ConfigMergeInput,
    pub(crate) config: ResolvedConfig,
    pub(crate) workspace_folder: String,
    pub(crate) mounts: Vec<DockerMountSpec>,
    pub(crate) forward_ports: Vec<ResolvedForwardPort>,
    pub(crate) ignored_detached_forwarding: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct UpOptions {
    pub(crate) workspace: PathBuf,
    pub(crate) config_path: Option<PathBuf>,
    pub(crate) cli_layer: ConfigLayer,
    pub(crate) pull: bool,
    pub(crate) rebuild: bool,
    pub(crate) no_cache: bool,
    pub(crate) update_features: bool,
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
    _credentials: CredentialRuntime,
}

struct CredentialRuntime {
    _git_credentials: GitCredentialRuntime,
    _github_cli: GithubCliRuntime,
    _ssh_agent: SshAgentRuntime,
    _forward: ForwardRuntime,
    mount_policy: CredentialRuntimeMountPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CredentialRuntimeMountPolicy {
    required_mounts: Vec<UpMountSummary>,
    managed_targets: Vec<String>,
}

impl CredentialRuntimeMountPolicy {
    fn new(required_mounts: Vec<UpMountSummary>) -> Self {
        Self {
            required_mounts,
            managed_targets: DECUNE_MANAGED_RUNTIME_MOUNT_TARGETS
                .iter()
                .map(|target| (*target).to_owned())
                .collect(),
        }
    }

    fn required_mounts(&self) -> &[UpMountSummary] {
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

impl CredentialRuntime {
    fn new(
        git_credentials: GitCredentialRuntime,
        github_cli: GithubCliRuntime,
        ssh_agent: SshAgentRuntime,
        forward: ForwardRuntime,
    ) -> Self {
        let required_mounts = git_credentials
            .mounts()
            .iter()
            .chain(github_cli.mounts())
            .chain(ssh_agent.mounts())
            .chain(forward.mounts())
            .map(|mount| UpMountSummary {
                source: mount.source.clone(),
                target: mount.target.clone(),
                mount_type: mount.mount_type,
                read_only: mount.read_only,
            })
            .collect();

        Self {
            _git_credentials: git_credentials,
            _github_cli: github_cli,
            _ssh_agent: ssh_agent,
            _forward: forward,
            mount_policy: CredentialRuntimeMountPolicy::new(required_mounts),
        }
    }

    fn mount_policy(&self) -> &CredentialRuntimeMountPolicy {
        &self.mount_policy
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

pub(crate) fn default_workspace_folder(workspace: &Workspace) -> String {
    format!("/workspaces/{}", workspace.basename())
}

#[cfg(test)]
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
        false,
        MountResolution::Resolve,
        UpPlanResolution::new(ForwardingResolution::Resolve, false),
    )
}

#[cfg(test)]
pub(crate) fn build_up_plan_with_update_features(
    workspace: &Workspace,
    explicit_config_path: Option<&Path>,
    cli_layer: ConfigLayer,
    update_features: bool,
) -> Result<UpPlan> {
    build_up_plan_inner(
        workspace,
        explicit_config_path,
        cli_layer,
        Vec::new(),
        false,
        MountResolution::Resolve,
        UpPlanResolution::new(ForwardingResolution::Resolve, update_features),
    )
}

#[cfg(test)]
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
        false,
        MountResolution::Resolve,
        UpPlanResolution::new(ForwardingResolution::Resolve, false),
    )
}

fn build_preliminary_up_plan_with_forwarding_resolution(
    workspace: &Workspace,
    explicit_config_path: Option<&Path>,
    cli_layer: ConfigLayer,
    forwarding_resolution: ForwardingResolution,
    update_features: bool,
) -> Result<UpPlan> {
    build_up_plan_inner(
        workspace,
        explicit_config_path,
        cli_layer,
        Vec::new(),
        false,
        MountResolution::DeferConfigMounts,
        UpPlanResolution::new(forwarding_resolution, update_features),
    )
}

fn build_up_plan_with_forwarding_resolution(
    workspace: &Workspace,
    explicit_config_path: Option<&Path>,
    cli_layer: ConfigLayer,
    forwarding_resolution: ForwardingResolution,
    update_features: bool,
) -> Result<UpPlan> {
    build_up_plan_inner(
        workspace,
        explicit_config_path,
        cli_layer,
        Vec::new(),
        false,
        MountResolution::Resolve,
        UpPlanResolution::new(forwarding_resolution, update_features),
    )
}

fn build_up_plan_with_image_metadata_and_forwarding_resolution(
    workspace: &Workspace,
    explicit_config_path: Option<&Path>,
    cli_layer: ConfigLayer,
    image_metadata: Vec<ConfigLayer>,
    ignored_image_metadata_forwarding: bool,
    forwarding_resolution: ForwardingResolution,
    update_features: bool,
) -> Result<UpPlan> {
    build_up_plan_inner(
        workspace,
        explicit_config_path,
        cli_layer,
        image_metadata,
        ignored_image_metadata_forwarding,
        MountResolution::Resolve,
        UpPlanResolution::new(forwarding_resolution, update_features),
    )
}

fn build_up_plan_inner(
    workspace: &Workspace,
    explicit_config_path: Option<&Path>,
    cli_layer: ConfigLayer,
    image_metadata: Vec<ConfigLayer>,
    ignored_image_metadata_forwarding: bool,
    mount_resolution: MountResolution,
    resolution: UpPlanResolution,
) -> Result<UpPlan> {
    let devcontainer_json = DevcontainerJson::load(workspace.root(), explicit_config_path)?;
    let metadata = parse_metadata(devcontainer_json.value().clone())?;
    let devcontainer_layer = match resolution.forwarding {
        ForwardingResolution::Resolve => metadata.to_config_layer()?,
        ForwardingResolution::IgnoreDetached => metadata.to_config_layer_without_forward_ports()?,
    };
    let global_layer = ConfigLayer::from_raw_decune_with_origin(
        load_config_file(workspace.paths().global_config_path())?,
        crate::config::path::ConfigPathOrigin::Global,
    );
    let project_layer = ConfigLayer::from_raw_decune_with_origin(
        load_config_file(workspace.paths().project_config_path())?,
        crate::config::path::ConfigPathOrigin::Project,
    );
    let config_layers = ConfigMergeInput {
        image_metadata,
        global: Some(global_layer),
        devcontainer: Some(devcontainer_layer),
        project: Some(project_layer),
        cli: Some(cli_layer),
        ..ConfigMergeInput::default()
    };
    let config = resolve_config(config_layers.clone());
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
    hash_input.feature_locks = feature_lock_hash_inputs(
        workspace,
        devcontainer_json.path(),
        &config,
        resolution.update_features,
    )?;
    if mount_resolution == MountResolution::Resolve {
        hash_input.resolved_mounts = mount_hash_inputs(&mounts);
    }
    let hash = config_hash(&hash_input);
    let resources = DockerResources::from_workspace(
        workspace,
        hash,
        devcontainer_json.path().display().to_string(),
    );
    let base_image = base_image_source(&config, &resources)?;
    let image = final_image_source(&config, &resources)?;
    let forward_ports = match resolution.forwarding {
        ForwardingResolution::Resolve => resolve_forward_ports(&config.ports.entries)?,
        ForwardingResolution::IgnoreDetached => Vec::new(),
    };
    let ignored_detached_forwarding = resolution.forwarding == ForwardingResolution::IgnoreDetached
        && (ignored_image_metadata_forwarding
            || !metadata.forward_ports().is_empty()
            || !config.ports.entries.is_empty());

    Ok(UpPlan {
        image,
        base_image,
        build_context,
        build_options,
        feature_install: None,
        feature_build_context_dir: None,
        resources,
        config_layers,
        config,
        workspace_folder: workspace_location.workspace_folder,
        mounts,
        forward_ports,
        ignored_detached_forwarding,
    })
}

fn feature_lock_hash_inputs(
    workspace: &Workspace,
    devcontainer_file: &Path,
    config: &ResolvedConfig,
    update_features: bool,
) -> Result<Vec<FeatureLockHashEntry>> {
    if config.features.is_empty() {
        return Ok(Vec::new());
    }

    let devcontainer_dir = devcontainer_file.parent().with_context(|| {
        format!(
            "Failed to resolve devcontainer directory for {}",
            devcontainer_file.display()
        )
    })?;
    let references = config
        .features
        .iter()
        .map(|feature| {
            parse_feature_ref_from_devcontainer_dir(&feature.id, devcontainer_dir)
                .with_context(|| format!("Failed to parse Feature ref: {}", feature.id))
        })
        .collect::<Result<Vec<_>>>()?;

    if update_features {
        return Ok(Vec::new());
    }

    let lock_path = workspace.root().join(".decune").join("features.lock.toml");
    let lock = read_feature_lock_file(&lock_path)?;
    let mut entries = Vec::new();

    for reference in references {
        let _resolved = resolve_locked_feature_ref(&reference, &lock, false);
        let canonical_id = reference.canonical_id().to_owned();

        if let FeatureRef::Oci(reference) = reference
            && let Some(digest) = lock.digest_for_reference(&reference)
        {
            entries.push(FeatureLockHashEntry {
                feature_id: canonical_id,
                digest: digest.to_owned(),
            });
        }
    }

    Ok(entries)
}

pub(crate) async fn run_detached_up(options: UpOptions) -> Result<UpOutcome> {
    let started = ensure_container_started(options, ForwardingResolution::IgnoreDetached).await?;
    warn_about_detached_forwarding(&started.plan);
    let _host_daemon = start_host_daemon_for_up(&started).await?;
    {
        let lifecycle = prepare_up_lifecycle(&started).await?;
        run_container_start_lifecycle_for_up(&started, &lifecycle).await?;
    }
    report_up_success(&started);

    Ok(started.outcome)
}

pub(crate) async fn run_attached_up(options: UpOptions) -> Result<i32> {
    let started = ensure_container_started(options, ForwardingResolution::Resolve).await?;
    let _host_daemon = start_host_daemon_for_up(&started).await?;
    let lifecycle = prepare_up_lifecycle(&started).await?;
    run_container_start_lifecycle_for_up(&started, &lifecycle).await?;
    let forwarding = start_forwarding_for_up(&started).await?;
    let attach_result = async {
        run_attach_lifecycle_for_up(&lifecycle).await?;
        report_up_success(&started);

        attach_shell(
            &started.client,
            &started.plan,
            &started.outcome.container_name,
        )
        .await
    }
    .await;
    stop_forwarding(forwarding).await;

    let exit_code = attach_result?;
    Ok(clamp_exit_code(exit_code))
}

fn warn_about_detached_forwarding(plan: &UpPlan) {
    if plan.ignored_detached_forwarding {
        ui::warn(
            "Port forwarding is ignored in detached mode; use appPort for detached publishing",
        );
    }
}

async fn ensure_container_started(
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
    )?;
    let plan_resolution = UpPlanResolution::new(forwarding_resolution, options.update_features);
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
            plan_resolution,
        )
        .await?;
        let (existing_plan, _) = finalize_up_plan_mounts(
            &client,
            &workspace,
            existing_plan,
            containers.first().and_then(existing_container_image_id),
            containers.first().and_then(existing_container_config_hash),
            Some((options.pull, options.no_cache)),
            options.update_features,
        )
        .await?;
        let (existing_plan, credentials) =
            add_credential_runtime_mounts(existing_plan, workspace.paths().runtime_dir())?;

        match decide_existing_container(
            &containers,
            &existing_plan.resources.config_hash,
            credentials.mount_policy(),
            false,
        )? {
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
                    _credentials: credentials,
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
                    _credentials: credentials,
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
        plan_resolution,
    )
    .await?;
    let (plan, mount_image_prepared) = finalize_up_plan_mounts(
        &client,
        &workspace,
        plan,
        None,
        None,
        Some((options.pull, options.no_cache)),
        options.update_features,
    )
    .await?;
    let (plan, credentials) = add_credential_runtime_mounts(plan, workspace.paths().runtime_dir())?;
    let image_prepared = image_prepared || mount_image_prepared;
    warn_about_deferred_features(&plan.config);

    match decide_existing_container(
        &containers,
        &plan.resources.config_hash,
        credentials.mount_policy(),
        options.rebuild,
    )? {
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
                _credentials: credentials,
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
                _credentials: credentials,
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
                _credentials: credentials,
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
                _credentials: credentials,
            })
        }
    }
}

fn add_credential_runtime_mounts(
    plan: UpPlan,
    runtime_dir: &Path,
) -> Result<(UpPlan, CredentialRuntime)> {
    let ssh_agent = prepare_ssh_agent_runtime(&plan.config)?;
    let github_cli = prepare_github_cli_runtime(&plan.config, runtime_dir)?;
    let forward = prepare_forward_runtime(&plan.forward_ports, runtime_dir)?;
    add_prepared_credential_runtime_mounts(plan, runtime_dir, github_cli, ssh_agent, forward)
}

#[cfg(test)]
fn add_credential_runtime_mounts_with_ssh_socket(
    plan: UpPlan,
    runtime_dir: &Path,
    ssh_auth_sock: Option<&Path>,
) -> Result<(UpPlan, CredentialRuntime)> {
    let ssh_agent = crate::host::credentials::prepare_ssh_agent_runtime_with_socket(
        &plan.config,
        ssh_auth_sock,
    )?;
    let github_cli = crate::host::credentials::prepare_github_cli_runtime_with_token(
        &plan.config,
        runtime_dir,
        None,
    )?;
    let forward = prepare_forward_runtime(&plan.forward_ports, runtime_dir)?;
    add_prepared_credential_runtime_mounts(plan, runtime_dir, github_cli, ssh_agent, forward)
}

#[cfg(test)]
fn add_credential_runtime_mounts_with_inputs(
    plan: UpPlan,
    runtime_dir: &Path,
    ssh_auth_sock: Option<&Path>,
    github_token: Option<&str>,
) -> Result<(UpPlan, CredentialRuntime)> {
    let ssh_agent = crate::host::credentials::prepare_ssh_agent_runtime_with_socket(
        &plan.config,
        ssh_auth_sock,
    )?;
    let github_cli = crate::host::credentials::prepare_github_cli_runtime_with_token(
        &plan.config,
        runtime_dir,
        github_token,
    )?;
    let forward = prepare_forward_runtime(&plan.forward_ports, runtime_dir)?;
    add_prepared_credential_runtime_mounts(plan, runtime_dir, github_cli, ssh_agent, forward)
}

fn add_prepared_credential_runtime_mounts(
    mut plan: UpPlan,
    runtime_dir: &Path,
    github_cli: GithubCliRuntime,
    ssh_agent: SshAgentRuntime,
    forward: ForwardRuntime,
) -> Result<(UpPlan, CredentialRuntime)> {
    let git_credentials = prepare_git_credential_runtime(&plan.config, runtime_dir)?;
    extend_runtime_mounts(&mut plan.mounts, git_credentials.mounts());
    extend_runtime_mounts(&mut plan.mounts, github_cli.mounts());
    extend_runtime_mounts(&mut plan.mounts, ssh_agent.mounts());
    extend_runtime_mounts(&mut plan.mounts, forward.mounts());
    plan.config
        .devcontainer
        .container_env
        .extend(github_cli.container_env().clone());
    plan.config
        .devcontainer
        .container_env
        .extend(ssh_agent.container_env().clone());

    Ok((
        plan,
        CredentialRuntime::new(git_credentials, github_cli, ssh_agent, forward),
    ))
}

fn extend_runtime_mounts(mounts: &mut Vec<DockerMountSpec>, runtime_mounts: &[DockerMountSpec]) {
    for mount in runtime_mounts {
        let target = normalize_container_path(&mount.target);
        if mounts
            .iter()
            .any(|existing| normalize_container_path(&existing.target) == target)
        {
            continue;
        }
        mounts.push(mount.clone());
    }
}

async fn build_existing_container_decision_plan(
    client: &DockerClient,
    workspace: &Workspace,
    explicit_config_path: Option<&Path>,
    cli_layer: ConfigLayer,
    existing_container_image_id: Option<&str>,
    preliminary_plan: &UpPlan,
    resolution: UpPlanResolution,
) -> Result<UpPlan> {
    if preliminary_plan.build_context.is_some() {
        let image = existing_container_image_id.unwrap_or(&preliminary_plan.image);
        warn_about_unsupported_dockerfile_image_metadata(client, image).await?;
        return build_up_plan_with_forwarding_resolution(
            workspace,
            explicit_config_path,
            cli_layer,
            resolution.forwarding,
            resolution.update_features,
        );
    }

    let include_forward_ports = resolution.forwarding == ForwardingResolution::Resolve;
    let image_metadata = match image_devcontainer_metadata_layers_if_present_with_forward_ports(
        client,
        &preliminary_plan.base_image,
        include_forward_ports,
    )
    .await?
    {
        Some(image_metadata) => image_metadata,
        None => {
            let Some(image_id) = existing_container_image_id else {
                return build_up_plan_with_forwarding_resolution(
                    workspace,
                    explicit_config_path,
                    cli_layer,
                    resolution.forwarding,
                    resolution.update_features,
                );
            };
            let Some(image_metadata) =
                image_devcontainer_metadata_layers_if_present_with_forward_ports(
                    client,
                    image_id,
                    include_forward_ports,
                )
                .await?
            else {
                return build_up_plan_with_forwarding_resolution(
                    workspace,
                    explicit_config_path,
                    cli_layer,
                    resolution.forwarding,
                    resolution.update_features,
                );
            };
            image_metadata
        }
    };

    if image_metadata.layers.is_empty() {
        return build_up_plan_with_forwarding_resolution(
            workspace,
            explicit_config_path,
            cli_layer,
            resolution.forwarding,
            resolution.update_features,
        );
    }

    build_up_plan_with_image_metadata_and_forwarding_resolution(
        workspace,
        explicit_config_path,
        cli_layer,
        image_metadata.layers,
        !include_forward_ports && image_metadata.has_forward_ports,
        resolution.forwarding,
        resolution.update_features,
    )
}

async fn prepare_image_based_metadata(
    client: &DockerClient,
    workspace: &Workspace,
    explicit_config_path: Option<&Path>,
    cli_layer: ConfigLayer,
    preliminary_plan: UpPlan,
    pull: bool,
    resolution: UpPlanResolution,
) -> Result<(UpPlan, bool)> {
    if preliminary_plan.build_context.is_some() {
        return Ok((
            build_up_plan_with_forwarding_resolution(
                workspace,
                explicit_config_path,
                cli_layer,
                resolution.forwarding,
                resolution.update_features,
            )?,
            false,
        ));
    }

    ensure_image(
        client,
        &preliminary_plan.base_image,
        if pull {
            PullPolicy::Always
        } else {
            PullPolicy::Missing
        },
    )
    .await?;
    let include_forward_ports = resolution.forwarding == ForwardingResolution::Resolve;
    let image_metadata = image_devcontainer_metadata_layers_with_forward_ports(
        client,
        &preliminary_plan.base_image,
        include_forward_ports,
    )
    .await?;
    if image_metadata.layers.is_empty() {
        return Ok((
            build_up_plan_with_forwarding_resolution(
                workspace,
                explicit_config_path,
                cli_layer,
                resolution.forwarding,
                resolution.update_features,
            )?,
            true,
        ));
    }

    let plan = build_up_plan_with_image_metadata_and_forwarding_resolution(
        workspace,
        explicit_config_path,
        cli_layer,
        image_metadata.layers,
        !include_forward_ports && image_metadata.has_forward_ports,
        resolution.forwarding,
        resolution.update_features,
    )?;

    Ok((plan, true))
}

async fn finalize_up_plan_mounts(
    client: &DockerClient,
    workspace: &Workspace,
    mut plan: UpPlan,
    remote_user_image: Option<&str>,
    existing_container_config_hash: Option<&str>,
    build_for_lookup: Option<(bool, bool)>,
    update_features: bool,
) -> Result<(UpPlan, bool)> {
    let using_existing_remote_user_image = remote_user_image.is_some();
    let mut lookup_image = remote_user_image.map(ToOwned::to_owned);
    let mut lookup_base_image = None;
    let mut image_prepared = false;
    plan = prepare_feature_metadata_for_plan(workspace, plan, update_features).await?;
    if lookup_image.is_none() {
        if plan_requires_workspace_layer(&plan) {
            let Some((pull, no_cache)) = build_for_lookup else {
                return Ok((plan, false));
            };
            prepare_base_image_for_plan(client, &plan, pull, no_cache).await?;
            lookup_base_image = Some(plan.base_image.clone());
            build_feature_layer_image(client, &plan, no_cache).await?;
            lookup_image = Some(plan.image.clone());
            image_prepared = true;
        } else if let Some(context) = plan.build_context.clone() {
            let Some((pull, no_cache)) = build_for_lookup else {
                return Ok((plan, false));
            };
            let mut build_options = plan.build_options.clone();
            build_options.pull = pull;
            build_options.no_cache = no_cache;
            build_image(
                client,
                DockerBuildInput {
                    image_tag: plan.base_image.clone(),
                    labels: plan.resources.labels.clone().into_iter().collect(),
                    context,
                    options: build_options,
                },
            )
            .await?;
            lookup_image = Some(plan.base_image.clone());
            image_prepared = true;
        } else {
            lookup_image = Some(plan.base_image.clone());
        }
    };
    let mut lookup_image = lookup_image.expect("lookup image must be set");
    let lookup = ImageLookupPreparation {
        image: &mut lookup_image,
        remote_user_image,
        base_image: &mut lookup_base_image,
        image_prepared: &mut image_prepared,
        build_options: if using_existing_remote_user_image {
            None
        } else {
            build_for_lookup
        },
        command_probe_build_options: build_for_lookup,
    };
    plan = Box::pin(maybe_auto_add_github_cli_feature_to_plan(
        client,
        workspace,
        plan,
        lookup,
        existing_container_config_hash,
        update_features,
    ))
    .await?;
    if plan.config.features.is_empty() {
        remove_feature_lock_file(&workspace.root().join(".decune").join("features.lock.toml"))?;
    }
    plan = Box::pin(finalize_mounts_and_resources_for_plan(
        client,
        workspace,
        plan,
        &lookup_image,
        update_features,
    ))
    .await?;

    if image_prepared && plan_requires_workspace_layer(&plan) {
        if let Some((pull, no_cache)) = build_for_lookup {
            prepare_base_image_for_plan(client, &plan, pull, no_cache).await?;
            build_feature_layer_image(client, &plan, no_cache).await?;
        }
        if plan.image != lookup_image {
            remove_image(client, &lookup_image, false).await?;
        }
        if let Some(lookup_base_image) = lookup_base_image
            && lookup_base_image != plan.base_image
        {
            remove_image(client, &lookup_base_image, false).await?;
        }
    } else if image_prepared && plan.image != lookup_image {
        tag_image(client, &lookup_image, &plan.image).await?;
        remove_image(client, &lookup_image, false).await?;
    }

    Ok((plan, image_prepared))
}

async fn finalize_mounts_and_resources_for_plan(
    client: &DockerClient,
    workspace: &Workspace,
    mut plan: UpPlan,
    lookup_image: &str,
    update_features: bool,
) -> Result<UpPlan> {
    let remote_user = resolve_remote_user_from_image(
        client,
        lookup_image,
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
    let devcontainer_file = Path::new(&plan.resources.labels["devcontainer.config_file"]);
    hash_input.feature_locks = match &plan.feature_install {
        Some(feature_install) => feature_install.lock_entries.clone(),
        None => {
            feature_lock_hash_inputs(workspace, devcontainer_file, &plan.config, update_features)?
        }
    };
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
    let image = final_image_source(&plan.config, &resources)?;
    let base_image = base_image_source(&plan.config, &resources)?;

    plan.image = image;
    plan.base_image = base_image;
    plan.resources = resources;
    plan.workspace_folder = workspace_location.workspace_folder;
    plan.mounts = mounts;

    Ok(plan)
}

async fn prepare_command_probe_image_for_plan(
    client: &DockerClient,
    plan: &UpPlan,
    remote_user_image: Option<&str>,
    build_for_lookup: Option<(bool, bool)>,
) -> Result<Option<CommandProbeImage>> {
    if remote_user_image.is_none() {
        return Ok(None);
    }

    if plan_requires_workspace_layer(plan) {
        let Some((pull, no_cache)) = build_for_lookup else {
            return Ok(None);
        };
        prepare_base_image_for_plan(client, plan, pull, no_cache).await?;
        build_feature_layer_image(client, plan, no_cache).await?;
        return Ok(Some(CommandProbeImage {
            image: plan.image.clone(),
            uses_existing_image: false,
        }));
    }

    if let Some(context) = plan.build_context.clone() {
        let Some((pull, no_cache)) = build_for_lookup else {
            return Ok(None);
        };
        let mut build_options = plan.build_options.clone();
        build_options.pull = pull;
        build_options.no_cache = no_cache;
        build_image(
            client,
            DockerBuildInput {
                image_tag: plan.base_image.clone(),
                labels: plan.resources.labels.clone().into_iter().collect(),
                context,
                options: build_options,
            },
        )
        .await?;
        return Ok(Some(CommandProbeImage {
            image: plan.base_image.clone(),
            uses_existing_image: false,
        }));
    }

    match local_image_presence(client, &plan.base_image).await? {
        LocalImagePresence::Present => Ok(Some(CommandProbeImage {
            image: plan.base_image.clone(),
            uses_existing_image: false,
        })),
        LocalImagePresence::Missing => Ok(Some(CommandProbeImage {
            image: remote_user_image
                .expect("remote user image must be set")
                .to_owned(),
            uses_existing_image: true,
        })),
    }
}

async fn prepare_feature_metadata_for_plan(
    workspace: &Workspace,
    mut plan: UpPlan,
    update_features: bool,
) -> Result<UpPlan> {
    if plan.feature_install.is_some() {
        return Ok(plan);
    }
    if plan.config.features.is_empty() {
        if !plan.config.devcontainer.entrypoints.is_empty() {
            plan.feature_build_context_dir =
                Some(workspace.paths().cache_dir().join("feature-build-context"));
        }
        return Ok(plan);
    }

    let features = plan.config.features.clone();
    let override_feature_install_order = plan
        .config
        .devcontainer
        .override_feature_install_order
        .clone();
    let devcontainer_file = PathBuf::from(&plan.resources.labels["devcontainer.config_file"]);
    let workspace_root = workspace.root().to_path_buf();
    let feature_archive_cache_dir = workspace.paths().feature_archive_cache_dir().to_path_buf();
    let feature_extract_dir = workspace
        .paths()
        .cache_dir()
        .join("features")
        .join("extracted");
    let Some(feature_install) = tokio::task::spawn_blocking(move || {
        prepare_feature_install_plan(
            &features,
            &devcontainer_file,
            &workspace_root,
            &feature_archive_cache_dir,
            &feature_extract_dir,
            &override_feature_install_order,
            update_features,
        )
    })
    .await
    .context("Feature install planning task failed")??
    else {
        return Ok(plan);
    };
    plan.config_layers.feature_metadata = feature_install.metadata_layers.clone();
    plan.config = resolve_config(plan.config_layers.clone());
    plan.feature_install = Some(feature_install);
    plan.feature_build_context_dir =
        Some(workspace.paths().cache_dir().join("feature-build-context"));

    Ok(plan)
}

async fn maybe_auto_add_github_cli_feature_to_plan(
    client: &DockerClient,
    workspace: &Workspace,
    mut plan: UpPlan,
    lookup: ImageLookupPreparation<'_>,
    existing_container_config_hash: Option<&str>,
    update_features: bool,
) -> Result<UpPlan> {
    if config_has_github_cli_feature(&plan.config) {
        return Ok(plan);
    }

    let host_token_available = host_github_auth_token_available()?;
    if !should_auto_add_github_cli_feature(&plan.config, host_token_available, false) {
        return Ok(plan);
    }

    let command_probe_image = prepare_command_probe_image_for_plan(
        client,
        &plan,
        lookup.remote_user_image,
        lookup.command_probe_build_options,
    )
    .await?
    .unwrap_or(CommandProbeImage {
        image: (*lookup.image).clone(),
        uses_existing_image: false,
    });
    let image_has_gh = image_has_command(
        client,
        &command_probe_image.image,
        "gh",
        &plan.config.devcontainer.container_env,
    )
    .await?;
    if image_has_gh && command_probe_image.uses_existing_image {
        return Box::pin(choose_github_cli_feature_plan_for_existing_image_probe(
            client,
            workspace,
            plan,
            &lookup,
            existing_container_config_hash,
            update_features,
        ))
        .await;
    }

    if !should_auto_add_github_cli_feature(&plan.config, host_token_available, image_has_gh) {
        return Ok(plan);
    }

    ui::info("Adding GitHub CLI Feature for GitHub token forwarding");
    plan = add_github_cli_feature_to_plan(plan)?;
    plan = prepare_feature_metadata_for_plan(workspace, plan, update_features).await?;

    if let Some((pull, no_cache)) = lookup.build_options {
        prepare_base_image_for_plan(client, &plan, pull, no_cache).await?;
        *lookup.base_image = Some(plan.base_image.clone());
        build_feature_layer_image(client, &plan, no_cache).await?;
        *lookup.image = plan.image.clone();
        *lookup.image_prepared = true;
    }

    Ok(plan)
}

async fn choose_github_cli_feature_plan_for_existing_image_probe(
    client: &DockerClient,
    workspace: &Workspace,
    plan: UpPlan,
    lookup: &ImageLookupPreparation<'_>,
    existing_container_config_hash: Option<&str>,
    update_features: bool,
) -> Result<UpPlan> {
    let Some(existing_container_config_hash) = existing_container_config_hash else {
        return Ok(plan);
    };

    let finalized_plan = Box::pin(finalize_mounts_and_resources_for_plan(
        client,
        workspace,
        plan.clone(),
        lookup.image,
        update_features,
    ))
    .await?;
    if finalized_plan.resources.config_hash == existing_container_config_hash {
        return Ok(plan);
    }

    if !should_auto_add_github_cli_feature(&plan.config, true, false) {
        return Ok(plan);
    }

    let candidate = add_github_cli_feature_to_plan(plan.clone())?;
    let candidate =
        prepare_feature_metadata_for_plan(workspace, candidate, update_features).await?;
    let finalized_candidate = Box::pin(finalize_mounts_and_resources_for_plan(
        client,
        workspace,
        candidate.clone(),
        lookup.image,
        update_features,
    ))
    .await?;
    if finalized_candidate.resources.config_hash == existing_container_config_hash {
        return Ok(candidate);
    }

    Ok(plan)
}

fn should_auto_add_github_cli_feature(
    config: &ResolvedConfig,
    host_token_available: bool,
    image_has_gh: bool,
) -> bool {
    config.credentials.github.enabled
        && config.credentials.github.mode == GithubCredentialsMode::GhTokenFile
        && config.credentials.github.install_feature_if_missing
        && host_token_available
        && !image_has_gh
        && !config_has_github_cli_feature(config)
}

fn config_has_github_cli_feature(config: &ResolvedConfig) -> bool {
    config
        .features
        .iter()
        .any(|feature| feature.canonical_id == GITHUB_CLI_FEATURE_CANONICAL_ID)
}

fn add_github_cli_feature_to_plan(mut plan: UpPlan) -> Result<UpPlan> {
    if config_has_github_cli_feature(&plan.config) {
        return Ok(plan);
    }

    let mut cli_layer = plan.config_layers.cli.take().unwrap_or_default();
    cli_layer
        .features
        .push(LayerFeature::new(GITHUB_CLI_FEATURE_REF.to_owned()));
    plan.config_layers.cli = Some(cli_layer);
    plan.config = resolve_config(plan.config_layers.clone());
    plan.feature_install = None;
    plan.image = final_image_source(&plan.config, &plan.resources)?;
    plan.base_image = base_image_source(&plan.config, &plan.resources)?;

    Ok(plan)
}

async fn image_has_command(
    client: &DockerClient,
    image: &str,
    command: &str,
    env: &BTreeMap<String, String>,
) -> Result<bool> {
    let probe_id = IMAGE_COMMAND_PROBE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let name = format!("decune-command-probe-{}-{probe_id}", std::process::id());
    let spec = ContainerCreateSpec {
        image: image.to_owned(),
        name: name.clone(),
        entrypoint: Some(vec!["/bin/sh".to_owned()]),
        command: Some(vec![
            "-c".to_owned(),
            format!("command -v {command} >/dev/null 2>&1"),
        ]),
        labels: BTreeMap::new(),
        env: env.clone(),
        working_dir: None,
        user: None,
        mounts: Vec::new(),
        publish_ports: Vec::new(),
        host_config: ContainerHostConfig::default(),
    };
    let container_id = create_container(client, &spec).await?;
    let result = async {
        start_container(client, &container_id).await?;
        wait_for_container_exit_code(client, &container_id).await
    }
    .await;
    let cleanup = remove_container(client, &container_id, true, true).await;

    match (result, cleanup) {
        (Ok(exit_code), Ok(())) => Ok(exit_code == 0),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup_error)) => Err(cleanup_error)
            .with_context(|| format!("Failed to remove command probe container: {name}")),
        (Err(error), Err(cleanup_error)) => Err(error).context(format!(
            "Failed to remove command probe container {name}: {cleanup_error:#}"
        )),
    }
}

async fn wait_for_container_exit_code(client: &DockerClient, container: &str) -> Result<i64> {
    let options = WaitContainerOptionsBuilder::default()
        .condition("not-running")
        .build();
    match client
        .raw()
        .wait_container(container, Some(options))
        .try_next()
        .await
    {
        Ok(Some(response)) => Ok(response.status_code),
        Ok(None) => Err(anyhow::anyhow!(
            "Docker container wait ended without a response: {container}"
        )),
        Err(bollard::errors::Error::DockerContainerWaitError { code, .. }) => Ok(code),
        Err(error) => {
            Err(error).with_context(|| format!("Failed to wait for Docker container: {container}"))
        }
    }
}

async fn prepare_base_image_for_plan(
    client: &DockerClient,
    plan: &UpPlan,
    pull: bool,
    no_cache: bool,
) -> Result<()> {
    if let Some(context) = plan.build_context.clone() {
        let mut build_options = plan.build_options.clone();
        build_options.pull = pull;
        build_options.no_cache = no_cache;
        build_image(
            client,
            DockerBuildInput {
                image_tag: plan.base_image.clone(),
                labels: plan.resources.labels.clone().into_iter().collect(),
                context,
                options: build_options,
            },
        )
        .await?;
        warn_about_unsupported_dockerfile_image_metadata(client, &plan.base_image).await?;
    } else {
        ensure_image(
            client,
            &plan.base_image,
            if pull {
                PullPolicy::Always
            } else {
                PullPolicy::Missing
            },
        )
        .await?;
    }

    Ok(())
}

async fn build_feature_layer_image(
    client: &DockerClient,
    plan: &UpPlan,
    no_cache: bool,
) -> Result<()> {
    if !plan_requires_workspace_layer(plan) {
        return Ok(());
    }
    let feature_build_context_dir = plan
        .feature_build_context_dir
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Feature build context directory was not prepared"))?;
    let final_user = image_config_user(client, &plan.base_image)
        .await?
        .unwrap_or_else(|| "root".to_owned());
    let install_env = feature_install_env(plan, &final_user);
    let devcontainer_id = plan
        .resources
        .labels
        .get("decune.workspace_id")
        .cloned()
        .context("Feature layer build requires a workspace id label")?;
    let context = prepare_feature_layer_build_context(&FeatureLayerBuildInput {
        base_image: plan.base_image.clone(),
        devcontainer_id,
        final_user,
        entrypoints: plan.config.devcontainer.entrypoints.clone(),
        install_env,
        context_dir: feature_build_context_dir.clone(),
        features: plan
            .feature_install
            .as_ref()
            .map(|feature_install| {
                feature_install
                    .entries
                    .iter()
                    .map(|entry| FeatureLayerBuildFeature {
                        id: entry.feature.canonical_id.clone(),
                        source_dir: entry.source_dir.clone(),
                        option_env: entry.option_env.clone(),
                        container_env: entry.container_env.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
    })?;
    build_image(
        client,
        DockerBuildInput {
            image_tag: plan.image.clone(),
            labels: plan.resources.labels.clone().into_iter().collect(),
            context,
            options: DockerBuildOptions {
                no_cache,
                ..DockerBuildOptions::default()
            },
        },
    )
    .await
}

fn plan_requires_workspace_layer(plan: &UpPlan) -> bool {
    plan.feature_install.is_some() || config_requires_workspace_layer(&plan.config)
}

fn config_requires_workspace_layer(config: &ResolvedConfig) -> bool {
    !config.features.is_empty() || !config.devcontainer.entrypoints.is_empty()
}

fn feature_install_env(plan: &UpPlan, image_user: &str) -> BTreeMap<String, String> {
    let container_user = plan
        .config
        .devcontainer
        .container_user
        .as_deref()
        .unwrap_or(image_user)
        .to_owned();
    let remote_user = plan
        .config
        .devcontainer
        .remote_user
        .clone()
        .unwrap_or_else(|| container_user.clone());

    BTreeMap::from([
        ("_CONTAINER_USER".to_owned(), container_user),
        ("_REMOTE_USER".to_owned(), remote_user),
    ])
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
    if plan_requires_workspace_layer(plan) {
        if !image_prepared {
            prepare_base_image_for_plan(client, plan, pull, no_cache).await?;
            build_feature_layer_image(client, plan, no_cache).await?;
        }
    } else if let Some(context) = plan.build_context.clone() {
        if !image_prepared {
            let mut build_options = plan.build_options.clone();
            build_options.pull = pull;
            build_options.no_cache = no_cache;
            build_image(
                client,
                DockerBuildInput {
                    image_tag: plan.base_image.clone(),
                    labels: plan.resources.labels.clone().into_iter().collect(),
                    context,
                    options: build_options,
                },
            )
            .await?;
        }
        warn_about_unsupported_dockerfile_image_metadata(client, &plan.base_image).await?;
    } else if !image_prepared {
        ensure_image(
            client,
            &plan.base_image,
            if pull {
                PullPolicy::Always
            } else {
                PullPolicy::Missing
            },
        )
        .await?;
    }

    let has_feature_entrypoints = !plan.config.devcontainer.entrypoints.is_empty();
    let (entrypoint, command) = if has_feature_entrypoints {
        let command = if plan.config.devcontainer.override_command {
            let (entrypoint, command) = devcontainer_keepalive_command();
            let mut wrapped_command = vec![entrypoint.join(" ")];
            wrapped_command.extend(command);
            Some(wrapped_command)
        } else {
            let startup = image_startup_command(client, &plan.image).await?;
            let mut wrapped_command = startup.entrypoint;
            wrapped_command.extend(startup.command);
            (!wrapped_command.is_empty()).then_some(wrapped_command)
        };
        (Some(vec![FEATURE_ENTRYPOINT_WRAPPER.to_owned()]), command)
    } else if plan.config.devcontainer.override_command {
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
        runtime_dir: started.workspace.paths().runtime_dir(),
        remote_user,
    })
    .await
}

async fn start_host_daemon_for_up(started: &StartedUpContainer) -> Result<HostDaemon> {
    let remote_user = resolve_remote_user(
        &started.client,
        &started.outcome.container_name,
        RemoteUserResolveInput {
            explicit_remote_user: started.plan.config.devcontainer.remote_user.as_deref(),
            image_metadata_remote_user: None,
        },
    )
    .await?;

    let daemon = HostDaemon::start_for_remote_user(
        started.workspace.paths().runtime_dir(),
        remote_user.uid,
        remote_user.gid,
    )
    .await
    .with_context(|| {
        format!(
            "Failed to start host daemon for workspace: {}",
            started.workspace.id()
        )
    })?;
    let _socket_path = daemon.socket_path();

    Ok(daemon)
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

async fn start_forwarding_for_up(started: &StartedUpContainer) -> Result<Option<ForwardSession>> {
    let auto_forward = AutoForwardConfig::from_config(&started.plan.config);
    if started.plan.forward_ports.is_empty() && auto_forward.is_none() {
        return Ok(None);
    }
    if started.plan.forward_ports.is_empty() && auto_forward.is_some() {
        let arch = match detect_container_arch_for_forward_agent(
            &started.client,
            &started.outcome.container_name,
        )
        .await
        {
            Ok(arch) => arch,
            Err(error) => {
                ui::warn(&format!(
                    "Automatic port forwarding is disabled because the container architecture could not be detected: {error:#}"
                ));
                return Ok(None);
            }
        };
        if let ForwardAgentStartDecision::SkipAutoWithWarning(warning) =
            decide_forward_agent_start(false, true, arch.as_deref())
        {
            ui::warn(&warning);
            return Ok(None);
        }
        if let Some(arch) = arch.as_deref()
            && !forward_agent_tool_exists_for_arch(started.workspace.paths().runtime_dir(), arch)
        {
            ui::warn(&format!(
                "Automatic port forwarding is disabled because the port forwarding agent artifact is not available for the container architecture: {arch}"
            ));
            return Ok(None);
        }
    }

    let secret = new_forward_agent_secret()?;
    let agent_exec_id = exec_detached(
        &started.client,
        &started.outcome.container_name,
        &forward_agent_command(&started.plan.forward_ports, &secret),
    )
    .await
    .with_context(|| {
        format!(
            "Failed to start port forwarding agent in container: {}",
            started.outcome.container_name
        )
    })?;
    let agent_socket_path =
        wait_for_forward_agent_with_status(started.workspace.paths().runtime_dir(), || async {
            let inspect = inspect_exec(
                &started.client,
                &agent_exec_id,
                &started.outcome.container_name,
            )
            .await?;
            Ok(
                if inspect.running == Some(false) || inspect.exit_code.is_some() {
                    ForwardAgentStatus::Exited {
                        exit_code: inspect.exit_code,
                    }
                } else {
                    ForwardAgentStatus::Running
                },
            )
        })
        .await
        .with_context(|| {
            format!(
                "Failed to wait for port forwarding agent in container: {}",
                started.outcome.container_name
            )
        })?;
    let session = start_forward_session_with_auto(
        &started.plan.forward_ports,
        auto_forward,
        agent_socket_path,
        secret,
    )
    .await
    .context("Failed to start port forwarding listeners")?;

    Ok(Some(session))
}

async fn stop_forwarding(forwarding: Option<ForwardSession>) {
    if let Some(session) = forwarding {
        session.stop().await;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ForwardAgentStartDecision {
    Start,
    SkipAutoWithWarning(String),
}

fn decide_forward_agent_start(
    has_manual_forward_ports: bool,
    auto_forward_enabled: bool,
    container_arch: Option<&str>,
) -> ForwardAgentStartDecision {
    if has_manual_forward_ports || !auto_forward_enabled {
        return ForwardAgentStartDecision::Start;
    }

    match container_arch.map(str::trim) {
        Some("x86_64" | "amd64" | "aarch64" | "arm64") => ForwardAgentStartDecision::Start,
        Some(arch) if !arch.is_empty() => ForwardAgentStartDecision::SkipAutoWithWarning(format!(
            "Automatic port forwarding is disabled because the container architecture is not supported by the port forwarding agent: {arch}"
        )),
        _ => ForwardAgentStartDecision::SkipAutoWithWarning(
            "Automatic port forwarding is disabled because the container architecture could not be detected".to_owned(),
        ),
    }
}

fn forward_agent_tool_exists_for_arch(runtime_dir: &Path, arch: &str) -> bool {
    let file_name = match arch.trim() {
        "x86_64" | "amd64" => "decune-forward-agent-linux-amd64",
        "aarch64" | "arm64" => "decune-forward-agent-linux-arm64",
        _ => return false,
    };
    runtime_dir.join(file_name).is_file()
}

async fn detect_container_arch_for_forward_agent(
    client: &DockerClient,
    container_name: &str,
) -> Result<Option<String>> {
    let output = exec_capture(
        client,
        container_name,
        &ExecCommandSpec {
            command: vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                "uname -m 2>/dev/null || true".to_owned(),
            ],
            user: Some("0".to_owned()),
            working_dir: None,
            env: BTreeMap::new(),
            tty: false,
        },
    )
    .await?;
    let arch = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok((!arch.is_empty()).then_some(arch))
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

fn final_image_source(config: &ResolvedConfig, resources: &DockerResources) -> Result<String> {
    if config_requires_workspace_layer(config) {
        return Ok(resources.image_tag.clone());
    }

    match &config.devcontainer.source {
        Some(ResolvedDevcontainerSource::Image(image)) => Ok(image.clone()),
        Some(ResolvedDevcontainerSource::Dockerfile(_)) => Ok(resources.image_tag.clone()),
        None => bail!("Devcontainer image is required"),
    }
}

fn base_image_source(config: &ResolvedConfig, resources: &DockerResources) -> Result<String> {
    match &config.devcontainer.source {
        Some(ResolvedDevcontainerSource::Image(image)) => Ok(image.clone()),
        Some(ResolvedDevcontainerSource::Dockerfile(_))
            if config_requires_workspace_layer(config) =>
        {
            Ok(format!("{}-base", resources.image_tag))
        }
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
    let mounts = container.mounts.map(|mounts| {
        mounts
            .into_iter()
            .filter_map(|mount| {
                let bollard::models::MountPoint {
                    typ,
                    source,
                    destination,
                    rw,
                    ..
                } = mount;
                let read_only = !rw.unwrap_or(true);
                let mount_type = mount_type_from_summary(typ.as_deref())?;
                destination.map(|target| UpMountSummary {
                    source,
                    target,
                    mount_type,
                    read_only,
                })
            })
            .collect()
    });
    let running = container
        .state
        .is_some_and(|state| state.to_string() == "running");

    Some(UpContainerSummary {
        id,
        name,
        image_id: container.image_id,
        config_hash,
        mounts,
        running,
    })
}

fn mount_type_from_summary(value: Option<&str>) -> Option<MountType> {
    match value {
        Some("bind") => Some(MountType::Bind),
        Some("volume") => Some(MountType::Volume),
        Some("tmpfs") => Some(MountType::Tmpfs),
        _ => None,
    }
}

fn existing_container_image_id(container: &UpContainerSummary) -> Option<&str> {
    container
        .image_id
        .as_deref()
        .filter(|image_id| !image_id.trim().is_empty())
}

fn existing_container_config_hash(container: &UpContainerSummary) -> Option<&str> {
    container
        .config_hash
        .as_deref()
        .filter(|config_hash| !config_hash.trim().is_empty())
}

fn warn_about_deferred_features(config: &ResolvedConfig) {
    let _ = config;
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
    use std::{
        collections::BTreeMap, fs, net::TcpListener, ops::Deref, os::unix::net::UnixListener,
        path::PathBuf,
    };

    use anyhow::Context;
    use bollard::models::{
        ContainerSummary, ContainerSummaryStateEnum, MountBindOptions,
        MountBindOptionsPropagationEnum, MountPoint, MountVolumeOptions,
    };

    use crate::config::layer::{LayerDevcontainerMetadata, LayerDevcontainerSource};
    use crate::config::resolved::{
        ResolvedConfig, ResolvedDevcontainerSource, ResolvedPublishPort,
    };
    use crate::config::types::{GitHttpsMode, GithubCredentialsMode, MountType, PortProtocol};
    use crate::config::{ConfigHashInput, ConfigLayer, ConfigMergeInput, config_hash};
    use crate::docker::client::DockerClient;
    use crate::docker::container::{remove_container, stop_container};
    use crate::docker::exec::{ExecCommandSpec, exec_capture};
    use crate::docker::image::{PullPolicy, ensure_image, remove_image};
    use crate::docker::mounts::DockerMountSpec;
    use crate::docker::ports::ResolvedForwardPort;
    use crate::docker::resource::DockerResources;
    use crate::host::credentials::{
        GITHUB_CLI_CONFIG_TARGET, GITHUB_CLI_TOKEN_DIR_TARGET, SSH_AGENT_SOCKET_TARGET,
    };
    use crate::workspace::Workspace;

    use super::{
        CredentialRuntimeMountPolicy, DECUNE_RUNTIME_TARGET, ExistingContainerDecision,
        ForwardingResolution, UpContainerSummary, UpMountSummary, UpOptions, UpPlan,
        add_credential_runtime_mounts_with_inputs, add_credential_runtime_mounts_with_ssh_socket,
        add_github_cli_feature_to_plan, build_up_plan, build_up_plan_with_forwarding_resolution,
        build_up_plan_with_image_metadata, build_up_plan_with_update_features, container_summary,
        create_and_start_container, decide_existing_container, default_workspace_folder,
        first_successful_shell_candidate, list_workspace_containers, mount_hash_inputs,
        run_attached_up, run_detached_up, shell_command_candidates,
        should_auto_add_github_cli_feature,
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
            mounts: Some(vec![mount_summary(None, "/workspaces/project")]),
            running: true,
        };

        let decision = decide_existing_container(
            &[container.clone()],
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
            mounts: Some(vec![mount_summary(
                Some("/tmp/agent-a.sock"),
                SSH_AGENT_SOCKET_TARGET,
            )]),
            running: true,
        };

        let decision = decide_existing_container(
            &[container.clone()],
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
            mounts: Some(vec![mount_summary_with_type(
                Some("/tmp/gh-config"),
                GITHUB_CLI_CONFIG_TARGET,
                MountType::Bind,
            )]),
            running: true,
        };

        let decision = decide_existing_container(
            &[container.clone()],
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
            mounts: Some(vec![mount_summary_with_type(
                None,
                GITHUB_CLI_CONFIG_TARGET,
                MountType::Tmpfs,
            )]),
            running: true,
        };

        let decision = decide_existing_container(
            &[container],
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
            mounts: Some(vec![mount_summary_with_type_and_read_only(
                Some("/tmp/gh-token"),
                "/run/decune/gh-token",
                MountType::Bind,
                false,
            )]),
            running: true,
        };

        let decision = decide_existing_container(
            &[container.clone()],
            "hash123",
            &mount_policy(&[mount_summary_with_type_and_read_only(
                Some("/tmp/gh-token"),
                "/run/decune/gh-token",
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
    fn existing_container_decision_reuses_when_required_read_only_mount_matches() {
        let container = UpContainerSummary {
            id: "container-id".to_owned(),
            name: "decune-project-abc123".to_owned(),
            image_id: None,
            config_hash: Some("hash123".to_owned()),
            mounts: Some(vec![mount_summary_with_type_and_read_only(
                Some("/tmp/gh-token"),
                "/run/decune/gh-token",
                MountType::Bind,
                true,
            )]),
            running: true,
        };

        let decision = decide_existing_container(
            &[container],
            "hash123",
            &mount_policy(&[mount_summary_with_type_and_read_only(
                Some("/tmp/gh-token"),
                "/run/decune/gh-token",
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
            mounts: Some(vec![mount_summary(
                Some("/tmp/agent-a.sock"),
                SSH_AGENT_SOCKET_TARGET,
            )]),
            running: true,
        };

        let decision =
            decide_existing_container(&[container.clone()], "hash123", &mount_policy(&[]), false)
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
            mounts: Some(vec![
                mount_summary_with_type_and_read_only(
                    Some("/tmp/gh-token"),
                    GITHUB_CLI_TOKEN_DIR_TARGET,
                    MountType::Bind,
                    true,
                ),
                mount_summary_with_type(None, GITHUB_CLI_CONFIG_TARGET, MountType::Tmpfs),
            ]),
            running: true,
        };

        let decision =
            decide_existing_container(&[container.clone()], "hash123", &mount_policy(&[]), false)
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
        let summary = container_summary(ContainerSummary {
            id: Some("container-id".to_owned()),
            names: Some(vec!["/decune-project-abc123".to_owned()]),
            state: Some(ContainerSummaryStateEnum::RUNNING),
            mounts: Some(vec![
                MountPoint {
                    typ: Some("bind".to_owned()),
                    source: Some("/tmp/gh-token".to_owned()),
                    destination: Some("/run/decune/gh-token".to_owned()),
                    rw: Some(false),
                    ..MountPoint::default()
                },
                MountPoint {
                    typ: Some("bind".to_owned()),
                    source: Some("/tmp/agent.sock".to_owned()),
                    destination: Some(SSH_AGENT_SOCKET_TARGET.to_owned()),
                    rw: Some(true),
                    ..MountPoint::default()
                },
            ]),
            ..ContainerSummary::default()
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
    fn credential_runtime_mounts_add_github_token_dir_without_hashing_token_or_env() {
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
            mount.target == "/run/decune/gh-token"
                && mount
                    .source
                    .as_deref()
                    .is_some_and(|source| source.ends_with("gh-token"))
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
    fn existing_container_decision_reuses_runtime_mount_when_forward_ports_are_added_later() {
        let runtime_dir = tempfile::tempdir().unwrap();
        let mut config = ResolvedConfig::default();
        config.credentials.git.enabled = false;
        config.credentials.github.enabled = false;
        config.credentials.github.mode = GithubCredentialsMode::Off;
        let mut plan = test_up_plan_with_config(config);
        plan.forward_ports = vec![ResolvedForwardPort {
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
            mounts: Some(Vec::new()),
            running: true,
        };

        let decision =
            decide_existing_container(&[container.clone()], "new-hash", &mount_policy(&[]), true)
                .unwrap();

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
        )
        .unwrap();

        assert_eq!(
            attached.forward_ports,
            vec![ResolvedForwardPort {
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
        )
        .unwrap();

        assert!(plan.forward_ports.is_empty());
        assert!(plan.config.ports.entries.is_empty());
        assert!(plan.ignored_detached_forwarding);
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
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
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

                let legacy =
                    create_and_start_container(&client, &legacy_plan, false, false, false).await?;
                let legacy_inspect = client
                    .raw()
                    .inspect_container(&container_name, None)
                    .await?;
                assert!(!container_has_mount_target(
                    &legacy_inspect.mounts,
                    "/run/decune"
                ));

                let recreated = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                assert!(!recreated.reused);
                assert_ne!(legacy.container_id, recreated.container_id);

                let recreated_inspect = client
                    .raw()
                    .inspect_container(&container_name, None)
                    .await?;
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
    fn up_running_attached_runs_post_attach_each_attach() {
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
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: true,
                    no_cache: false,
                    update_features: false,
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
        let config = ResolvedConfig::default();
        let mut input = ConfigHashInput::new(&config);
        input.resolved_mounts = mount_hash_inputs(&[mount]);

        config_hash(&input)
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

    fn test_up_plan_with_config(config: ResolvedConfig) -> UpPlan {
        UpPlan {
            image: "alpine:3.20".to_owned(),
            base_image: "alpine:3.20".to_owned(),
            build_context: None,
            build_options: crate::docker::build::DockerBuildOptions::default(),
            feature_install: None,
            feature_build_context_dir: None,
            resources: DockerResources {
                container_name: "decune-test".to_owned(),
                image_tag: "decune/test:stable-hash".to_owned(),
                labels: BTreeMap::new(),
                config_hash: "stable-hash".to_owned(),
            },
            config_layers: ConfigMergeInput::default(),
            config,
            workspace_folder: "/workspaces/project".to_owned(),
            mounts: Vec::new(),
            forward_ports: Vec::new(),
            ignored_detached_forwarding: false,
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

    fn container_has_mount_target(
        mounts: &Option<Vec<bollard::models::MountPoint>>,
        target: &str,
    ) -> bool {
        mounts.as_ref().is_some_and(|mounts| {
            mounts
                .iter()
                .any(|mount| mount.destination.as_deref() == Some(target))
        })
    }
}
