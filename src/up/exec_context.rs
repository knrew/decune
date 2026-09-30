use anyhow::{Context, Result};

use crate::{
    devcontainer::lifecycle::PreparedLifecycleRunContext,
    docker::{exec::effective_user_env_probe, user::ResolvedRemoteUser},
    state::ExecContextState,
    up::{start::StartedUpContainer, types::UpPlan},
};

/// Builds the exec context that `decune exec` reuses from the target and remote user that this
/// `up` resolved for its lifecycle commands and shell.
pub(in crate::up) async fn exec_context_for_up(
    started: &StartedUpContainer,
    lifecycle: &PreparedLifecycleRunContext<'_>,
) -> Result<ExecContextState> {
    let container_id = exec_context_container_id(started, lifecycle.container()).await?;

    Ok(exec_context_state(
        container_id,
        lifecycle.remote_user(),
        &started.plan,
    ))
}

/// Returns the full Docker ID of the primary container.
///
/// In image and Dockerfile modes the lifecycle target is the container name, which stays the
/// same across recreation, so the ID comes from the `up` outcome instead. In Compose mode the
/// target comes from `docker compose ps`, whose IDs may be truncated, so it is inspected.
async fn exec_context_container_id(started: &StartedUpContainer, target: &str) -> Result<String> {
    if started.plan.compose_project.is_none() {
        return Ok(started.outcome.container_id.clone());
    }

    started
        .client
        .cli()
        .inspect_container(target)
        .await?
        .id
        .with_context(|| format!("Docker inspect response did not include container id: {target}"))
}

fn exec_context_state(
    container_id: String,
    remote_user: &ResolvedRemoteUser,
    plan: &UpPlan,
) -> ExecContextState {
    ExecContextState {
        container_id,
        remote_user: remote_user.user.clone(),
        remote_user_home: remote_user.home.clone(),
        remote_user_shell: remote_user.shell.clone(),
        workspace_folder: plan.workspace_folder.clone(),
        user_env_probe: effective_user_env_probe(plan.config.devcontainer.user_env_probe).into(),
        sensitive_container_env_keys: plan
            .sensitive_container_env
            .iter()
            .map(|(key, _)| key.clone())
            .collect(),
        remote_env: plan.config.devcontainer.remote_env.clone(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::{
        config::{resolved::ResolvedUserEnvProbe, variables::SensitiveEnvValue},
        docker::user::{RemoteUserSource, ResolvedRemoteUser},
        state::UserEnvProbeSnapshot,
        up::test_support::test_up_plan_with_image_source,
    };

    use super::exec_context_state;

    fn remote_user() -> ResolvedRemoteUser {
        ResolvedRemoteUser {
            user: "1000:1000".to_owned(),
            uid: 1000,
            gid: 1000,
            home: Some("/home/vscode".to_owned()),
            shell: Some("/bin/zsh".to_owned()),
            source: RemoteUserSource::Explicit,
            fallback_from: None,
        }
    }

    // The exec context holds the runtime user with its home and shell, the workspace folder,
    // the userEnvProbe, the remoteEnv template, and the secret-sensitive containerEnv keys.
    #[test]
    fn exec_context_records_the_shell_context_of_the_up() {
        let mut plan = test_up_plan_with_image_source("alpine:3.20");
        plan.workspace_folder = "/workspaces/app".to_owned();
        plan.config.devcontainer.user_env_probe = Some(ResolvedUserEnvProbe::LoginShell);
        plan.config.devcontainer.remote_env =
            BTreeMap::from([("HOME_BIN".to_owned(), "${remoteUserHome}/bin".to_owned())]);
        plan.sensitive_container_env.insert(
            "NPM_TOKEN",
            SensitiveEnvValue {
                value: "npm-secret".to_owned(),
                redactions: vec!["npm-secret".to_owned()],
            },
        );

        let context = exec_context_state("container-id".to_owned(), &remote_user(), &plan);

        assert_eq!(context.container_id, "container-id");
        assert_eq!(context.remote_user, "1000:1000");
        assert_eq!(context.remote_user_home.as_deref(), Some("/home/vscode"));
        assert_eq!(context.remote_user_shell.as_deref(), Some("/bin/zsh"));
        assert_eq!(context.workspace_folder, "/workspaces/app");
        assert_eq!(context.user_env_probe, UserEnvProbeSnapshot::LoginShell);
        assert_eq!(
            context.remote_env.get("HOME_BIN").map(String::as_str),
            Some("${remoteUserHome}/bin")
        );
        assert_eq!(context.sensitive_container_env_keys, vec!["NPM_TOKEN"]);
    }

    // Without a userEnvProbe setting, the exec context records the default probe that the
    // `up` ran, so `exec` does not depend on the default at its own run time.
    #[test]
    fn exec_context_records_the_default_user_env_probe() {
        let plan = test_up_plan_with_image_source("alpine:3.20");

        let context = exec_context_state("container-id".to_owned(), &remote_user(), &plan);

        assert_eq!(
            context.user_env_probe,
            UserEnvProbeSnapshot::LoginInteractiveShell
        );
    }

    // The exec context keeps secret-sensitive containerEnv values out of the state file.
    #[test]
    fn exec_context_keeps_sensitive_container_env_values_out_of_state() {
        let mut plan = test_up_plan_with_image_source("alpine:3.20");
        plan.sensitive_container_env.insert(
            "NPM_TOKEN",
            SensitiveEnvValue {
                value: "Bearer npm-secret".to_owned(),
                redactions: vec!["npm-secret".to_owned(), "Bearer npm-secret".to_owned()],
            },
        );

        let context = exec_context_state("container-id".to_owned(), &remote_user(), &plan);
        let serialized = toml::to_string(&context).unwrap();

        assert!(serialized.contains("NPM_TOKEN"), "{serialized}");
        assert!(!serialized.contains("npm-secret"), "{serialized}");
    }
}
