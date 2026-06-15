use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

use crate::{
    config::resolved::ResolvedDevcontainerSource,
    docker::exec::{ExecCommandSpec, exec_capture, exec_detached},
    docker::ports::ResolvedForwardPort,
    host::forward::{
        AutoForwardConfig, ForwardAgentStatus, ForwardSession, forward_agent_command_at,
        forward_agent_socket_target, new_forward_agent_secret, service_forward_runtime_dir,
        start_forward_session_with_auto, wait_for_forward_agent_with_status,
    },
    runtime::compose_cli::{ComposeIntrospector, ComposePsContainer},
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
) -> Result<Option<ForwardingSession>> {
    let mut targets =
        resolve_forwarding_agent_targets(started, started.workspace.paths().runtime_dir()).await?;
    if targets.is_empty() {
        return Ok(None);
    }
    filter_unsupported_auto_only_targets(started, &mut targets).await;
    if targets.is_empty() {
        return Ok(None);
    }

    let mut sessions = Vec::new();
    for target in targets {
        let secret = new_forward_agent_secret()?;
        exec_detached(
            &started.client,
            &target.container_name,
            &forward_agent_command_at(&target.forward_ports, &secret, &target.socket_target),
        )
        .await
        .with_context(|| {
            format!(
                "Failed to start port forwarding agent in container: {}",
                target.container_name
            )
        })?;
        let agent_socket_path = wait_for_forward_agent_with_status(&target.runtime_dir, || async {
            Ok(ForwardAgentStatus::Running)
        })
        .await
        .with_context(|| {
            format!(
                "Failed to wait for port forwarding agent in container: {}",
                target.container_name
            )
        })?;
        let session = start_forward_session_with_auto(
            &target.forward_ports,
            target.auto_forward,
            agent_socket_path,
            secret,
        )
        .await
        .context("Failed to start port forwarding listeners")?;
        sessions.push(session);
    }

    Ok(Some(ForwardingSession { sessions }))
}

pub(in crate::up) async fn stop_forwarding(forwarding: Option<ForwardingSession>) {
    if let Some(session) = forwarding {
        session.stop().await;
    }
}

pub(in crate::up) struct ForwardingSession {
    sessions: Vec<ForwardSession>,
}

impl ForwardingSession {
    pub(crate) async fn stop(self) {
        for session in self.sessions {
            session.stop().await;
        }
    }
}

#[derive(Debug)]
pub(in crate::up) struct ForwardingAgentTarget {
    pub(in crate::up) service: Option<String>,
    pub(in crate::up) container_name: String,
    pub(in crate::up) runtime_dir: PathBuf,
    #[cfg(test)]
    pub(in crate::up) socket_name: String,
    socket_target: String,
    pub(in crate::up) forward_ports: Vec<ResolvedForwardPort>,
    pub(in crate::up) auto_forward: Option<AutoForwardConfig>,
}

async fn resolve_forwarding_agent_targets(
    started: &StartedUpContainer,
    primary_runtime_dir: &Path,
) -> Result<Vec<ForwardingAgentTarget>> {
    let mut targets = plan_forwarding_agent_targets(&started.plan, primary_runtime_dir)?;
    if targets.is_empty() {
        return Ok(Vec::new());
    }

    for target in &mut targets {
        if let Some(service) = target.service.as_deref() {
            let container = resolve_compose_forwarding_container(started, service).await?;
            target.container_name = compose_container_name(container);
        } else {
            target.container_name = started.outcome.container_name.clone();
        }
    }

    Ok(targets)
}

async fn filter_unsupported_auto_only_targets(
    started: &StartedUpContainer,
    targets: &mut Vec<ForwardingAgentTarget>,
) {
    let mut kept = Vec::with_capacity(targets.len());
    for target in targets.drain(..) {
        let auto_only = target.forward_ports.is_empty() && target.auto_forward.is_some();
        if !auto_only {
            kept.push(target);
            continue;
        }
        let arch = match detect_container_arch_for_forward_agent(
            &started.client,
            &target.container_name,
        )
        .await
        {
            Ok(arch) => arch,
            Err(error) => {
                ui::warn(&format!(
                    "Automatic port forwarding is disabled because the container architecture could not be detected: {error:#}"
                ));
                continue;
            }
        };
        match decide_forward_agent_start(false, true, arch.as_deref()) {
            ForwardAgentStartDecision::Start => kept.push(target),
            ForwardAgentStartDecision::SkipAutoWithWarning(warning) => ui::warn(&warning),
        }
    }
    *targets = kept;
}

pub(in crate::up) fn plan_forwarding_agent_targets(
    plan: &UpPlan,
    primary_runtime_dir: &Path,
) -> Result<Vec<ForwardingAgentTarget>> {
    let auto_forward = AutoForwardConfig::from_config(&plan.config);
    if plan.forward_ports.is_empty() && auto_forward.is_none() {
        return Ok(Vec::new());
    }

    let primary_service = primary_compose_service(plan);
    let mut primary_ports = Vec::new();
    let mut sidecar_ports = BTreeMap::<String, Vec<ResolvedForwardPort>>::new();
    for port in &plan.forward_ports {
        match forwarding_target_service(port.service.as_deref(), primary_service) {
            Some(service) => sidecar_ports
                .entry(service.to_owned())
                .or_default()
                .push(port.clone()),
            None => primary_ports.push(port.clone()),
        }
    }

    let mut targets = Vec::new();
    if !primary_ports.is_empty() || auto_forward.is_some() {
        targets.push(ForwardingAgentTarget {
            service: None,
            container_name: String::new(),
            runtime_dir: primary_runtime_dir.to_path_buf(),
            #[cfg(test)]
            socket_name: crate::host::forward::forward_agent_socket_name(None),
            socket_target: forward_agent_socket_target(None),
            forward_ports: primary_ports,
            auto_forward,
        });
    }
    for (service, forward_ports) in sidecar_ports {
        targets.push(ForwardingAgentTarget {
            service: Some(service.clone()),
            container_name: String::new(),
            runtime_dir: service_forward_runtime_dir(primary_runtime_dir, &service),
            #[cfg(test)]
            socket_name: crate::host::forward::forward_agent_socket_name(Some(&service)),
            socket_target: forward_agent_socket_target(Some(&service)),
            forward_ports,
            auto_forward: None,
        });
    }

    Ok(targets)
}

async fn resolve_compose_forwarding_container(
    started: &StartedUpContainer,
    service: &str,
) -> Result<ComposePsContainer> {
    let Some(compose_project) = &started.plan.compose_project else {
        anyhow::bail!(
            "Service-qualified port forwarding requires Docker Compose project state: {service}"
        );
    };
    ComposeIntrospector::default()
        .resolve_service_container(
            &compose_project.command_plan_with_generated_override(),
            service,
        )
        .await
}

fn compose_container_name(container: ComposePsContainer) -> String {
    container.name.unwrap_or(container.id)
}

fn primary_compose_service(plan: &UpPlan) -> Option<&str> {
    match &plan.config.devcontainer.source {
        Some(ResolvedDevcontainerSource::Compose(compose)) => Some(&compose.service),
        _ => None,
    }
}

fn forwarding_target_service<'a>(
    service: Option<&'a str>,
    primary_service: Option<&str>,
) -> Option<&'a str> {
    match (service, primary_service) {
        (Some(service), Some(primary)) if service != primary => Some(service),
        _ => None,
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
