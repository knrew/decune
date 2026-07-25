use std::path::Path;

use anyhow::{Context, Result};

use crate::{
    config::variables::{VariableContext, VariableContextInput, expand_remote_env_tracked},
    docker::{container::inspect_container_env, dotfiles::setup_dotfiles, exec::resolve_exec_env},
    host::credentials::{
        install_staged_host_gitconfig, remove_staged_host_gitconfig, setup_git_credentials,
        setup_github_cli_credentials,
    },
};

use super::context::{LifecycleRunContext, PreparedLifecycleRunContext};

pub(crate) async fn prepare_container_lifecycle(
    context: LifecycleRunContext<'_>,
) -> Result<PreparedLifecycleRunContext<'_>> {
    let dotfile_variables = dotfile_variable_context(&context);
    let dotfiles_result = setup_dotfiles(
        context.client,
        &context.container,
        context.config,
        &context.remote_user,
        &dotfile_variables,
    )
    .await;
    if dotfiles_result.is_err() {
        remove_staged_host_gitconfig(context.runtime_dir)?;
    }
    dotfiles_result?;

    let install_result = install_staged_host_gitconfig(
        context.client,
        &context.container,
        context.config,
        &context.remote_user,
    )
    .await;
    let cleanup_result = remove_staged_host_gitconfig(context.runtime_dir);
    install_result?;
    cleanup_result?;

    setup_git_credentials(
        context.client,
        &context.container,
        context.config,
        &context.remote_user,
    )
    .await?;
    setup_github_cli_credentials(
        context.client,
        &context.container,
        context.config,
        &context.remote_user,
    )
    .await?;
    let container_env = inspect_container_env(context.client, &context.container).await?;
    let remote_env_variables = dotfile_variable_context(&context)
        .with_container_env(container_env, context.sensitive_container_env);
    let remote_env = expand_remote_env_tracked(
        &context.config.devcontainer.remote_env,
        &remote_env_variables,
    )
    .with_context(|| {
        format!(
            "Failed to expand remoteEnv for container: {}",
            context.container
        )
    })?;
    let remote_env_redactions = remote_env.sensitive.redaction_values();
    let remote_env = remote_env.values;
    let remote_process_env = resolve_exec_env(
        context.client,
        &context.container,
        &context.remote_user.user,
        context.remote_user.shell.as_deref(),
        &remote_env,
        context.config.devcontainer.user_env_probe,
    )
    .await?;

    Ok(PreparedLifecycleRunContext {
        client: context.client,
        container: context.container,
        config: context.config,
        workspace_root: context.workspace_root,
        workspace_folder: context.workspace_folder,
        remote_user: context.remote_user,
        remote_env,
        remote_process_env,
        remote_env_redactions,
    })
}

fn dotfile_variable_context(context: &LifecycleRunContext<'_>) -> VariableContext {
    VariableContext::new(VariableContextInput {
        local_workspace_folder: context.workspace_root.to_path_buf(),
        local_workspace_folder_basename: context.workspace_basename.to_owned(),
        container_workspace_folder: context.workspace_folder.to_owned(),
        container_workspace_folder_basename: container_workspace_folder_basename(
            context.workspace_folder,
            context.workspace_basename,
        ),
        devcontainer_id: context.workspace_id.to_owned(),
        uid: current_uid(),
        gid: current_gid(),
        remote_user: context.remote_user.user.clone(),
        remote_user_home: context.remote_user.home.clone(),
    })
}

fn container_workspace_folder_basename(workspace_folder: &str, workspace_basename: &str) -> String {
    Path::new(workspace_folder)
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or(workspace_basename)
        .to_owned()
}

#[cfg(unix)]
fn current_uid() -> u32 {
    // SAFETY: getuid has no preconditions, takes no pointers, and cannot fail.
    unsafe { libc::getuid() }
}

#[cfg(not(unix))]
fn current_uid() -> u32 {
    0
}

#[cfg(unix)]
fn current_gid() -> u32 {
    // SAFETY: getgid has no preconditions, takes no pointers, and cannot fail.
    unsafe { libc::getgid() }
}

#[cfg(not(unix))]
fn current_gid() -> u32 {
    0
}
