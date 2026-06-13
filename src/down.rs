use std::{
    io::{self, IsTerminal, Write},
    path::PathBuf,
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
    runtime::compose_cli::{ComposeDownOptions, ComposeLifecyclePlan, DockerComposeCli},
    state::remove_state_runtime_dirs,
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
    if let Some(plan) = compose_lifecycle_plan(&workspace, ComposeLifecycleCommand::Down)? {
        DockerComposeCli::default()
            .stop(&plan.project, &plan.services)
            .await?;
        ui::done(&format!(
            "Stopped Docker Compose project: {}",
            plan.project.project_name
        ));
        return Ok(());
    }

    let client = DockerClient::connect_from_env()?;
    let containers = list_managed_containers(&client, workspace.id()).await?;

    if containers.is_empty() {
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
    if let Some(plan) = compose_lifecycle_plan(
        &workspace,
        ComposeLifecycleCommand::Clean {
            images: options.images,
        },
    )? {
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

        if plan.cleanup.remove_generated_images {
            let client = DockerClient::connect_from_env()?;
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
        return Ok(());
    }

    let client = DockerClient::connect_from_env()?;
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

    if options.images {
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
        .list_workspace_containers(workspace_id)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComposeLifecycleCommand {
    Down,
    Clean { images: bool },
}

fn compose_lifecycle_plan(
    workspace: &Workspace,
    command: ComposeLifecycleCommand,
) -> Result<Option<ComposeLifecyclePlan>> {
    if !has_devcontainer_metadata_hint(workspace) {
        return Ok(None);
    }

    let plan = build_up_plan_with_forwarding_resolution(
        workspace,
        None,
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
    use anyhow::Result;

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
}
