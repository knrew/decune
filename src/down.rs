use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

mod volumes;

use crate::{
    config::ConfigLayer,
    docker::{
        client::DockerClient,
        container::{ContainerInspect, remove_container, stop_container},
        image::{remove_image, workspace_image_tags},
        resource::{
            DockerResources, compose_project_name_from_labels, managed_workspace_id_from_container,
            managed_workspace_id_from_labels, workspace_path_from_labels,
        },
        volume::workspace_volumes,
    },
    host::{
        credentials::cleanup_github_cli_token_file,
        daemon::cleanup_host_daemon_socket,
        forward::{forward_status_dir, remove_forward_status_dir},
    },
    runtime::{
        compose_cli::{
            ComposeDownOptions, ComposeLifecyclePlan, ComposeStopOptions, DockerComposeCli,
        },
        docker_cli::VolumeRemoval,
    },
    state::{WorkspaceState, load_state_file, remove_runtime_dir, remove_state_runtime_dirs},
    ui,
    up::{
        ForwardingResolution, UpPlan, build_read_only_up_plan_with_forwarding_resolution,
        build_up_plan_with_forwarding_resolution,
    },
    workspace::{
        Workspace, decune_state_root, is_valid_workspace_id, runtime_dir_for_workspace_id,
        safe_workspace_slug_for_name, state_dir_for_workspace_id,
    },
};

use self::volumes::{KeptVolume, VolumeRemovalReport, mounted_volume_names, print_kept_volumes};

const DEFAULT_STOP_TIMEOUT_SECONDS: i32 = 10;
const NON_INTERACTIVE_REMOVE_ERROR: &str = "Cannot confirm remove in a non-interactive terminal; rerun with --no-confirm to remove resources";

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
    mounted_volumes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct WorkspaceRemovalPlan {
    workspace_id: String,
    workspace_path: Option<String>,
    state_dir: PathBuf,
    runtime_dir: PathBuf,
    containers: Vec<ManagedContainer>,
    compose_projects: Vec<String>,
    /// `docker compose down` が消したプロジェクト。コンテナと network は、ラベルから消し直さない。
    /// volume は、`docker compose down` の後に残ったものもラベルから消す。
    compose_projects_removed_by_compose: Vec<String>,
    /// decune のラベルを持つ volume。
    volumes: Vec<String>,
    /// 確認の前に示す、Compose プロジェクトの volume。削除するときは、ラベルで探し直す。
    compose_volumes: Vec<String>,
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
    let client = DockerClient::connect_from_env();
    let mut compose_project_names = compose_fallback_project_names(&workspace, &client).await?;
    let mut stopped_compose_project = false;
    match compose_lifecycle_plan(
        &workspace,
        ComposeLifecycleCommand::Down,
        &client,
        build_up_plan_with_forwarding_resolution,
    )
    .await
    {
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
    if remove_rejects_non_interactive(no_confirm, stdin_is_terminal) {
        bail!(NON_INTERACTIVE_REMOVE_ERROR);
    }

    let workspace = Workspace::resolve(&workspace)?;
    let client = DockerClient::connect_from_env();
    let confirm = remove_requires_confirmation(no_confirm, stdin_is_terminal)
        .then_some(confirm_remove as fn(&str) -> Result<bool>);
    let kept = remove_workspace(&client, &workspace, images, confirm).await?;
    print_kept_volumes(&kept, "this workspace");
    Ok(())
}

/// 一つのワークスペースを削除する。`confirm` があれば、削除する decune-managed ボリュームの
/// 一覧を含む確認の文を渡して、Docker と状態に手を加える前に確かめる。
async fn remove_workspace(
    client: &DockerClient,
    workspace: &Workspace,
    images: bool,
    confirm: Option<impl FnOnce(&str) -> Result<bool>>,
) -> Result<Vec<KeptVolume>> {
    if let Some(confirm) = confirm {
        let volumes = workspace_volume_candidates(client, workspace).await?;
        if !confirm(&workspace_remove_prompt(&volumes))? {
            bail!("Remove cancelled");
        }
    }

    cleanup_workspace_runtime_secrets(workspace.paths().runtime_dir()).await;
    let mut report = VolumeRemovalReport::default();
    let mut plan = WorkspaceRemovalPlan {
        state_dir: workspace.paths().state_dir().to_path_buf(),
        runtime_dir: workspace.paths().runtime_dir().to_path_buf(),
        ..empty_removal_plan(workspace.id())
    };
    let mut compose_project_names = compose_fallback_project_names(workspace, client).await?;
    let mut remove_generated_images = images;
    match compose_lifecycle_plan(
        workspace,
        ComposeLifecycleCommand::Remove { images },
        client,
        build_up_plan_with_forwarding_resolution,
    )
    .await
    {
        Ok(Some(lifecycle)) => {
            let project_name = lifecycle.project.project_name.clone();
            push_unique(&mut compose_project_names, project_name.clone());
            // `docker compose down` はプロジェクトのコンテナを消すので、mount はその前に読む。
            record_compose_project_mounts(client, &project_name, &mut report).await?;
            let compose_remove_result = DockerComposeCli::default()
                .down(
                    &lifecycle.project,
                    ComposeDownOptions {
                        volumes: lifecycle.cleanup.compose.remove_volumes,
                        remove_orphans: true,
                    },
                )
                .await;
            match compose_remove_result {
                Ok(()) => {
                    ui::done(&format!("Removed Docker Compose project: {project_name}"));
                    push_unique(&mut plan.compose_projects_removed_by_compose, project_name);
                }
                Err(error) => {
                    ui::warn(&format!(
                        "Falling back to Docker labels because Docker Compose project removal failed: {error:#}"
                    ));
                }
            }

            remove_generated_images |= lifecycle.cleanup.workspace.remove_generated_images;
        }
        Ok(None) => {}
        Err(error) => {
            ui::warn(&format!(
                "Falling back to Docker labels because Docker Compose lifecycle planning failed: {error:#}"
            ));
        }
    }

    plan.compose_projects = compose_project_names;
    plan.containers = list_managed_containers(client, workspace.id()).await?;
    plan.volumes = workspace_volumes(client, workspace.id()).await?;
    if remove_generated_images {
        let image_repository = DockerResources::image_repository_for_workspace(workspace);
        plan.images = workspace_image_tags(client, &image_repository).await?;
    }

    remove_workspace_plans(client, std::slice::from_ref(&plan), &mut report).await?;
    ui::done("Removed dev container resources");
    report.kept_volumes(client).await
}

async fn run_remove_all_workspaces(images: bool, no_confirm: bool) -> Result<()> {
    let client = DockerClient::connect_from_env();
    let plans = discover_all_workspace_removal_plans(&client, images).await?;
    if plans.is_empty() {
        ui::done("No decune-managed workspace environments found");
        return Ok(());
    }

    print_remove_all_summary(&plans, images);
    let stdin_is_terminal = io::stdin().is_terminal();
    ensure_remove_confirmed(
        RemoveConfirmation {
            no_confirm,
            stdin_is_terminal,
            has_targets: true,
        },
        confirm_remove_all,
    )?;

    for plan in &plans {
        cleanup_workspace_runtime_secrets(&plan.runtime_dir).await;
    }
    let mut report = VolumeRemovalReport::default();
    for removed in remove_workspace_plans(&client, &plans, &mut report).await? {
        ui::done(&format!(
            "Removed dev container resources for workspace id: {removed}"
        ));
    }
    ui::done("Removed all decune-managed workspace environments");
    print_kept_volumes(
        &report.kept_volumes(&client).await?,
        "the removed workspaces",
    );
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
        if let Some(project_name) = compose_project_name_from_labels(labels) {
            push_unique(&mut plan.compose_projects, project_name);
        } else if let (Some(id), Some(name)) = (container.id.clone(), container_name(&container)) {
            plan.containers.push(ManagedContainer {
                id,
                name,
                mounted_volumes: mounted_volume_names(&container),
            });
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
        plan.runtime_dir = runtime_dir_for_workspace_id(&plan.workspace_id);
        plan.has_state |= plan.state_dir.exists();
        plan.has_runtime = plan.runtime_dir.exists();
        plan.has_forward_status = forward_status_dir(&plan.runtime_dir).exists();
        if include_images {
            append_workspace_images(client, plan).await?;
        }
        for project_name in &plan.compose_projects {
            plan.compose_volumes
                .extend(list_compose_project_volumes(client, project_name).await?);
        }
        plan.containers.sort_by(|a, b| a.name.cmp(&b.name));
        plan.containers.dedup_by(|a, b| a.id == b.id);
        plan.volumes.sort();
        plan.volumes.dedup();
        plan.compose_volumes.sort();
        plan.compose_volumes.dedup();
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

/// 削除の計画を実行し、削除したワークスペースの workspace id を返す。
///
/// すべての計画のコンテナを消してから volume を消す。ワークスペース X の decune-managed
/// ボリュームを、同じ実行で消すワークスペース Y のコンテナだけが mount しているとき、
/// ワークスペースの順によらず、その volume を使用中として残さずに消すためである。
async fn remove_workspace_plans(
    client: &DockerClient,
    plans: &[WorkspaceRemovalPlan],
    report: &mut VolumeRemovalReport,
) -> Result<Vec<String>> {
    for plan in plans {
        remove_workspace_containers(client, plan, report).await?;
    }
    let mut removed = Vec::new();
    for plan in plans {
        remove_workspace_volumes_and_data(client, plan, report).await?;
        removed.push(plan.workspace_id.clone());
    }
    Ok(removed)
}

async fn remove_workspace_containers(
    client: &DockerClient,
    plan: &WorkspaceRemovalPlan,
    report: &mut VolumeRemovalReport,
) -> Result<()> {
    for project_name in plan.label_cleanup_compose_projects() {
        remove_compose_project_containers(client, project_name, report).await?;
    }

    for container in &plan.containers {
        report.record_mounted_volumes(&container.mounted_volumes);
        stop_container(client, &container.id, DEFAULT_STOP_TIMEOUT_SECONDS).await?;
        remove_container(client, &container.id, true, true).await?;
        ui::done(&format!("Removed dev container: {}", container.name));
    }
    Ok(())
}

async fn remove_workspace_volumes_and_data(
    client: &DockerClient,
    plan: &WorkspaceRemovalPlan,
    report: &mut VolumeRemovalReport,
) -> Result<()> {
    // `docker compose down --volumes` が成功した後も、プロジェクトのラベルを持つ volume が
    // 残っていれば消す。今の Compose ファイルが宣言していない volume も含め、消える volume を
    // Compose で消す経路とラベルから消す経路とで揃えるためである。
    let mut compose_volume_in_use = false;
    for project_name in &plan.compose_projects {
        for volume in list_compose_project_volumes(client, project_name).await? {
            compose_volume_in_use |=
                report.remove_managed_volume(client, &volume).await? == VolumeRemoval::InUse;
        }
    }
    for project_name in plan.label_cleanup_compose_projects() {
        remove_compose_project_networks(client, project_name).await?;
    }

    for volume in &plan.volumes {
        report.remove_managed_volume(client, volume).await?;
    }

    for image in &plan.images {
        remove_image(client, image, true).await?;
        ui::done(&format!("Removed Docker image: {image}"));
    }

    let status_dir = forward_status_dir(&plan.runtime_dir);
    if compose_volume_in_use {
        // Compose プロジェクトの volume を辿る手掛かりは、状態とコンテナのラベルにしかない。
        // コンテナを消した後に状態も消すと、残した volume を decune から辿れなくなる。
        remove_runtime_dir(&plan.runtime_dir)?;
        ui::warn(&format!(
            "Kept decune state for workspace id {} because its Docker Compose volumes are in use; run decune remove again after they are released",
            plan.workspace_id
        ));
    } else {
        remove_state_runtime_dirs(&plan.state_dir, &plan.runtime_dir)?;
    }
    remove_forward_status_dir(status_dir)?;
    Ok(())
}

async fn cleanup_workspace_runtime_secrets(runtime_dir: &Path) {
    cleanup_github_cli_token_file(runtime_dir);
    cleanup_host_daemon_socket(runtime_dir).await;
}

/// 確認の前に示す、削除する decune-managed ボリューム。Docker と状態を読むだけで、
/// 変えない。Compose プロジェクト名は、削除と同じく、状態、コンテナのラベル、
/// 今の設定から得る。今の設定は、ホストのパスを作らない読み取り専用の解決で読む。
async fn workspace_volume_candidates(
    client: &DockerClient,
    workspace: &Workspace,
) -> Result<Vec<String>> {
    let mut project_names = compose_fallback_project_names(workspace, client).await?;
    if let Ok(Some(lifecycle)) = compose_lifecycle_plan(
        workspace,
        ComposeLifecycleCommand::Remove { images: false },
        client,
        build_read_only_up_plan_with_forwarding_resolution,
    )
    .await
    {
        push_unique(&mut project_names, lifecycle.project.project_name);
    }

    let mut volumes = BTreeSet::new();
    volumes.extend(workspace_volumes(client, workspace.id()).await?);
    for project_name in &project_names {
        volumes.extend(list_compose_project_volumes(client, project_name).await?);
    }
    Ok(volumes.into_iter().collect())
}

fn workspace_remove_prompt(volumes: &[String]) -> String {
    let mut prompt = String::new();
    if !volumes.is_empty() {
        prompt.push_str("Docker volumes to remove:\n");
        for volume in volumes {
            prompt.push_str("  ");
            prompt.push_str(volume);
            prompt.push('\n');
        }
    }
    prompt.push_str("Remove decune-managed resources for this workspace? [y/N] ");
    prompt
}

pub(crate) const fn remove_requires_confirmation(
    no_confirm: bool,
    stdin_is_terminal: bool,
) -> bool {
    !no_confirm && stdin_is_terminal
}

pub(crate) const fn remove_rejects_non_interactive(
    no_confirm: bool,
    stdin_is_terminal: bool,
) -> bool {
    !no_confirm && !stdin_is_terminal
}

#[derive(Debug, Clone, Copy)]
struct RemoveConfirmation {
    no_confirm: bool,
    stdin_is_terminal: bool,
    has_targets: bool,
}

fn ensure_remove_confirmed(
    confirmation: RemoveConfirmation,
    confirm: impl FnOnce() -> Result<bool>,
) -> Result<()> {
    if !confirmation.has_targets {
        return Ok(());
    }
    if remove_rejects_non_interactive(confirmation.no_confirm, confirmation.stdin_is_terminal) {
        bail!(NON_INTERACTIVE_REMOVE_ERROR);
    }
    if remove_requires_confirmation(confirmation.no_confirm, confirmation.stdin_is_terminal)
        && !confirm()?
    {
        bail!("Remove cancelled");
    }

    Ok(())
}

fn confirm_remove(prompt: &str) -> Result<bool> {
    let mut stderr = io::stderr();
    stderr
        .write_all(prompt.as_bytes())
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
    const fn has_targets(&self) -> bool {
        self.has_state
            || self.has_runtime
            || self.has_forward_status
            || !self.containers.is_empty()
            || !self.compose_projects.is_empty()
            || !self.volumes.is_empty()
            || !self.images.is_empty()
    }
}

impl WorkspaceRemovalPlan {
    fn label_cleanup_compose_projects(&self) -> impl Iterator<Item = &String> {
        self.compose_projects.iter().filter(|project_name| {
            !self
                .compose_projects_removed_by_compose
                .contains(project_name)
        })
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
        if !is_valid_workspace_id(&workspace_id) {
            if path.join("state.toml").is_file() {
                ui::warn(&format!(
                    "Ignoring decune state directory with invalid workspace id: {workspace_id}"
                ));
            }
            continue;
        }
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
    for line in remove_all_summary_lines(plans, include_images) {
        ui::info(&line);
    }
}

fn remove_all_summary_lines(plans: &[WorkspaceRemovalPlan], include_images: bool) -> Vec<String> {
    let mut lines = Vec::new();
    for plan in plans {
        let workspace = plan.workspace_path.as_deref().unwrap_or("<unknown>");
        let volumes = plan
            .volumes
            .iter()
            .chain(&plan.compose_volumes)
            .collect::<BTreeSet<_>>();
        lines.push(format!(
            "Workspace {} ({}) containers={} compose_projects={} volumes={}{}",
            plan.workspace_id,
            workspace,
            plan.containers.len(),
            plan.compose_projects.len(),
            volumes.len(),
            if include_images {
                format!(" images={}", plan.images.len())
            } else {
                String::new()
            }
        ));
        for volume in volumes {
            lines.push(format!("Volume {volume} will be removed"));
        }
    }
    lines
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
        .list_standalone_workspace_container_inspects(workspace_id)
        .await
        .with_context(|| {
            format!("Failed to list Docker containers for workspace: {workspace_id}")
        })?;

    Ok(containers
        .iter()
        .filter_map(|container| {
            Some(ManagedContainer {
                id: container.id.clone()?,
                name: container_name(container)?,
                mounted_volumes: mounted_volume_names(container),
            })
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

async fn record_compose_project_mounts(
    client: &DockerClient,
    project_name: &str,
    report: &mut VolumeRemovalReport,
) -> Result<()> {
    for container in list_compose_project_container_inspects(client, project_name).await? {
        report.record_mounted_volumes(&mounted_volume_names(&container));
    }
    Ok(())
}

async fn remove_compose_project_containers(
    client: &DockerClient,
    project_name: &str,
    report: &mut VolumeRemovalReport,
) -> Result<()> {
    for container in list_compose_project_container_inspects(client, project_name).await? {
        report.record_mounted_volumes(&mounted_volume_names(&container));
        let (Some(id), Some(name)) = (container.id.as_deref(), container_name(&container)) else {
            continue;
        };
        let running = container
            .state
            .as_ref()
            .and_then(|state| state.running)
            .unwrap_or(false);
        if running {
            stop_container(client, id, DEFAULT_STOP_TIMEOUT_SECONDS).await?;
        }
        remove_container(client, id, true, true).await?;
        ui::done(&format!("Removed Docker Compose container: {name}"));
    }
    Ok(())
}

async fn list_compose_project_container_inspects(
    client: &DockerClient,
    project_name: &str,
) -> Result<Vec<ContainerInspect>> {
    client
        .cli()
        .list_compose_project_container_inspects_by_project(project_name)
        .await
        .with_context(|| {
            format!("Failed to list Docker Compose containers for project: {project_name}")
        })
}

async fn list_compose_project_volumes(
    client: &DockerClient,
    project_name: &str,
) -> Result<Vec<String>> {
    client
        .cli()
        .list_compose_project_volumes(project_name)
        .await
        .with_context(|| {
            format!("Failed to list Docker Compose volumes for project: {project_name}")
        })
}

async fn remove_compose_project_networks(client: &DockerClient, project_name: &str) -> Result<()> {
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
    Ok(())
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.iter().any(|existing| existing == &value) {
        values.push(value);
        values.sort();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComposeLifecycleCommand {
    Down,
    Remove { images: bool },
}

type UpPlanBuilder =
    fn(&Workspace, Option<&Path>, ConfigLayer, ForwardingResolution, bool, bool) -> Result<UpPlan>;

async fn compose_lifecycle_plan(
    workspace: &Workspace,
    command: ComposeLifecycleCommand,
    client: &DockerClient,
    build_up_plan: UpPlanBuilder,
) -> Result<Option<ComposeLifecyclePlan>> {
    let explicit_config_path = compose_lifecycle_config_path(workspace, client).await?;
    if !has_devcontainer_metadata_hint(workspace) && explicit_config_path.is_none() {
        return Ok(None);
    }

    let plan = build_up_plan(
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

    let generated_override_path = compose_project.generated_override_path();
    let use_generated_override = matches!(command, ComposeLifecycleCommand::Remove { .. })
        && generated_override_path.try_exists().with_context(|| {
            format!(
                "Failed to inspect Docker Compose generated override: {}",
                generated_override_path.display()
            )
        })?;
    let command_plan = if use_generated_override {
        compose_project.command_plan_with_generated_override()
    } else {
        compose_project.command_plan_without_generated_override()
    };
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
        state::{
            LifecycleState, StateContainerSnapshot, WorkspaceModeSnapshot,
            sync_state_with_container,
        },
        workspace::Workspace,
    };
    use anyhow::Result;
    use std::{collections::BTreeMap, fs, path::Path};

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
        assert!(
            super::ensure_remove_confirmed(
                super::RemoveConfirmation {
                    no_confirm: false,
                    stdin_is_terminal: true,
                    has_targets: true,
                },
                || Ok(true),
            )
            .is_ok()
        );

        let error = super::ensure_remove_confirmed(
            super::RemoveConfirmation {
                no_confirm: false,
                stdin_is_terminal: true,
                has_targets: true,
            },
            || Ok(false),
        )
        .unwrap_err();
        assert!(error.to_string().contains("Remove cancelled"));
    }

    #[test]
    fn remove_confirmation_gate_rejects_non_interactive_and_skips_prompt_for_no_confirm() {
        let error = super::ensure_remove_confirmed(
            super::RemoveConfirmation {
                no_confirm: false,
                stdin_is_terminal: false,
                has_targets: true,
            },
            || Ok(true),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Cannot confirm remove in a non-interactive terminal")
        );

        let mut prompted = false;
        assert!(
            super::ensure_remove_confirmed(
                super::RemoveConfirmation {
                    no_confirm: true,
                    stdin_is_terminal: false,
                    has_targets: true,
                },
                || -> Result<bool> {
                    prompted = true;
                    Ok(false)
                },
            )
            .is_ok()
        );
        assert!(!prompted);
    }

    #[test]
    fn remove_confirmation_gate_skips_empty_all_workspace_target_without_prompt() {
        let mut prompted = false;

        assert!(
            super::ensure_remove_confirmed(
                super::RemoveConfirmation {
                    no_confirm: false,
                    stdin_is_terminal: false,
                    has_targets: false,
                },
                || -> Result<bool> {
                    prompted = true;
                    Ok(false)
                },
            )
            .is_ok()
        );
        assert!(!prompted);
    }

    #[test]
    fn managed_workspace_id_from_labels_rejects_invalid_workspace_ids() {
        let labels_with_workspace_id = |workspace_id: &str| {
            BTreeMap::from([
                ("decune.managed".to_owned(), "true".to_owned()),
                ("decune.workspace_id".to_owned(), workspace_id.to_owned()),
            ])
        };

        assert_eq!(
            super::managed_workspace_id_from_labels(&labels_with_workspace_id("123456abcdef")),
            Some("123456abcdef".to_owned())
        );

        for workspace_id in [
            "",
            "123456abcde",
            "123456abcdef0",
            "123456ABCDE",
            "123456abcdeg",
            " 123456abcde",
            "123456abcde ",
            "../victim",
            r"..\victim",
            "123456/abcde",
        ] {
            assert_eq!(
                super::managed_workspace_id_from_labels(&labels_with_workspace_id(workspace_id)),
                None,
                "{workspace_id:?}"
            );
        }

        assert_eq!(
            super::managed_workspace_id_from_labels(&BTreeMap::from([
                ("decune.managed".to_owned(), "false".to_owned()),
                ("decune.workspace_id".to_owned(), "123456abcdef".to_owned()),
            ])),
            None
        );
    }

    #[test]
    fn stop_timeout_rejects_values_that_docker_api_cannot_represent() {
        assert_eq!(stop_timeout_seconds(10).unwrap(), 10);
        assert!(stop_timeout_seconds(i32::MAX as u64 + 1).is_err());
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
                mode: WorkspaceModeSnapshot::Compose,
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
                let client = DockerClient::connect_from_env();
                super::compose_lifecycle_plan(
                    &workspace,
                    super::ComposeLifecycleCommand::Down,
                    &client,
                    crate::up::build_up_plan_with_forwarding_resolution,
                )
                .await
            })
            .unwrap()
            .unwrap();

        assert_eq!(plan.services, Vec::<String>::new());
        assert_eq!(plan.project.project_directory, config_dir);
        assert!(plan.project.files.iter().any(|file| {
            file.file_name()
                .is_some_and(|name| name == Path::new("compose.yaml"))
        }));
    }

    #[test]
    fn compose_remove_lifecycle_includes_existing_generated_override() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_root = temp.path().join("workspace");
        let config_dir = workspace_root.join(".devcontainer");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            config_dir.join("devcontainer.json"),
            r#"{"dockerComposeFile":"compose.yaml","service":"app"}"#,
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
                image: "alpine:3.20".to_owned(),
                config_hash: "hash-a".to_owned(),
                config_file: Some(config_dir.join("devcontainer.json").display().to_string()),
                mode: WorkspaceModeSnapshot::Image,
            },
            LifecycleState::default(),
        )
        .unwrap();
        let generated_override = workspace.paths().state_dir().join("compose.override.yaml");
        fs::write(&generated_override, "services: {}\n").unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let plan = runtime
            .block_on(async {
                super::compose_lifecycle_plan(
                    &workspace,
                    super::ComposeLifecycleCommand::Remove { images: false },
                    &DockerClient::connect_from_env(),
                    crate::up::build_up_plan_with_forwarding_resolution,
                )
                .await
            })
            .unwrap()
            .unwrap();

        assert!(plan.project.files.contains(&generated_override));
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
                mode: WorkspaceModeSnapshot::Dockerfile,
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
                let client = DockerClient::connect_from_env();
                super::compose_lifecycle_plan(
                    &workspace,
                    super::ComposeLifecycleCommand::Down,
                    &client,
                    crate::up::build_up_plan_with_forwarding_resolution,
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

#[cfg(test)]
mod removal_tests {
    use std::{fs, path::Path, sync::Arc};

    use super::{
        ManagedContainer, WorkspaceRemovalPlan, empty_removal_plan, remove_all_summary_lines,
        remove_workspace, remove_workspace_plans, volumes::KeptVolume, volumes::KeptVolumeReason,
        volumes::VolumeRemovalReport, workspace_remove_prompt,
    };
    use crate::{
        docker::client::DockerClient,
        runtime::{docker_cli::DockerCli, fake_docker::FakeDocker},
        workspace::Workspace,
    };

    const WORKSPACE_X: &str = "aaaaaaaaaaaa";
    const WORKSPACE_Y: &str = "bbbbbbbbbbbb";
    const COMPOSE_PROJECT: (&str, &str) = ("com.docker.compose.project", "decune-app-aaaaaaaaaaaa");
    const ANONYMOUS: (&str, &str) = ("com.docker.volume.anonymous", "");

    fn client(docker: &FakeDocker) -> DockerClient {
        DockerClient::from_cli(DockerCli::new(Arc::new(docker.clone())))
    }

    fn managed_labels(workspace_id: &str) -> [(&'static str, &str); 2] {
        [
            ("decune.managed", "true"),
            ("decune.workspace_id", workspace_id),
        ]
    }

    /// 状態とランタイムディレクトリを `root` の下に持つ計画。
    fn plan(root: &Path, workspace_id: &str) -> WorkspaceRemovalPlan {
        let state_dir = root.join("state").join(workspace_id);
        let runtime_dir = root.join("runtime").join(workspace_id);
        fs::create_dir_all(&state_dir).unwrap();
        fs::create_dir_all(&runtime_dir).unwrap();
        fs::write(state_dir.join("state.toml"), "version = 1\n").unwrap();
        WorkspaceRemovalPlan {
            state_dir,
            runtime_dir,
            has_state: true,
            ..empty_removal_plan(workspace_id)
        }
    }

    fn standalone(id: &str, mounted_volumes: &[&str]) -> ManagedContainer {
        ManagedContainer {
            id: id.to_owned(),
            name: id.to_owned(),
            mounted_volumes: mounted_volumes.iter().map(|v| (*v).to_owned()).collect(),
        }
    }

    fn run_plans(docker: &FakeDocker, plans: &[WorkspaceRemovalPlan]) -> Vec<KeptVolume> {
        let client = client(docker);
        block_on(async {
            let mut report = VolumeRemovalReport::default();
            remove_workspace_plans(&client, plans, &mut report).await?;
            report.kept_volumes(&client).await
        })
        .unwrap()
    }

    fn block_on<T>(future: impl Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    fn kept(name: &str, reason: KeptVolumeReason) -> KeptVolume {
        KeptVolume {
            name: name.to_owned(),
            reason,
        }
    }

    // 削除したコンテナが mount していた、decune のラベルの無い volume は消さず、
    // decune-managed ボリュームでないという理由で残したものとして示す
    #[test]
    fn remove_keeps_unlabeled_mounted_volume_as_not_managed() {
        let temp = tempfile::tempdir().unwrap();
        let docker = FakeDocker::new();
        docker.add_volume("user-data", &[]);
        docker.add_volume("x-data", &managed_labels(WORKSPACE_X));
        docker.add_container(
            "x-dev",
            &managed_labels(WORKSPACE_X),
            &["user-data", "x-data"],
        );
        let plan = WorkspaceRemovalPlan {
            containers: vec![standalone("x-dev", &["user-data", "x-data"])],
            volumes: vec!["x-data".to_owned()],
            ..plan(temp.path(), WORKSPACE_X)
        };

        let kept_volumes = run_plans(&docker, &[plan]);

        assert_eq!(
            kept_volumes,
            [kept("user-data", KeptVolumeReason::NotManaged)]
        );
        assert!(docker.volume_exists("user-data"));
        assert!(!docker.volume_exists("x-data"));
    }

    // 使用中で Docker に削除を拒否された decune-managed ボリュームは、削除したコンテナが
    // mount していなくても、使用中という理由で残したものとして示し、remove は失敗しない
    #[test]
    fn remove_reports_in_use_managed_volume_not_mounted_by_removed_containers() {
        let temp = tempfile::tempdir().unwrap();
        let docker = FakeDocker::new();
        docker.add_volume("x-data", &managed_labels(WORKSPACE_X));
        docker.add_container("outside", &[], &["x-data"]);
        let plan = WorkspaceRemovalPlan {
            volumes: vec!["x-data".to_owned()],
            ..plan(temp.path(), WORKSPACE_X)
        };

        let kept_volumes = run_plans(&docker, &[plan]);

        assert_eq!(kept_volumes, [kept("x-data", KeptVolumeReason::InUse)]);
        assert!(docker.volume_exists("x-data"));
    }

    // decune のラベルを持つ volume が使用中で残っても、状態は消す。
    // ラベルが残るので、その volume は状態が無くても後の remove --all-workspaces から辿れる
    #[test]
    fn remove_removes_state_when_only_labeled_volume_is_in_use() {
        let temp = tempfile::tempdir().unwrap();
        let docker = FakeDocker::new();
        docker.add_volume("x-data", &managed_labels(WORKSPACE_X));
        docker.add_container("outside", &[], &["x-data"]);
        let plan = WorkspaceRemovalPlan {
            volumes: vec!["x-data".to_owned()],
            ..plan(temp.path(), WORKSPACE_X)
        };
        let state_dir = plan.state_dir.clone();

        run_plans(&docker, &[plan]);

        assert!(!state_dir.exists());
    }

    // 匿名 volume は、コンテナと一緒に消えても、他のコンテナが使っていて残っても、
    // 残したものとして示さない
    #[test]
    fn remove_does_not_report_anonymous_volumes() {
        let temp = tempfile::tempdir().unwrap();
        let docker = FakeDocker::new();
        docker.add_volume("anon-own", &[ANONYMOUS]);
        docker.add_volume("anon-shared", &[ANONYMOUS]);
        docker.add_container(
            "x-dev",
            &managed_labels(WORKSPACE_X),
            &["anon-own", "anon-shared"],
        );
        docker.add_container("outside", &[], &["anon-shared"]);
        let plan = WorkspaceRemovalPlan {
            containers: vec![standalone("x-dev", &["anon-own", "anon-shared"])],
            ..plan(temp.path(), WORKSPACE_X)
        };

        let kept_volumes = run_plans(&docker, &[plan]);

        assert_eq!(kept_volumes, []);
        assert!(!docker.volume_exists("anon-own"));
        assert!(docker.volume_exists("anon-shared"));
    }

    /// ワークスペース X の decune-managed ボリューム `x-data` を、
    /// Y のコンテナだけが mount している。
    fn shared_volume_plans(docker: &FakeDocker, root: &Path) -> [WorkspaceRemovalPlan; 2] {
        docker.add_volume("x-data", &managed_labels(WORKSPACE_X));
        docker.add_container("y-dev", &managed_labels(WORKSPACE_Y), &["x-data"]);
        [
            WorkspaceRemovalPlan {
                volumes: vec!["x-data".to_owned()],
                ..plan(root, WORKSPACE_X)
            },
            WorkspaceRemovalPlan {
                containers: vec![standalone("y-dev", &["x-data"])],
                ..plan(root, WORKSPACE_Y)
            },
        ]
    }

    // --all-workspaces で、X の volume を同じ実行で消す Y のコンテナだけが mount していても、
    // X を先に処理したときに volume は消える。すべてのコンテナを消してから volume を消すため
    #[test]
    fn remove_all_removes_volume_shared_with_removed_workspace_when_owner_comes_first() {
        let temp = tempfile::tempdir().unwrap();
        let docker = FakeDocker::new();
        let [x, y] = shared_volume_plans(&docker, temp.path());

        let kept_volumes = run_plans(&docker, &[x, y]);

        assert_eq!(kept_volumes, []);
        assert!(!docker.volume_exists("x-data"));
    }

    // 上と同じ構成で、Y を先に処理しても、volume は消える
    #[test]
    fn remove_all_removes_volume_shared_with_removed_workspace_when_owner_comes_last() {
        let temp = tempfile::tempdir().unwrap();
        let docker = FakeDocker::new();
        let [x, y] = shared_volume_plans(&docker, temp.path());

        let kept_volumes = run_plans(&docker, &[y, x]);

        assert_eq!(kept_volumes, []);
        assert!(!docker.volume_exists("x-data"));
    }

    // docker compose down の後に、プロジェクトのラベルを持つ volume(今の Compose ファイルが
    // 宣言していないものなど)が残っていれば、ラベルから探して消す
    #[test]
    fn remove_compose_project_removes_leftover_project_volumes_after_compose_down() {
        let temp = tempfile::tempdir().unwrap();
        let docker = FakeDocker::new();
        docker.add_volume("project_old-data", &[COMPOSE_PROJECT]);
        let plan = WorkspaceRemovalPlan {
            compose_projects: vec![COMPOSE_PROJECT.1.to_owned()],
            compose_projects_removed_by_compose: vec![COMPOSE_PROJECT.1.to_owned()],
            ..plan(temp.path(), WORKSPACE_X)
        };

        let kept_volumes = run_plans(&docker, &[plan]);

        assert_eq!(kept_volumes, []);
        assert!(!docker.volume_exists("project_old-data"));
    }

    // docker compose down が消したプロジェクトの network は、ラベルから消し直さない。
    // down が使用中で残した network を消そうとして、remove を失敗させないため
    #[test]
    fn remove_compose_project_leaves_networks_of_project_removed_by_compose() {
        let temp = tempfile::tempdir().unwrap();
        let docker = FakeDocker::new();
        docker.add_network("project_default", &[COMPOSE_PROJECT]);
        let plan = WorkspaceRemovalPlan {
            compose_projects: vec![COMPOSE_PROJECT.1.to_owned()],
            compose_projects_removed_by_compose: vec![COMPOSE_PROJECT.1.to_owned()],
            ..plan(temp.path(), WORKSPACE_X)
        };

        run_plans(&docker, &[plan]);

        assert!(docker.network_exists("project_default"));
    }

    // Compose プロジェクトをラベルから消すとき、プロジェクトの volume は消し、サービスが
    // mount していた external の volume は消さずに残したものとして示す。どのサービスも
    // mount していない external の volume は示さない
    #[test]
    fn remove_compose_project_keeps_external_volume_mounted_by_service() {
        let temp = tempfile::tempdir().unwrap();
        let docker = FakeDocker::new();
        docker.add_volume("project_db", &[COMPOSE_PROJECT]);
        docker.add_volume("shared-external", &[]);
        docker.add_volume("unused-external", &[]);
        docker.add_container(
            "project-db-1",
            &[COMPOSE_PROJECT],
            &["project_db", "shared-external"],
        );
        let plan = WorkspaceRemovalPlan {
            compose_projects: vec![COMPOSE_PROJECT.1.to_owned()],
            ..plan(temp.path(), WORKSPACE_X)
        };

        let kept_volumes = run_plans(&docker, &[plan]);

        assert_eq!(
            kept_volumes,
            [kept("shared-external", KeptVolumeReason::NotManaged)]
        );
        assert!(!docker.container_exists("project-db-1"));
        assert!(!docker.volume_exists("project_db"));
        assert!(docker.volume_exists("unused-external"));
    }

    // Compose プロジェクトの volume をプロジェクトの外のコンテナが参照しているとき、remove は
    // volume と状態を残して成功し、使用中が解けた後の remove で両方を消す
    //
    // シナリオ:
    //   1. プロジェクトの volume を、プロジェクトの外のコンテナにも mount させる
    //   2. remove する → volume は使用中として残り、状態は残り、ランタイムディレクトリは消える
    //   3. 外のコンテナを消し、同じ内容の計画を作り直して remove する → volume と状態が消える
    #[test]
    fn remove_keeps_state_while_compose_project_volume_is_in_use() {
        let temp = tempfile::tempdir().unwrap();
        let docker = FakeDocker::new();
        docker.add_volume("project_db", &[COMPOSE_PROJECT]);
        docker.add_container("project-db-1", &[COMPOSE_PROJECT], &["project_db"]);
        docker.add_container("outside", &[], &["project_db"]);
        let first = WorkspaceRemovalPlan {
            compose_projects: vec![COMPOSE_PROJECT.1.to_owned()],
            ..plan(temp.path(), WORKSPACE_X)
        };
        let state_dir = first.state_dir.clone();
        let runtime_dir = first.runtime_dir.clone();

        let kept_volumes = run_plans(&docker, &[first]);

        assert_eq!(kept_volumes, [kept("project_db", KeptVolumeReason::InUse)]);
        assert!(!docker.container_exists("project-db-1"));
        assert!(docker.volume_exists("project_db"));
        assert!(state_dir.join("state.toml").exists());
        assert!(!runtime_dir.exists());

        assert!(
            block_on(
                client(&docker)
                    .cli()
                    .remove_container("outside", true, true)
            )
            .is_ok()
        );
        let second = WorkspaceRemovalPlan {
            compose_projects: vec![COMPOSE_PROJECT.1.to_owned()],
            ..plan(temp.path(), WORKSPACE_X)
        };

        let kept_volumes = run_plans(&docker, &[second]);

        assert_eq!(kept_volumes, []);
        assert!(!docker.volume_exists("project_db"));
        assert!(!state_dir.exists());
    }

    // --all-workspaces で、あるワークスペースの削除の後に残っても、後のワークスペースの削除で
    // 消えた volume は、残したものとして示さない
    #[test]
    fn remove_all_does_not_report_volume_removed_by_later_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let docker = FakeDocker::new();
        docker.add_volume("y-data", &managed_labels(WORKSPACE_Y));
        docker.add_container("x-dev", &managed_labels(WORKSPACE_X), &["y-data"]);
        let x = WorkspaceRemovalPlan {
            containers: vec![standalone("x-dev", &["y-data"])],
            ..plan(temp.path(), WORKSPACE_X)
        };
        let y = WorkspaceRemovalPlan {
            volumes: vec!["y-data".to_owned()],
            ..plan(temp.path(), WORKSPACE_Y)
        };

        let kept_volumes = run_plans(&docker, &[x, y]);

        assert_eq!(kept_volumes, []);
        assert!(!docker.volume_exists("y-data"));
    }

    // 確認の文は、削除する volume の名前を [y/N] の問いより前に並べる
    #[test]
    fn remove_prompt_lists_volumes_before_question() {
        let prompt = workspace_remove_prompt(&["app_db".to_owned(), "cache".to_owned()]);

        let question = prompt.find("[y/N]").unwrap();
        assert!(prompt.find("app_db").unwrap() < question);
        assert!(prompt.find("cache").unwrap() < question);
    }

    // TTY で確認するとき、削除する decune-managed ボリュームを確認の文に含め、確認より前に
    // Docker の状態を変えない。確認で断れば、何も消さずに取り消す
    #[test]
    fn remove_workspace_shows_volumes_before_confirmation_without_docker_changes() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::resolve(temp.path()).unwrap();
        let docker = FakeDocker::new();
        docker.add_volume("workspace-data", &managed_labels(workspace.id()));
        docker.add_volume("user-data", &[]);
        let mut prompted = None;

        let result = block_on(remove_workspace(
            &client(&docker),
            &workspace,
            false,
            Some(|prompt: &str| {
                prompted = Some((prompt.to_owned(), docker.mutating_commands()));
                Ok(false)
            }),
        ));

        assert!(result.unwrap_err().to_string().contains("Remove cancelled"));
        let (prompt, mutating_before_prompt) = prompted.unwrap();
        assert!(prompt.contains("workspace-data"), "{prompt}");
        assert!(!prompt.contains("user-data"), "{prompt}");
        assert_eq!(mutating_before_prompt, Vec::<Vec<String>>::new());
        assert_eq!(docker.mutating_commands(), Vec::<Vec<String>>::new());
        assert!(docker.volume_exists("workspace-data"));
    }

    // --all-workspaces の確認の前の一覧は、ワークスペースごとに、削除する decune のラベルの
    // volume と Compose プロジェクトの volume の名前を並べる
    #[test]
    fn remove_all_summary_lists_volume_names_per_workspace() {
        let x = WorkspaceRemovalPlan {
            volumes: vec!["x-data".to_owned()],
            compose_volumes: vec!["project_db".to_owned()],
            ..empty_removal_plan(WORKSPACE_X)
        };
        let y = WorkspaceRemovalPlan {
            volumes: vec!["y-data".to_owned()],
            ..empty_removal_plan(WORKSPACE_Y)
        };

        let lines = remove_all_summary_lines(&[x, y], false);

        let position = |text: &str| lines.iter().position(|line| line.contains(text)).unwrap();
        assert!(position(WORKSPACE_X) < position("x-data"));
        assert!(position(WORKSPACE_X) < position("project_db"));
        assert!(position("x-data") < position(WORKSPACE_Y));
        assert!(position("project_db") < position(WORKSPACE_Y));
        assert!(position(WORKSPACE_Y) < position("y-data"));
    }
}
