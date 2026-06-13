use std::{
    collections::BTreeSet,
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
    host::{credentials::cleanup_github_cli_token_file, daemon::cleanup_host_daemon_socket},
    runtime::compose_cli::{
        ComposeDownOptions, ComposeLifecyclePlan, ComposeStopOptions, DockerComposeCli,
    },
    state::{load_state_file, remove_state_runtime_dirs},
    ui,
    up::{ForwardingResolution, build_up_plan_with_forwarding_resolution},
    workspace::Workspace,
};

const DEFAULT_STOP_TIMEOUT_SECONDS: i32 = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DownOptions {
    pub(crate) workspace: PathBuf,
    pub(crate) timeout_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CleanOptions {
    pub(crate) workspace: PathBuf,
    pub(crate) images: bool,
    pub(crate) force: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ManagedContainer {
    id: String,
    name: String,
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

pub(crate) async fn run_clean(options: CleanOptions) -> Result<()> {
    let stdin_is_terminal = io::stdin().is_terminal();
    ensure_clean_confirmed(options.force, stdin_is_terminal, confirm_clean)?;

    let workspace = Workspace::resolve(&options.workspace)?;
    cleanup_github_cli_token_file(workspace.paths().runtime_dir());
    cleanup_host_daemon_socket(workspace.paths().runtime_dir()).await;
    let client = DockerClient::connect_from_env()?;
    let mut compose_project_names = compose_fallback_project_names(&workspace, &client).await?;
    let mut remove_generated_images = options.images;
    let mut compose_projects_removed_by_compose = Vec::new();
    match compose_lifecycle_plan(
        &workspace,
        ComposeLifecycleCommand::Clean {
            images: options.images,
        },
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

    remove_state_runtime_dirs(
        workspace.paths().state_dir(),
        workspace.paths().runtime_dir(),
    )?;
    ui::done("Cleaned dev container resources");
    Ok(())
}

pub(crate) fn clean_requires_confirmation(force: bool, stdin_is_terminal: bool) -> bool {
    !force && stdin_is_terminal
}

pub(crate) fn clean_rejects_non_interactive(force: bool, stdin_is_terminal: bool) -> bool {
    !force && !stdin_is_terminal
}

fn ensure_clean_confirmed(
    force: bool,
    stdin_is_terminal: bool,
    confirm: impl FnOnce() -> Result<bool>,
) -> Result<()> {
    if clean_rejects_non_interactive(force, stdin_is_terminal) {
        bail!(
            "Cannot confirm clean in a non-interactive terminal; rerun with --force to remove resources"
        );
    }
    if clean_requires_confirmation(force, stdin_is_terminal) && !confirm()? {
        bail!("Clean cancelled");
    }

    Ok(())
}

fn confirm_clean() -> Result<bool> {
    let mut stderr = io::stderr();
    stderr
        .write_all(b"Remove decune resources for this workspace? [y/N] ")
        .context("Failed to write clean confirmation prompt")?;
    stderr
        .flush()
        .context("Failed to flush clean confirmation prompt")?;

    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("Failed to read clean confirmation response")?;

    Ok(clean_confirmation_response_is_yes(&input))
}

fn clean_confirmation_response_is_yes(input: &str) -> bool {
    matches!(input.trim(), "y" | "Y" | "yes" | "YES" | "Yes")
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
    Clean { images: bool },
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
        ComposeLifecycleCommand::Clean { images } => {
            ComposeLifecyclePlan::clean(command_plan, images)
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
    fn clean_confirmation_is_required_only_for_interactive_non_force_runs() {
        assert!(super::clean_requires_confirmation(false, true));
        assert!(!super::clean_requires_confirmation(true, true));
        assert!(!super::clean_requires_confirmation(false, false));
    }

    #[test]
    fn clean_non_interactive_without_force_is_rejected_before_cleanup() {
        assert!(super::clean_rejects_non_interactive(false, false));
        assert!(!super::clean_rejects_non_interactive(true, false));
        assert!(!super::clean_rejects_non_interactive(false, true));
    }

    #[test]
    fn clean_prompt_accepts_only_explicit_yes() {
        assert!(super::clean_confirmation_response_is_yes("y\n"));
        assert!(super::clean_confirmation_response_is_yes("yes\n"));
        assert!(super::clean_confirmation_response_is_yes("YES\n"));
        assert!(!super::clean_confirmation_response_is_yes("\n"));
        assert!(!super::clean_confirmation_response_is_yes("no\n"));
    }

    #[test]
    fn clean_confirmation_gate_handles_interactive_accept_and_reject() {
        assert!(super::ensure_clean_confirmed(false, true, || Ok(true)).is_ok());

        let error = super::ensure_clean_confirmed(false, true, || Ok(false)).unwrap_err();
        assert!(error.to_string().contains("Clean cancelled"));
    }

    #[test]
    fn clean_confirmation_gate_rejects_non_interactive_and_skips_prompt_for_force() {
        let error = super::ensure_clean_confirmed(false, false, || Ok(true)).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Cannot confirm clean in a non-interactive terminal")
        );

        let mut prompted = false;
        assert!(
            super::ensure_clean_confirmed(true, false, || -> Result<bool> {
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
