use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

use crate::{
    config::ConfigLayer,
    docker::{
        client::DockerClient,
        container::{remove_container, stop_container},
        image::{remove_image, workspace_image_tags},
        resource::DockerResources,
        volume::{remove_volume, workspace_volumes},
    },
    host::{
        credentials::cleanup_github_cli_token_file,
        daemon::cleanup_host_daemon_socket,
        forward::{forward_status_dir, remove_forward_status_dir},
    },
    runtime::compose_cli::{
        ComposeDownOptions, ComposeLifecyclePlan, ComposeStopOptions, DockerComposeCli,
    },
    state::{WorkspaceState, load_state_file, remove_state_runtime_dirs},
    ui,
    up::{ForwardingResolution, build_up_plan_with_forwarding_resolution},
    workspace::{
        Workspace, decune_state_root, runtime_dir_for_workspace_id, safe_workspace_slug_for_name,
        state_dir_for_workspace_id,
    },
};

const DEFAULT_STOP_TIMEOUT_SECONDS: i32 = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DownOptions {
    pub(crate) workspace: PathBuf,
    pub(crate) timeout_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoveOptions {
    pub(crate) target: RemoveTarget,
    pub(crate) images: bool,
    pub(crate) no_confirm: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RemoveTarget {
    Workspace(PathBuf),
    AllWorkspaces,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ManagedContainer {
    id: String,
    name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct WorkspaceRemovalPlan {
    workspace_id: String,
    workspace_path: Option<String>,
    state_dir: PathBuf,
    runtime_dir: PathBuf,
    containers: Vec<ManagedContainer>,
    compose_projects: Vec<String>,
    volumes: Vec<String>,
    images: Vec<String>,
    has_state: bool,
    has_runtime: bool,
    has_forward_status: bool,
}

pub(crate) async fn run_down(options: DownOptions) -> Result<()> {
    let timeout_seconds = stop_timeout_seconds(options.timeout_seconds)?;
    let workspace = Workspace::resolve(&options.workspace)?;
    cleanup_github_cli_token_file(workspace.paths().runtime_dir());
    cleanup_host_daemon_socket(workspace.paths().runtime_dir()).await;
    let client = DockerClient::connect_from_env()?;
    let mut compose_project_names = compose_fallback_project_names(&workspace, &client).await?;
    let mut stopped_compose_project = false;
    match compose_lifecycle_plan(&workspace, ComposeLifecycleCommand::Down, &client).await {
        Ok(Some(plan)) => {
            push_unique(
                &mut compose_project_names,
                plan.project.project_name.clone(),
            );
            DockerComposeCli::default()
                .stop(
                    &plan.project,
                    ComposeStopOptions {
                        timeout_seconds: Some(timeout_seconds),
                    },
                    &plan.services,
                )
                .await?;
            ui::done(&format!(
                "Stopped Docker Compose project: {}",
                plan.project.project_name
            ));
            stopped_compose_project = true;
        }
        Ok(None) => {}
        Err(error) => {
            ui::warn(&format!(
                "Falling back to Docker labels because Docker Compose lifecycle planning failed: {error:#}"
            ));
        }
    }

    stopped_compose_project |=
        stop_compose_project_containers(&client, &compose_project_names, timeout_seconds).await?;

    let containers = list_managed_containers(&client, workspace.id()).await?;

    if containers.is_empty() && !stopped_compose_project {
        ui::done("No dev container found for this workspace");
        return Ok(());
    }

    for container in containers {
        stop_container(&client, &container.id, timeout_seconds).await?;
        ui::done(&format!("Stopped dev container: {}", container.name));
    }

    Ok(())
}

pub(crate) async fn run_remove(options: RemoveOptions) -> Result<()> {
    match options.target {
        RemoveTarget::Workspace(workspace) => {
            run_remove_workspace(workspace, options.images, options.no_confirm).await
        }
        RemoveTarget::AllWorkspaces => {
            run_remove_all_workspaces(options.images, options.no_confirm).await
        }
    }
}

async fn run_remove_workspace(workspace: PathBuf, images: bool, no_confirm: bool) -> Result<()> {
    let stdin_is_terminal = io::stdin().is_terminal();
    ensure_remove_confirmed(no_confirm, stdin_is_terminal, true, confirm_remove)?;

    let workspace = Workspace::resolve(&workspace)?;
    cleanup_github_cli_token_file(workspace.paths().runtime_dir());
    cleanup_host_daemon_socket(workspace.paths().runtime_dir()).await;
    let client = DockerClient::connect_from_env()?;
    let mut compose_project_names = compose_fallback_project_names(&workspace, &client).await?;
    let mut remove_generated_images = images;
    let mut compose_projects_removed_by_compose = Vec::new();
    match compose_lifecycle_plan(
        &workspace,
        ComposeLifecycleCommand::Remove { images },
        &client,
    )
    .await
    {
        Ok(Some(plan)) => {
            push_unique(
                &mut compose_project_names,
                plan.project.project_name.clone(),
            );
            DockerComposeCli::default()
                .down(
                    &plan.project,
                    ComposeDownOptions {
                        volumes: plan.cleanup.remove_volumes,
                        remove_orphans: true,
                    },
                )
                .await?;
            ui::done(&format!(
                "Removed Docker Compose project: {}",
                plan.project.project_name
            ));

            remove_generated_images |= plan.cleanup.remove_generated_images;
            push_unique(
                &mut compose_projects_removed_by_compose,
                plan.project.project_name,
            );
        }
        Ok(None) => {}
        Err(error) => {
            ui::warn(&format!(
                "Falling back to Docker labels because Docker Compose lifecycle planning failed: {error:#}"
            ));
        }
    }

    retain_compose_label_cleanup_projects(
        &mut compose_project_names,
        &compose_projects_removed_by_compose,
    );
    remove_compose_project_resources(&client, &compose_project_names).await?;

    let containers = list_managed_containers(&client, workspace.id()).await?;

    for container in containers {
        stop_container(&client, &container.id, DEFAULT_STOP_TIMEOUT_SECONDS).await?;
        remove_container(&client, &container.id, true, true).await?;
        ui::done(&format!("Removed dev container: {}", container.name));
    }

    for volume in workspace_volumes(&client, workspace.id()).await? {
        remove_volume(&client, &volume, true).await?;
        ui::done(&format!("Removed Docker volume: {volume}"));
    }

    if remove_generated_images {
        let image_repository = DockerResources::image_repository_for_workspace(&workspace);
        for image in workspace_image_tags(&client, &image_repository).await? {
            remove_image(&client, &image, true).await?;
            ui::done(&format!("Removed Docker image: {image}"));
        }
    }

    let status_dir = forward_status_dir(workspace.paths().runtime_dir());
    remove_state_runtime_dirs(
        workspace.paths().state_dir(),
        workspace.paths().runtime_dir(),
    )?;
    remove_forward_status_dir(status_dir)?;
    ui::done("Removed dev container resources");
    Ok(())
}

async fn run_remove_all_workspaces(images: bool, no_confirm: bool) -> Result<()> {
    let client = DockerClient::connect_from_env()?;
    let plans = discover_all_workspace_removal_plans(&client, images).await?;
    if plans.is_empty() {
        ui::done("No decune-managed workspace environments found");
        return Ok(());
    }

    print_remove_all_summary(&plans, images);
    let stdin_is_terminal = io::stdin().is_terminal();
    ensure_remove_confirmed(no_confirm, stdin_is_terminal, true, confirm_remove_all)?;

    for plan in plans {
        remove_workspace_plan(&client, plan).await?;
    }
    ui::done("Removed all decune-managed workspace environments");
    Ok(())
}

async fn discover_all_workspace_removal_plans(
    client: &DockerClient,
    include_images: bool,
) -> Result<Vec<WorkspaceRemovalPlan>> {
    let containers = client
        .cli()
        .list_all_managed_container_inspects()
        .await
        .context("Failed to list decune-managed Docker containers")?;
    let volumes = client
        .cli()
        .list_all_managed_volume_inspects()
        .await
        .context("Failed to list decune-managed Docker volumes")?;
    let states = load_all_workspace_states()?;
    let mut entries: BTreeMap<String, WorkspaceRemovalPlan> = BTreeMap::new();

    for state_entry in states {
        let plan = entries
            .entry(state_entry.workspace_id.clone())
            .or_insert_with(|| empty_removal_plan(&state_entry.workspace_id));
        plan.workspace_path
            .get_or_insert_with(|| state_entry.state.workspace.clone());
        if let Some(project_name) = state_entry
            .state
            .compose_project_name
            .as_ref()
            .filter(|project_name| !project_name.trim().is_empty())
            .cloned()
        {
            push_unique(&mut plan.compose_projects, project_name);
        }
        plan.has_state = true;
        if include_images {
            push_state_image_if_decune_generated(plan, &state_entry.state);
        }
    }

    for container in containers {
        let Some((workspace_id, labels)) = managed_workspace_id_from_container(&container) else {
            continue;
        };
        let plan = entries
            .entry(workspace_id.clone())
            .or_insert_with(|| empty_removal_plan(&workspace_id));
        if let Some(workspace_path) = workspace_path_from_labels(labels) {
            plan.workspace_path.get_or_insert(workspace_path);
        }
        if let Some(project_name) = labels
            .get("com.docker.compose.project")
            .filter(|project_name| !project_name.trim().is_empty())
            .cloned()
        {
            push_unique(&mut plan.compose_projects, project_name);
        } else if let (Some(id), Some(name)) = (container.id.clone(), container_name(&container)) {
            plan.containers.push(ManagedContainer { id, name });
        }
    }

    for volume in volumes {
        let Some(labels) = volume.labels.as_ref() else {
            continue;
        };
        let Some(workspace_id) = managed_workspace_id_from_labels(labels) else {
            continue;
        };
        let Some(name) = volume.name.clone().filter(|name| !name.trim().is_empty()) else {
            continue;
        };
        let plan = entries
            .entry(workspace_id.clone())
            .or_insert_with(|| empty_removal_plan(&workspace_id));
        push_unique(&mut plan.volumes, name);
    }

    for plan in entries.values_mut() {
        plan.state_dir = state_dir_for_workspace_id(&plan.workspace_id)?;
        plan.runtime_dir = runtime_dir_for_workspace_id(&plan.workspace_id)?;
        plan.has_state |= plan.state_dir.exists();
        plan.has_runtime = plan.runtime_dir.exists();
        plan.has_forward_status = forward_status_dir(&plan.runtime_dir).exists();
        if include_images {
            append_workspace_images(client, plan).await?;
        }
        plan.containers.sort_by(|a, b| a.name.cmp(&b.name));
        plan.containers.dedup_by(|a, b| a.id == b.id);
        plan.volumes.sort();
        plan.volumes.dedup();
        plan.compose_projects.sort();
        plan.compose_projects.dedup();
        plan.images.sort();
        plan.images.dedup();
    }

    Ok(entries
        .into_values()
        .filter(WorkspaceRemovalPlan::has_targets)
        .collect())
}

async fn remove_workspace_plan(client: &DockerClient, plan: WorkspaceRemovalPlan) -> Result<()> {
    cleanup_github_cli_token_file(&plan.runtime_dir);
    cleanup_host_daemon_socket(&plan.runtime_dir).await;
    remove_compose_project_resources(client, &plan.compose_projects).await?;

    for container in plan.containers {
        stop_container(client, &container.id, DEFAULT_STOP_TIMEOUT_SECONDS).await?;
        remove_container(client, &container.id, true, true).await?;
        ui::done(&format!("Removed dev container: {}", container.name));
    }

    for volume in plan.volumes {
        remove_volume(client, &volume, true).await?;
        ui::done(&format!("Removed Docker volume: {volume}"));
    }

    for image in plan.images {
        remove_image(client, &image, true).await?;
        ui::done(&format!("Removed Docker image: {image}"));
    }

    let status_dir = forward_status_dir(&plan.runtime_dir);
    remove_state_runtime_dirs(&plan.state_dir, &plan.runtime_dir)?;
    remove_forward_status_dir(status_dir)?;
    ui::done(&format!(
        "Removed dev container resources for workspace id: {}",
        plan.workspace_id
    ));
    Ok(())
}

pub(crate) fn remove_requires_confirmation(no_confirm: bool, stdin_is_terminal: bool) -> bool {
    !no_confirm && stdin_is_terminal
}

pub(crate) fn remove_rejects_non_interactive(no_confirm: bool, stdin_is_terminal: bool) -> bool {
    !no_confirm && !stdin_is_terminal
}

fn ensure_remove_confirmed(
    no_confirm: bool,
    stdin_is_terminal: bool,
    has_targets: bool,
    confirm: impl FnOnce() -> Result<bool>,
) -> Result<()> {
    if !has_targets {
        return Ok(());
    }
    if remove_rejects_non_interactive(no_confirm, stdin_is_terminal) {
        bail!(
            "Cannot confirm remove in a non-interactive terminal; rerun with --no-confirm to remove resources"
        );
    }
    if remove_requires_confirmation(no_confirm, stdin_is_terminal) && !confirm()? {
        bail!("Remove cancelled");
    }

    Ok(())
}

fn confirm_remove() -> Result<bool> {
    let mut stderr = io::stderr();
    stderr
        .write_all(b"Remove decune-managed resources for this workspace? [y/N] ")
        .context("Failed to write remove confirmation prompt")?;
    stderr
        .flush()
        .context("Failed to flush remove confirmation prompt")?;

    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("Failed to read remove confirmation response")?;

    Ok(remove_confirmation_response_is_yes(&input))
}

fn confirm_remove_all() -> Result<bool> {
    let mut stderr = io::stderr();
    stderr
        .write_all(b"Remove all decune-managed workspace environments? [y/N] ")
        .context("Failed to write remove confirmation prompt")?;
    stderr
        .flush()
        .context("Failed to flush remove confirmation prompt")?;

    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("Failed to read remove confirmation response")?;

    Ok(remove_confirmation_response_is_yes(&input))
}

fn remove_confirmation_response_is_yes(input: &str) -> bool {
    matches!(input.trim(), "y" | "Y" | "yes" | "YES" | "Yes")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StateRemovalEntry {
    workspace_id: String,
    state: WorkspaceState,
}

impl WorkspaceRemovalPlan {
    fn has_targets(&self) -> bool {
        self.has_state
            || self.has_runtime
            || self.has_forward_status
            || !self.containers.is_empty()
            || !self.compose_projects.is_empty()
            || !self.volumes.is_empty()
            || !self.images.is_empty()
    }
}

fn empty_removal_plan(workspace_id: &str) -> WorkspaceRemovalPlan {
    WorkspaceRemovalPlan {
        workspace_id: workspace_id.to_owned(),
        ..WorkspaceRemovalPlan::default()
    }
}

fn load_all_workspace_states() -> Result<Vec<StateRemovalEntry>> {
    let root = decune_state_root()?;
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to read decune state root: {}", root.display()));
        }
    };
    let mut states = Vec::new();

    for entry in entries {
        let entry = entry.with_context(|| {
            format!("Failed to read decune state root entry: {}", root.display())
        })?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(workspace_id) = path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
        else {
            continue;
        };
        match load_state_file(&path) {
            Ok(Some(state)) => states.push(StateRemovalEntry {
                workspace_id,
                state,
            }),
            Ok(None) => {}
            Err(error) => ui::warn(&format!(
                "Ignoring invalid decune state file for workspace id {workspace_id}: {error:#}"
            )),
        }
    }

    Ok(states)
}

fn managed_workspace_id_from_container(
    container: &crate::docker::container::ContainerInspect,
) -> Option<(String, &BTreeMap<String, String>)> {
    let labels = container.config.as_ref()?.labels.as_ref()?;
    let workspace_id = managed_workspace_id_from_labels(labels)?;
    Some((workspace_id, labels))
}

fn managed_workspace_id_from_labels(labels: &BTreeMap<String, String>) -> Option<String> {
    let managed = labels.get("decune.managed")?;
    if managed != "true" {
        return None;
    }
    labels
        .get("decune.workspace_id")
        .filter(|workspace_id| !workspace_id.trim().is_empty())
        .cloned()
}

fn workspace_path_from_labels(labels: &BTreeMap<String, String>) -> Option<String> {
    labels
        .get("decune.workspace")
        .or_else(|| labels.get("devcontainer.local_folder"))
        .filter(|workspace_path| !workspace_path.trim().is_empty())
        .cloned()
}

fn container_name(container: &crate::docker::container::ContainerInspect) -> Option<String> {
    container
        .name
        .as_ref()
        .map(|name| name.trim_start_matches('/').to_owned())
        .filter(|name| !name.is_empty())
        .or_else(|| container.id.clone())
}

fn push_state_image_if_decune_generated(plan: &mut WorkspaceRemovalPlan, state: &WorkspaceState) {
    let Some(repository) =
        image_repository_for_workspace_path(&state.workspace, &plan.workspace_id)
    else {
        return;
    };
    if state.image.starts_with(&format!("{repository}:")) {
        push_unique(&mut plan.images, state.image.clone());
    }
}

async fn append_workspace_images(
    client: &DockerClient,
    plan: &mut WorkspaceRemovalPlan,
) -> Result<()> {
    let Some(workspace_path) = plan.workspace_path.as_deref() else {
        return Ok(());
    };
    let Some(repository) = image_repository_for_workspace_path(workspace_path, &plan.workspace_id)
    else {
        return Ok(());
    };
    for image in workspace_image_tags(client, &repository).await? {
        push_unique(&mut plan.images, image);
    }
    Ok(())
}

fn image_repository_for_workspace_path(workspace_path: &str, workspace_id: &str) -> Option<String> {
    let basename = Path::new(workspace_path)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())?;
    let safe_slug = safe_workspace_slug_for_name(basename);
    Some(DockerResources::image_repository_for_slug_and_id(
        &safe_slug,
        workspace_id,
    ))
}

fn print_remove_all_summary(plans: &[WorkspaceRemovalPlan], include_images: bool) {
    ui::notice(&format!(
        "Removing {} decune-managed workspace environment(s)",
        plans.len()
    ));
    for plan in plans {
        let workspace = plan.workspace_path.as_deref().unwrap_or("<unknown>");
        ui::info(&format!(
            "Workspace {} ({}) containers={} compose_projects={} volumes={}{}",
            plan.workspace_id,
            workspace,
            plan.containers.len(),
            plan.compose_projects.len(),
            plan.volumes.len(),
            if include_images {
                format!(" images={}", plan.images.len())
            } else {
                String::new()
            }
        ));
    }
}

fn stop_timeout_seconds(timeout_seconds: u64) -> Result<i32> {
    i32::try_from(timeout_seconds).context("Stop timeout is too large")
}

async fn list_managed_containers(
    client: &DockerClient,
    workspace_id: &str,
) -> Result<Vec<ManagedContainer>> {
    let containers = client
        .cli()
        .list_standalone_workspace_containers(workspace_id)
        .await
        .with_context(|| {
            format!("Failed to list Docker containers for workspace: {workspace_id}")
        })?;

    Ok(containers
        .into_iter()
        .map(|container| ManagedContainer {
            id: container.id,
            name: container.name,
        })
        .collect())
}

async fn compose_fallback_project_names(
    workspace: &Workspace,
    client: &DockerClient,
) -> Result<Vec<String>> {
    let mut project_names = BTreeSet::new();
    if let Some(project_name) = load_state_file(workspace.paths().state_dir())
        .ok()
        .flatten()
        .and_then(|state| state.compose_project_name)
        .filter(|project_name| !project_name.trim().is_empty())
    {
        project_names.insert(project_name);
    }

    for project_name in client
        .cli()
        .list_workspace_compose_project_names(workspace.id())
        .await
        .with_context(|| {
            format!(
                "Failed to list Docker Compose projects for workspace: {}",
                workspace.id()
            )
        })?
    {
        project_names.insert(project_name);
    }

    Ok(project_names.into_iter().collect())
}

async fn stop_compose_project_containers(
    client: &DockerClient,
    project_names: &[String],
    timeout_seconds: i32,
) -> Result<bool> {
    let mut found = false;
    for project_name in project_names {
        let containers = client
            .cli()
            .list_containers_for_compose_project(project_name)
            .await
            .with_context(|| {
                format!("Failed to list Docker Compose containers for project: {project_name}")
            })?;
        found |= !containers.is_empty();
        for container in containers.into_iter().filter(|container| container.running) {
            stop_container(client, &container.id, timeout_seconds).await?;
            ui::done(&format!(
                "Stopped Docker Compose container: {}",
                container.name
            ));
        }
    }
    Ok(found)
}

async fn remove_compose_project_resources(
    client: &DockerClient,
    project_names: &[String],
) -> Result<()> {
    for project_name in project_names {
        let containers = client
            .cli()
            .list_containers_for_compose_project(project_name)
            .await
            .with_context(|| {
                format!("Failed to list Docker Compose containers for project: {project_name}")
            })?;
        for container in containers {
            if container.running {
                stop_container(client, &container.id, DEFAULT_STOP_TIMEOUT_SECONDS).await?;
            }
            remove_container(client, &container.id, true, true).await?;
            ui::done(&format!(
                "Removed Docker Compose container: {}",
                container.name
            ));
        }

        for volume in client
            .cli()
            .list_compose_project_volumes(project_name)
            .await
            .with_context(|| {
                format!("Failed to list Docker Compose volumes for project: {project_name}")
            })?
        {
            remove_volume(client, &volume, true).await?;
            ui::done(&format!("Removed Docker volume: {volume}"));
        }

        for network in client
            .cli()
            .list_compose_project_networks(project_name)
            .await
            .with_context(|| {
                format!("Failed to list Docker Compose networks for project: {project_name}")
            })?
        {
            client
                .cli()
                .remove_network(&network)
                .await
                .with_context(|| format!("Failed to remove Docker network: {network}"))?;
            ui::done(&format!("Removed Docker network: {network}"));
        }
    }

    Ok(())
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.iter().any(|existing| existing == &value) {
        values.push(value);
        values.sort();
    }
}

fn retain_compose_label_cleanup_projects(
    project_names: &mut Vec<String>,
    removed_by_compose: &[String],
) {
    project_names.retain(|project_name| {
        !removed_by_compose
            .iter()
            .any(|removed| removed == project_name)
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComposeLifecycleCommand {
    Down,
    Remove { images: bool },
}

async fn compose_lifecycle_plan(
    workspace: &Workspace,
    command: ComposeLifecycleCommand,
    client: &DockerClient,
) -> Result<Option<ComposeLifecyclePlan>> {
    let explicit_config_path = compose_lifecycle_config_path(workspace, client).await?;
    if !has_devcontainer_metadata_hint(workspace) && explicit_config_path.is_none() {
        return Ok(None);
    }

    let plan = build_up_plan_with_forwarding_resolution(
        workspace,
        explicit_config_path.as_deref(),
        ConfigLayer::default(),
        ForwardingResolution::IgnoreDetached,
        false,
        false,
    )?;
    let Some(compose_project) = &plan.compose_project else {
        return Ok(None);
    };
    let Some(crate::config::resolved::ResolvedDevcontainerSource::Compose(_)) =
        &plan.config.devcontainer.source
    else {
        return Ok(None);
    };
    if plan.workspace_folder.is_empty() {
        anyhow::bail!("workspaceFolder must not be empty");
    }

    let command_plan = compose_project.command_plan_without_generated_override();
    let lifecycle = match command {
        ComposeLifecycleCommand::Down => ComposeLifecyclePlan::down(command_plan),
        ComposeLifecycleCommand::Remove { images } => {
            ComposeLifecyclePlan::remove(command_plan, images)
        }
    };

    Ok(Some(lifecycle))
}

async fn compose_lifecycle_config_path(
    workspace: &Workspace,
    client: &DockerClient,
) -> Result<Option<PathBuf>> {
    if let Some(config_file) = load_state_file(workspace.paths().state_dir())
        .ok()
        .flatten()
        .and_then(|state| state.config_file)
        .filter(|config_file| !config_file.trim().is_empty())
    {
        return Ok(Some(config_path_from_label(workspace.root(), &config_file)));
    }

    let containers = client
        .cli()
        .list_workspace_containers(workspace.id())
        .await
        .with_context(|| {
            format!(
                "Failed to list Docker containers for workspace: {}",
                workspace.id()
            )
        })?;
    Ok(containers
        .into_iter()
        .filter_map(|container| container.config_file)
        .find(|config_file| !config_file.trim().is_empty())
        .map(|config_file| config_path_from_label(workspace.root(), &config_file)))
}

fn config_path_from_label(workspace_root: &Path, config_file: &str) -> PathBuf {
    let path = PathBuf::from(config_file);
    if path.is_absolute() {
        path
    } else {
        workspace_root.join(path)
    }
}

fn has_devcontainer_metadata_hint(workspace: &Workspace) -> bool {
    let root = workspace.root();
    root.join(".devcontainer/devcontainer.json").is_file()
        || root.join(".devcontainer.json").is_file()
        || root
            .join(".devcontainer")
            .read_dir()
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .any(|entry| entry.path().join("devcontainer.json").is_file())
}

#[cfg(test)]
mod tests {
    use super::stop_timeout_seconds;
    use crate::{
        docker::client::DockerClient,
        state::{LifecycleState, StateContainerSnapshot, sync_state_with_container},
        workspace::Workspace,
    };
    use anyhow::Result;
    use std::{fs, path::Path};

    #[test]
    fn remove_confirmation_is_required_only_for_interactive_runs_without_no_confirm() {
        assert!(super::remove_requires_confirmation(false, true));
        assert!(!super::remove_requires_confirmation(true, true));
        assert!(!super::remove_requires_confirmation(false, false));
    }

    #[test]
    fn remove_non_interactive_without_no_confirm_is_rejected_before_cleanup() {
        assert!(super::remove_rejects_non_interactive(false, false));
        assert!(!super::remove_rejects_non_interactive(true, false));
        assert!(!super::remove_rejects_non_interactive(false, true));
    }

    #[test]
    fn remove_prompt_accepts_only_explicit_yes() {
        assert!(super::remove_confirmation_response_is_yes("y\n"));
        assert!(super::remove_confirmation_response_is_yes("yes\n"));
        assert!(super::remove_confirmation_response_is_yes("YES\n"));
        assert!(!super::remove_confirmation_response_is_yes("\n"));
        assert!(!super::remove_confirmation_response_is_yes("no\n"));
    }

    #[test]
    fn remove_confirmation_gate_handles_interactive_accept_and_reject() {
        assert!(super::ensure_remove_confirmed(false, true, true, || Ok(true)).is_ok());

        let error = super::ensure_remove_confirmed(false, true, true, || Ok(false)).unwrap_err();
        assert!(error.to_string().contains("Remove cancelled"));
    }

    #[test]
    fn remove_confirmation_gate_rejects_non_interactive_and_skips_prompt_for_no_confirm() {
        let error = super::ensure_remove_confirmed(false, false, true, || Ok(true)).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Cannot confirm remove in a non-interactive terminal")
        );

        let mut prompted = false;
        assert!(
            super::ensure_remove_confirmed(true, false, true, || -> Result<bool> {
                prompted = true;
                Ok(false)
            })
            .is_ok()
        );
        assert!(!prompted);
    }

    #[test]
    fn remove_confirmation_gate_skips_empty_all_workspace_target_without_prompt() {
        let mut prompted = false;

        assert!(
            super::ensure_remove_confirmed(false, false, false, || -> Result<bool> {
                prompted = true;
                Ok(false)
            })
            .is_ok()
        );
        assert!(!prompted);
    }

    #[test]
    fn stop_timeout_rejects_values_that_docker_api_cannot_represent() {
        assert_eq!(stop_timeout_seconds(10).unwrap(), 10);
        assert!(stop_timeout_seconds(i32::MAX as u64 + 1).is_err());
    }

    #[test]
    fn compose_label_cleanup_excludes_projects_removed_by_compose_down() {
        let mut project_names = vec![
            "decune-current".to_owned(),
            "decune-stale".to_owned(),
            "decune-other".to_owned(),
        ];
        super::retain_compose_label_cleanup_projects(
            &mut project_names,
            &["decune-current".to_owned(), "decune-other".to_owned()],
        );

        assert_eq!(project_names, vec!["decune-stale".to_owned()]);
    }

    #[test]
    fn compose_lifecycle_uses_state_config_path_without_standard_hint() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_root = temp.path().join("workspace");
        let config_dir = workspace_root.join("custom-devcontainer");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            config_dir.join("devcontainer.json"),
            r#"
            {
              "dockerComposeFile": "compose.yaml",
              "service": "app"
            }
            "#,
        )
        .unwrap();
        fs::write(
            config_dir.join("compose.yaml"),
            "services:\n  app:\n    image: alpine:3.20\n",
        )
        .unwrap();
        let workspace = Workspace::resolve(&workspace_root).unwrap();
        sync_state_with_container(
            workspace.paths().state_dir(),
            workspace.root(),
            StateContainerSnapshot {
                container_id: "container-a".to_owned(),
                image: "decune/project:hash-a".to_owned(),
                config_hash: "hash-a".to_owned(),
                config_file: Some(config_dir.join("devcontainer.json").display().to_string()),
            },
            LifecycleState::default(),
        )
        .unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let plan = runtime
            .block_on(async {
                let client = DockerClient::connect_from_env().unwrap();
                super::compose_lifecycle_plan(
                    &workspace,
                    super::ComposeLifecycleCommand::Down,
                    &client,
                )
                .await
            })
            .unwrap()
            .unwrap();

        assert!(plan.services.is_empty());
        assert_eq!(plan.project.project_directory, config_dir);
        assert!(plan.project.files.iter().any(|file| {
            file.file_name()
                .is_some_and(|name| name == Path::new("compose.yaml"))
        }));
    }

    #[test]
    fn compose_lifecycle_prefers_state_config_path_with_standard_hint() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_root = temp.path().join("workspace");
        let standard_dir = workspace_root.join(".devcontainer");
        let custom_dir = workspace_root.join("custom-devcontainer");
        fs::create_dir_all(&standard_dir).unwrap();
        fs::create_dir_all(&custom_dir).unwrap();
        fs::write(
            standard_dir.join("devcontainer.json"),
            r#"
            {
              "dockerComposeFile": "compose.yaml",
              "service": "default"
            }
            "#,
        )
        .unwrap();
        fs::write(
            standard_dir.join("compose.yaml"),
            "services:\n  default:\n    image: alpine:3.20\n",
        )
        .unwrap();
        fs::write(
            custom_dir.join("devcontainer.json"),
            r#"
            {
              "dockerComposeFile": "compose.yaml",
              "service": "app"
            }
            "#,
        )
        .unwrap();
        fs::write(
            custom_dir.join("compose.yaml"),
            "services:\n  app:\n    image: alpine:3.20\n",
        )
        .unwrap();
        let workspace = Workspace::resolve(&workspace_root).unwrap();
        sync_state_with_container(
            workspace.paths().state_dir(),
            workspace.root(),
            StateContainerSnapshot {
                container_id: "container-a".to_owned(),
                image: "decune/project:hash-a".to_owned(),
                config_hash: "hash-a".to_owned(),
                config_file: Some(custom_dir.join("devcontainer.json").display().to_string()),
            },
            LifecycleState::default(),
        )
        .unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let plan = runtime
            .block_on(async {
                let client = DockerClient::connect_from_env().unwrap();
                super::compose_lifecycle_plan(
                    &workspace,
                    super::ComposeLifecycleCommand::Down,
                    &client,
                )
                .await
            })
            .unwrap()
            .unwrap();

        assert_eq!(plan.project.project_directory, custom_dir);
        assert!(plan.project.files.iter().any(|file| {
            file.parent().is_some_and(|parent| parent == custom_dir)
                && file
                    .file_name()
                    .is_some_and(|name| name == Path::new("compose.yaml"))
        }));
    }
}
