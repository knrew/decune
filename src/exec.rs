//! `decune exec`: runs one command in the primary container with the user, environment, and
//! working directory of the shell of the last `up`.
//!
//! The command reads only `state.toml` and the target container. It does not read
//! `devcontainer.json` or the decune config, and it changes no state, runtime files, or Docker
//! resources, so it can run beside an attached `decune up` session.

use std::{collections::BTreeMap, path::PathBuf};

use anyhow::{Context, Result, bail};

use crate::{
    config::variables::{SensitiveEnvMap, SensitiveEnvValue, expand_remote_env_tracked},
    docker::{
        client::DockerClient,
        container::{ContainerInspect, container_env_from_inspect},
        exec::{ExecCommandSpec, resolve_exec_env, run_attached_exec_stdio},
        resource::managed_workspace_id_from_container,
    },
    state::{ExecContextState, load_state_file, state_file_path},
    terminal::StdioTerminals,
    up::{clamp_exit_code, mount_variable_context},
    workspace::Workspace,
};

pub(crate) struct ExecOptions {
    pub(crate) workspace: PathBuf,
    pub(crate) command: Vec<String>,
}

/// Runs the command and returns its exit code, or 1 when the code is outside 0..=255.
pub(crate) async fn run_exec(options: ExecOptions) -> Result<i32> {
    let workspace = Workspace::resolve(&options.workspace)?;
    let context = load_exec_context(&workspace)?;
    let client = DockerClient::connect_from_env();
    let container = inspect_exec_target(&client, &workspace, &context).await?;
    let container_env = container_env_from_inspect(&container);
    let sensitive_container_env =
        recorded_sensitive_container_env(&context.sensitive_container_env_keys, &container_env);
    let remote_env_variables = mount_variable_context(
        &workspace,
        &context.workspace_folder,
        context.remote_user.clone(),
        context.remote_user_home.clone(),
    )
    .with_container_env(container_env, &sensitive_container_env);
    let remote_env = expand_remote_env_tracked(&context.remote_env, &remote_env_variables)
        .with_context(|| {
            format!(
                "Failed to expand remoteEnv for container: {}",
                context.container_id
            )
        })?;
    let remote_env_redactions = remote_env.sensitive.redaction_values();
    let mut probe_redactions = sensitive_container_env.redaction_values();
    probe_redactions.extend(remote_env_redactions.iter().cloned());
    let env = resolve_exec_env(
        &client,
        &context.container_id,
        &context.remote_user,
        context.remote_user_shell.as_deref(),
        &remote_env.values,
        Some(context.user_env_probe.into()),
        &probe_redactions,
    )
    .await?;
    let spec = ExecCommandSpec {
        command: options.command,
        user: Some(context.remote_user),
        working_dir: Some(context.workspace_folder),
        env,
        redactions: remote_env_redactions,
        tty: StdioTerminals::detect().allocates_exec_tty(),
    };

    let exit_code = run_attached_exec_stdio(&client, &context.container_id, &spec).await?;
    Ok(clamp_exit_code(exit_code))
}

fn load_exec_context(workspace: &Workspace) -> Result<ExecContextState> {
    let state_dir = workspace.paths().state_dir();
    let context = load_state_file(state_dir)?.and_then(|state| state.exec_context);
    let Some(context) = context else {
        bail!(
            "No dev container started by decune up is recorded for workspace: {} (state file: {}). Run decune up to start the dev container.",
            workspace.root().display(),
            state_file_path(state_dir).display()
        );
    };

    Ok(context)
}

/// Inspects the recorded container and requires it to be a running container of this
/// workspace.
///
/// The recorded container ID is the only way to find the target, because `exec` reads no
/// configuration. A container that decune recreated, or that was replaced outside decune, has a
/// new ID, so it is reported as missing rather than run with a context recorded for another
/// container.
async fn inspect_exec_target(
    client: &DockerClient,
    workspace: &Workspace,
    context: &ExecContextState,
) -> Result<ContainerInspect> {
    let container_id = &context.container_id;
    let Some(container) = client
        .cli()
        .inspect_container_if_present(container_id)
        .await?
    else {
        bail!(
            "Dev container recorded by decune up no longer exists: {container_id}. Run decune up to start the dev container."
        );
    };
    let workspace_id = managed_workspace_id_from_container(&container).map(|(id, _)| id);
    if workspace_id.as_deref() != Some(workspace.id()) {
        bail!(
            "Container recorded by decune up is not a decune-managed container of this workspace: {container_id}. Run decune up to start the dev container."
        );
    }
    let status = container
        .state
        .as_ref()
        .and_then(|state| state.status.as_deref());
    // Docker reports `Running: true` for a paused container, so the status decides.
    if status != Some("running") {
        bail!(
            "Dev container recorded by decune up is not running (status: {}): {container_id}. Run decune up to start the dev container.",
            status.unwrap_or("unknown")
        );
    }

    Ok(container)
}

/// Rebuilds the secret-sensitive `containerEnv` values from the recorded key names and the
/// environment of the running container.
///
/// Each value is tracked as a whole. The state keeps no `containerEnv` template, so a part of a
/// value that came from `${localEnv:...}` cannot be told apart from the rest of the value.
fn recorded_sensitive_container_env(
    keys: &[String],
    container_env: &BTreeMap<String, String>,
) -> SensitiveEnvMap {
    let mut sensitive = SensitiveEnvMap::default();
    for key in keys {
        if let Some(value) = container_env.get(key) {
            sensitive.insert(
                key.clone(),
                SensitiveEnvValue {
                    value: value.clone(),
                    redactions: vec![value.clone()],
                },
            );
        }
    }

    sensitive
}
