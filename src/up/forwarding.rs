use std::{collections::BTreeMap, path::Path};

use anyhow::{Context, Result};

use crate::{
    docker::exec::{ExecCommandSpec, exec_capture, exec_detached, inspect_exec},
    host::forward::{
        AutoForwardConfig, ForwardAgentStatus, ForwardSession, forward_agent_command,
        new_forward_agent_secret, start_forward_session_with_auto,
        wait_for_forward_agent_with_status,
    },
    ui,
    up::{start::StartedUpContainer, types::UpPlan},
};

pub(in crate::up) fn warn_about_detached_forwarding(plan: &UpPlan) {
    if plan.ignored_detached_forwarding {
        ui::warn(
            "Port forwarding is ignored in detached mode; use appPort for detached publishing",
        );
    }
}

pub(in crate::up) async fn start_forwarding_for_up(
    started: &StartedUpContainer,
) -> Result<Option<ForwardSession>> {
    let auto_forward = AutoForwardConfig::from_config(&started.plan.config);
    if started.plan.forward_ports.is_empty() && auto_forward.is_none() {
        return Ok(None);
    }
    if started.plan.forward_ports.is_empty() && auto_forward.is_some() {
        let arch = match detect_container_arch_for_forward_agent(
            &started.client,
            &started.outcome.container_name,
        )
        .await
        {
            Ok(arch) => arch,
            Err(error) => {
                ui::warn(&format!(
                    "Automatic port forwarding is disabled because the container architecture could not be detected: {error:#}"
                ));
                return Ok(None);
            }
        };
        if let ForwardAgentStartDecision::SkipAutoWithWarning(warning) =
            decide_forward_agent_start(false, true, arch.as_deref())
        {
            ui::warn(&warning);
            return Ok(None);
        }
        if let Some(arch) = arch.as_deref()
            && !forward_agent_tool_exists_for_arch(started.workspace.paths().runtime_dir(), arch)
        {
            ui::warn(&format!(
                "Automatic port forwarding is disabled because the port forwarding agent artifact is not available for the container architecture: {arch}"
            ));
            return Ok(None);
        }
    }

    let secret = new_forward_agent_secret()?;
    let agent_exec_id = exec_detached(
        &started.client,
        &started.outcome.container_name,
        &forward_agent_command(&started.plan.forward_ports, &secret),
    )
    .await
    .with_context(|| {
        format!(
            "Failed to start port forwarding agent in container: {}",
            started.outcome.container_name
        )
    })?;
    let agent_socket_path =
        wait_for_forward_agent_with_status(started.workspace.paths().runtime_dir(), || async {
            let inspect = inspect_exec(
                &started.client,
                &agent_exec_id,
                &started.outcome.container_name,
            )
            .await?;
            Ok(
                if inspect.running == Some(false) || inspect.exit_code.is_some() {
                    ForwardAgentStatus::Exited {
                        exit_code: inspect.exit_code,
                    }
                } else {
                    ForwardAgentStatus::Running
                },
            )
        })
        .await
        .with_context(|| {
            format!(
                "Failed to wait for port forwarding agent in container: {}",
                started.outcome.container_name
            )
        })?;
    let session = start_forward_session_with_auto(
        &started.plan.forward_ports,
        auto_forward,
        agent_socket_path,
        secret,
    )
    .await
    .context("Failed to start port forwarding listeners")?;

    Ok(Some(session))
}

pub(in crate::up) async fn stop_forwarding(forwarding: Option<ForwardSession>) {
    if let Some(session) = forwarding {
        session.stop().await;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::up) enum ForwardAgentStartDecision {
    Start,
    SkipAutoWithWarning(String),
}

pub(in crate::up) fn decide_forward_agent_start(
    has_manual_forward_ports: bool,
    auto_forward_enabled: bool,
    container_arch: Option<&str>,
) -> ForwardAgentStartDecision {
    if has_manual_forward_ports || !auto_forward_enabled {
        return ForwardAgentStartDecision::Start;
    }

    match container_arch.map(str::trim) {
        Some("x86_64" | "amd64" | "aarch64" | "arm64") => ForwardAgentStartDecision::Start,
        Some(arch) if !arch.is_empty() => ForwardAgentStartDecision::SkipAutoWithWarning(format!(
            "Automatic port forwarding is disabled because the container architecture is not supported by the port forwarding agent: {arch}"
        )),
        _ => ForwardAgentStartDecision::SkipAutoWithWarning(
            "Automatic port forwarding is disabled because the container architecture could not be detected".to_owned(),
        ),
    }
}

fn forward_agent_tool_exists_for_arch(runtime_dir: &Path, arch: &str) -> bool {
    let file_name = match arch.trim() {
        "x86_64" | "amd64" => "decune-forward-agent-linux-amd64",
        "aarch64" | "arm64" => "decune-forward-agent-linux-arm64",
        _ => return false,
    };
    runtime_dir.join(file_name).is_file()
}

async fn detect_container_arch_for_forward_agent(
    client: &crate::docker::client::DockerClient,
    container_name: &str,
) -> Result<Option<String>> {
    let output = exec_capture(
        client,
        container_name,
        &ExecCommandSpec {
            command: vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                "uname -m 2>/dev/null || true".to_owned(),
            ],
            user: Some("0".to_owned()),
            working_dir: None,
            env: BTreeMap::new(),
            tty: false,
        },
    )
    .await?;
    let arch = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok((!arch.is_empty()).then_some(arch))
}
