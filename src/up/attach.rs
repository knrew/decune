use anyhow::{Context, Result};

use crate::{
    docker::{
        client::DockerClient,
        exec::{ExecCommandSpec, exec_attach, resolve_exec_env, run_attached_exec_stdio},
        user::resolve_remote_user,
    },
    up::{
        shell::{first_successful_shell_candidate, shell_command_candidates},
        types::UpPlan,
    },
};

pub(in crate::up) async fn attach_shell(
    client: &DockerClient,
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
