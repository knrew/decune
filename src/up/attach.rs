use anyhow::{Context, Result};

use crate::{
    config::variables::expand_remote_env,
    docker::{
        client::DockerClient,
        container::inspect_container_env,
        exec::{ExecCommandSpec, exec_capture_output, resolve_exec_env, run_attached_exec_stdio},
        user::resolve_remote_user,
    },
    up::{
        mounts::mount_variable_context,
        shell::{first_successful_shell_candidate, shell_command_candidates},
        types::UpPlan,
    },
    workspace::Workspace,
};

pub(in crate::up) async fn attach_shell(
    client: &DockerClient,
    workspace: &Workspace,
    plan: &UpPlan,
    container_name: &str,
) -> Result<i64> {
    let remote_user = resolve_remote_user(
        client,
        container_name,
        &plan.effective_users,
        &plan.uid_gid_sync_plan,
    )
    .await?;
    let container_env = inspect_container_env(client, container_name).await?;
    let remote_env_variables = mount_variable_context(
        workspace,
        &plan.workspace_folder,
        remote_user.user.clone(),
        remote_user.home.clone(),
    )
    .with_container_env(container_env);
    let remote_env = expand_remote_env(&plan.config.devcontainer.remote_env, &remote_env_variables)
        .with_context(|| format!("Failed to expand remoteEnv for container: {container_name}"))?;
    let env = resolve_exec_env(
        client,
        container_name,
        &remote_user.user,
        remote_user.shell.as_deref(),
        &remote_env,
        plan.config.devcontainer.user_env_probe,
    )
    .await?;
    let command = if let Some(shell) = plan
        .config
        .shell
        .as_deref()
        .map(str::trim)
        .filter(|shell| !shell.is_empty())
    {
        shell.to_owned()
    } else {
        let candidates = shell_command_candidates(None, remote_user.shell.as_deref());
        first_successful_shell_candidate(candidates, |command| {
            let env = env.clone();
            let user = remote_user.user.clone();

            async move {
                let output = exec_capture_output(
                    client,
                    container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-lc".to_owned(),
                            "command -v \"$1\" >/dev/null 2>&1".to_owned(),
                            "decune-shell-probe".to_owned(),
                            command.clone(),
                        ],
                        user: Some(user),
                        working_dir: None,
                        env,
                        tty: false,
                    },
                )
                .await?;

                if output.exit_code == 0 {
                    Ok(command)
                } else {
                    anyhow::bail!("shell command was not found")
                }
            }
        })
        .await
        .with_context(|| {
            format!("Failed to select an attached shell in container: {container_name}")
        })?
    };
    let spec = {
        let env = env.clone();
        let user = remote_user.user.clone();
        let working_dir = plan.workspace_folder.clone();

        ExecCommandSpec {
            command: vec![command],
            user: Some(user),
            working_dir: Some(working_dir),
            env,
            tty: true,
        }
    };

    run_attached_exec_stdio(client, container_name, &spec).await
}
