use std::{collections::BTreeMap, path::Path, process::Command as HostCommand};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::future::join_all;

use crate::{
    config::{resolved::ResolvedConfig, types::Command},
    docker::exec::{ExecCommandSpec, ExecOutput, exec_capture_output},
};

use super::{
    context::PreparedLifecycleRunContext,
    types::{LifecycleCommand, LifecycleStage},
};

pub(in crate::devcontainer::lifecycle) async fn run_lifecycle_stage(
    context: &PreparedLifecycleRunContext<'_>,
    stage: LifecycleStage,
) -> Result<()> {
    let Some(lifecycle) = &context.config.devcontainer.lifecycle else {
        crate::ui::skipped(stage.property_name());
        return Ok(());
    };
    if lifecycle.commands(stage).is_empty() {
        crate::ui::skipped(stage.property_name());
        return Ok(());
    }

    crate::ui::status("Running", stage.property_name());
    for command in lifecycle.commands(stage) {
        run_container_lifecycle_command(context, stage, command).await?;
    }

    Ok(())
}

pub(in crate::devcontainer::lifecycle) fn run_host_lifecycle_command(
    config: &ResolvedConfig,
    workspace_root: &Path,
    stage: LifecycleStage,
) -> Result<()> {
    let Some(lifecycle) = &config.devcontainer.lifecycle else {
        crate::ui::skipped(stage.property_name());
        return Ok(());
    };
    if lifecycle.commands(stage).is_empty() {
        crate::ui::skipped(stage.property_name());
        return Ok(());
    }

    crate::ui::status("Running", stage.property_name());
    for command in lifecycle.commands(stage) {
        run_host_lifecycle_command_value(workspace_root, stage, command)?;
    }

    Ok(())
}

pub(in crate::devcontainer::lifecycle) fn run_host_lifecycle_command_value(
    workspace_root: &Path,
    stage: LifecycleStage,
    command: &LifecycleCommand,
) -> Result<()> {
    match command {
        LifecycleCommand::Shell(_) | LifecycleCommand::Args(_) => run_host_process(
            stage.property_name(),
            &lifecycle_command_argv(command),
            workspace_root,
        ),
        LifecycleCommand::Parallel(commands) => run_host_parallel(workspace_root, stage, commands),
    }
}

async fn run_container_lifecycle_command(
    context: &PreparedLifecycleRunContext<'_>,
    stage: LifecycleStage,
    command: &LifecycleCommand,
) -> Result<()> {
    match command {
        LifecycleCommand::Shell(_) | LifecycleCommand::Args(_) => {
            let argv = lifecycle_command_argv(command);
            run_container_process(context, stage.property_name(), argv, None).await
        }
        LifecycleCommand::Parallel(commands) => {
            let futures = commands.iter().map(|(name, command)| async move {
                let argv = lifecycle_command_argv(command);
                let stage_name = format!("{}.{name}", stage.property_name());
                run_container_process(context, &stage_name, argv, None).await
            });
            let results = join_all(futures).await;
            let mut first_error = None;
            for result in results {
                if let Err(error) = result
                    && first_error.is_none()
                {
                    first_error = Some(error);
                }
            }
            if let Some(error) = first_error {
                return Err(error);
            }
            Ok(())
        }
    }
}

fn run_host_parallel(
    workspace_root: &Path,
    stage: LifecycleStage,
    commands: &BTreeMap<String, LifecycleCommand>,
) -> Result<()> {
    let handles = commands
        .iter()
        .map(|(name, command)| {
            let stage_name = format!("{}.{}", stage.property_name(), name);
            let argv = lifecycle_command_argv(command);
            let workdir = workspace_root.to_path_buf();
            std::thread::spawn(move || run_host_process(&stage_name, &argv, &workdir))
        })
        .collect::<Vec<_>>();

    let mut first_error = None;
    for handle in handles {
        match handle.join() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
            Err(_) => {
                if first_error.is_none() {
                    first_error = Some(anyhow!(
                        "Lifecycle stage {} failed because a host command thread panicked",
                        stage.property_name()
                    ));
                }
            }
        }
    }

    first_error.map_or_else(|| Ok(()), Err)
}

pub(in crate::devcontainer::lifecycle) async fn run_container_process(
    context: &PreparedLifecycleRunContext<'_>,
    stage_name: &str,
    command: Vec<String>,
    hook_context: Option<(String, String)>,
) -> Result<()> {
    let (user, working_dir) = hook_context.unwrap_or_else(|| {
        (
            context.remote_user.user.clone(),
            context.workspace_folder.to_owned(),
        )
    });
    let output = exec_capture_output(
        context.client,
        &context.container,
        &ExecCommandSpec {
            command: command.clone(),
            user: Some(user.clone()),
            working_dir: Some(working_dir),
            env: lifecycle_process_env(context, &user),
            redactions: context.lifecycle_redactions.clone(),
            tty: false,
        },
    )
    .await
    .with_context(|| format!("Failed to run lifecycle stage {stage_name}"))?;

    ensure_lifecycle_success(stage_name, &command, &output, &context.lifecycle_redactions)
}

fn lifecycle_process_env(
    context: &PreparedLifecycleRunContext<'_>,
    user: &str,
) -> BTreeMap<String, String> {
    if same_container_user(user, &context.remote_user.user) {
        return context.remote_process_env.clone();
    }

    context.remote_env.clone()
}

pub(in crate::devcontainer::lifecycle) fn same_container_user(left: &str, right: &str) -> bool {
    let left = docker_user_lookup_key(left);
    let right = docker_user_lookup_key(right);

    left == right || (is_root_user(left) && is_root_user(right))
}

fn docker_user_lookup_key(user: &str) -> &str {
    user.split_once(':').map_or(user, |(name, _)| name).trim()
}

fn is_root_user(user: &str) -> bool {
    matches!(user, "root" | "0")
}

pub(in crate::devcontainer::lifecycle) fn run_host_process(
    stage_name: &str,
    command_argv: &[String],
    workdir: &Path,
) -> Result<()> {
    let (program, args) = command_argv
        .split_first()
        .with_context(|| format!("Lifecycle stage {stage_name} command must not be empty"))?;
    let output = HostCommand::new(program)
        .args(args)
        .current_dir(workdir)
        .output()
        .with_context(|| {
            format!(
                "Failed to run lifecycle stage {stage_name} in directory: {}",
                workdir.display()
            )
        })?;
    let exit_code = output.status.code().map_or(-1, i64::from);

    let output = ExecOutput {
        stdout: output.stdout,
        stderr: output.stderr,
        exit_code,
    };
    ensure_lifecycle_success(stage_name, command_argv, &output, &[])
}

pub(crate) fn lifecycle_command_argv(command: &LifecycleCommand) -> Vec<String> {
    match command {
        LifecycleCommand::Shell(command) => shell_argv(command),
        LifecycleCommand::Args(args) => args.clone(),
        LifecycleCommand::Parallel(_) => Vec::new(),
    }
}

pub(crate) fn hook_command_argv(command: &Command, shell: bool) -> Vec<String> {
    match (command, shell) {
        (Command::Shell(command), true) => shell_argv(command),
        (Command::Shell(command), false) => vec![command.clone()],
        (Command::Args(args), false) => args.clone(),
        (Command::Args(args), true) => shell_argv(&args.join(" ")),
    }
}

fn shell_argv(command: &str) -> Vec<String> {
    vec!["/bin/sh".to_owned(), "-lc".to_owned(), command.to_owned()]
}

pub(in crate::devcontainer::lifecycle) fn ensure_lifecycle_success(
    stage_name: &str,
    command: &[String],
    output: &ExecOutput,
    redactions: &[String],
) -> Result<()> {
    if output.exit_code == 0 {
        return Ok(());
    }

    bail!(
        "Lifecycle stage {stage_name} failed: command `{}` exited with exit code {}. stdout tail: `{}` stderr tail: `{}`",
        redact_values(&command_display(command), redactions),
        output.exit_code,
        redact_values(&output_tail(&output.stdout), redactions),
        redact_values(&output_tail(&output.stderr), redactions),
    );
}

fn command_display(command: &[String]) -> String {
    command.join(" ")
}

fn output_tail(output: &[u8]) -> String {
    const MAX_TAIL_BYTES: usize = 4096;

    let start = output.len().saturating_sub(MAX_TAIL_BYTES);
    String::from_utf8_lossy(&output[start..]).trim().to_owned()
}

fn redact_values(value: &str, redactions: &[String]) -> String {
    redactions
        .iter()
        .filter(|secret| !secret.is_empty())
        .fold(value.to_owned(), |redacted, secret| {
            redacted.replace(secret, "[REDACTED]")
        })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::{config::types::Command, docker::exec::ExecOutput};

    #[test]
    fn lifecycle_commands_map_to_process_argv() {
        assert_eq!(
            lifecycle_command_argv(&LifecycleCommand::Shell("echo ready".to_owned())),
            vec!["/bin/sh", "-lc", "echo ready"]
        );
        assert_eq!(
            lifecycle_command_argv(&LifecycleCommand::Args(vec![
                "bash".to_owned(),
                "-lc".to_owned(),
                "echo ready".to_owned()
            ])),
            vec!["bash", "-lc", "echo ready"]
        );
    }

    #[test]
    fn host_parallel_lifecycle_waits_for_all_siblings_after_failure() {
        let workspace = tempfile::Builder::new()
            .prefix("decune-host-parallel-lifecycle-")
            .tempdir()
            .unwrap();
        let marker = workspace.path().join("slow-finished");
        let command = LifecycleCommand::Parallel(BTreeMap::from([
            (
                "a_fail".to_owned(),
                LifecycleCommand::Shell("exit 7".to_owned()),
            ),
            (
                "z_slow".to_owned(),
                LifecycleCommand::Shell("sleep 1; printf done > slow-finished".to_owned()),
            ),
        ]));

        let error = run_host_lifecycle_command_value(
            workspace.path(),
            LifecycleStage::Initialize,
            &command,
        )
        .unwrap_err();

        let message = format!("{error:#}");
        assert!(message.contains("Lifecycle stage initializeCommand.a_fail failed"));
        assert!(marker.exists());
    }

    #[test]
    fn host_lifecycle_failure_reports_command_exit_code_and_output_tails() {
        let workspace = tempfile::Builder::new()
            .prefix("decune-host-lifecycle-failure-")
            .tempdir()
            .unwrap();
        let command = LifecycleCommand::Shell(
            "printf stdout-sentinel; printf stderr-sentinel >&2; exit 7".to_owned(),
        );

        let error = run_host_lifecycle_command_value(
            workspace.path(),
            LifecycleStage::Initialize,
            &command,
        )
        .unwrap_err();

        let message = format!("{error:#}");
        assert!(message.contains("Lifecycle stage initializeCommand failed"));
        assert!(message.contains(
            "command `/bin/sh -lc printf stdout-sentinel; printf stderr-sentinel >&2; exit 7`"
        ));
        assert!(message.contains("exit code 7"));
        assert!(message.contains("stdout tail: `stdout-sentinel`"));
        assert!(message.contains("stderr tail: `stderr-sentinel`"));
    }

    #[test]
    fn lifecycle_failure_redacts_secret_values() {
        let output = ExecOutput {
            stdout: b"stdout secret-token".to_vec(),
            stderr: b"stderr secret-token".to_vec(),
            exit_code: 7,
        };
        let command = vec![
            "/bin/sh".to_owned(),
            "-lc".to_owned(),
            "printf secret-token".to_owned(),
        ];

        let error = ensure_lifecycle_success(
            "postStartCommand",
            &command,
            &output,
            &["secret-token".to_owned()],
        )
        .unwrap_err();

        let message = format!("{error:#}");
        assert!(!message.contains("secret-token"));
        assert!(message.contains("[REDACTED]"));
        assert!(message.contains("exit code 7"));
    }

    #[test]
    fn hook_commands_respect_shell_flag() {
        assert_eq!(
            hook_command_argv(&Command::Shell("scripts/setup.sh".to_owned()), true),
            vec!["/bin/sh", "-lc", "scripts/setup.sh"]
        );
        assert_eq!(
            hook_command_argv(
                &Command::Args(vec!["bash".to_owned(), "scripts/setup.sh".to_owned()]),
                false,
            ),
            vec!["bash", "scripts/setup.sh"]
        );
        assert_eq!(
            hook_command_argv(
                &Command::Args(vec!["bash".to_owned(), "scripts/setup.sh".to_owned()]),
                true,
            ),
            vec!["/bin/sh", "-lc", "bash scripts/setup.sh"]
        );
    }

    #[test]
    fn same_container_user_matches_group_suffix_and_root_alias() {
        assert!(same_container_user("vscode", "vscode:shared"));
        assert!(same_container_user("root", "0"));
        assert!(!same_container_user("root", "vscode"));
    }
}
