use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

use crate::{
    config::{
        resolved::{ResolvedDevcontainerSource, ResolvedPublishPort},
        types::PortProtocol,
    },
    docker::exec::{ExecCommandSpec, exec_capture, exec_detached},
    docker::ports::{HostPortReservation, ResolvedForwardPort},
    host::forward::{
        AutoForwardConfig, ForwardAgentStatus, ForwardSession, ForwardStatusRegistry,
        ForwardStatusServer, forward_agent_command_at, forward_agent_session_socket_name,
        forward_agent_socket_target_from_name, forward_status_dir, new_forward_agent_secret,
        new_forward_agent_socket_id, service_forward_runtime_dir, start_forward_session_with_auto,
        start_forward_status_server, wait_for_forward_agent_with_status,
    },
    runtime::compose_cli::{ComposeIntrospector, ComposePsContainer},
    state::{PublishedPortHostIpKind, PublishedPortRuntimeState},
    ui,
    up::{start::StartedUpContainer, types::UpPlan},
};

const OMITTED_PUBLISHED_HOST_IP_RESERVATION: &str = "0.0.0.0";

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

    let status_server =
        start_forward_status_server(forward_status_dir(started.workspace.paths().runtime_dir()))
            .await?;
    let status_registry = status_server.registry();
    let mut sessions = Vec::new();
    for target in targets {
        match start_forwarding_target(started, target, status_registry.clone()).await {
            Ok(session) => sessions.push(session),
            Err(error) => {
                stop_started_forward_sessions(sessions).await;
                return Err(error);
            }
        }
    }

    Ok(Some(ForwardingSession {
        sessions,
        status_server: Some(status_server),
    }))
}

async fn start_forwarding_target(
    started: &StartedUpContainer,
    target: ForwardingAgentTarget,
    status_registry: ForwardStatusRegistry,
) -> Result<ForwardSession> {
    let secret = new_forward_agent_secret()?;
    let socket_id = new_forward_agent_socket_id()?;
    let socket_name = forward_agent_session_socket_name(target.service.as_deref(), &socket_id);
    let socket_target = forward_agent_socket_target_from_name(&socket_name);
    exec_detached(
        &started.client,
        &target.container_name,
        &forward_agent_command_at(&target.forward_ports, &secret, &socket_target),
    )
    .await
    .with_context(|| {
        format!(
            "Failed to start port forwarding agent in container: {}",
            target.container_name
        )
    })?;
    let agent_socket_path =
        wait_for_forward_agent_with_status(&target.runtime_dir, &socket_name, || async {
            Ok(ForwardAgentStatus::Running)
        })
        .await
        .with_context(|| {
            format!(
                "Failed to wait for port forwarding agent in container: {}",
                target.container_name
            )
        })?;
    start_forward_session_with_auto(
        &target.forward_ports,
        target.auto_forward,
        agent_socket_path,
        secret,
        Some(status_registry),
    )
    .await
    .context("Failed to start port forwarding listeners")
}

async fn stop_started_forward_sessions(sessions: Vec<ForwardSession>) {
    for session in sessions {
        session.stop().await;
    }
}

pub(in crate::up) async fn stop_forwarding(forwarding: Option<ForwardingSession>) {
    if let Some(session) = forwarding {
        session.stop().await;
    }
}

pub(in crate::up) struct ForwardingSession {
    sessions: Vec<ForwardSession>,
    status_server: Option<ForwardStatusServer>,
}

impl ForwardingSession {
    pub(crate) async fn stop(mut self) {
        for session in self.sessions.drain(..) {
            session.stop().await;
        }
        if let Some(status_server) = self.status_server.take() {
            status_server.stop().await;
        }
    }
}

#[derive(Debug)]
pub(in crate::up) struct ForwardingAgentTarget {
    pub(in crate::up) service: Option<String>,
    pub(in crate::up) container_name: String,
    pub(in crate::up) runtime_dir: PathBuf,
    pub(in crate::up) forward_ports: Vec<ResolvedForwardPort>,
    pub(in crate::up) auto_forward: Option<AutoForwardConfig>,
}

async fn resolve_forwarding_agent_targets(
    started: &StartedUpContainer,
    primary_runtime_dir: &Path,
) -> Result<Vec<ForwardingAgentTarget>> {
    let published_host_reservations =
        published_port_host_reservations(&started.state.borrow().published_ports);
    let published_ports = published_port_publish_ports_for_service(
        &started.state.borrow().published_ports,
        primary_compose_service(&started.plan),
    );
    let mut targets = plan_forwarding_agent_targets_with_host_reservations(
        &started.plan,
        primary_runtime_dir,
        published_host_reservations,
        published_ports,
    )?;
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

#[cfg(test)]
pub(in crate::up) fn plan_forwarding_agent_targets(
    plan: &UpPlan,
    primary_runtime_dir: &Path,
) -> Result<Vec<ForwardingAgentTarget>> {
    plan_forwarding_agent_targets_with_host_reservations(
        plan,
        primary_runtime_dir,
        Vec::new(),
        Vec::new(),
    )
}

pub(in crate::up) fn plan_forwarding_agent_targets_with_host_reservations(
    plan: &UpPlan,
    primary_runtime_dir: &Path,
    host_port_reservations: Vec<HostPortReservation>,
    publish_ports: Vec<ResolvedPublishPort>,
) -> Result<Vec<ForwardingAgentTarget>> {
    let auto_forward = AutoForwardConfig::from_config_with_runtime_ports(
        &plan.config,
        host_port_reservations,
        publish_ports,
    );
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
            forward_ports: primary_ports,
            auto_forward,
        });
    }
    for (service, forward_ports) in sidecar_ports {
        targets.push(ForwardingAgentTarget {
            service: Some(service.clone()),
            container_name: String::new(),
            runtime_dir: service_forward_runtime_dir(primary_runtime_dir, &service),
            forward_ports,
            auto_forward: None,
        });
    }

    Ok(targets)
}

pub(in crate::up) fn published_port_host_reservations(
    published_ports: &[PublishedPortRuntimeState],
) -> Vec<HostPortReservation> {
    let mut reservations = BTreeSet::<(String, u16)>::new();
    for port in published_ports {
        if port.target.protocol != "tcp" {
            continue;
        }
        reservations.insert((
            published_endpoint_reservation_host_ip(
                port.planned.host_ip_kind,
                port.planned.host_ip_value.as_deref(),
            ),
            port.planned.host_port,
        ));
        for binding in &port.actual_bindings {
            reservations.insert((binding.host_ip.clone(), binding.host_port));
        }
    }

    reservations
        .into_iter()
        .map(|(host_ip, host)| HostPortReservation { host_ip, host })
        .collect()
}

#[cfg(test)]
pub(in crate::up) fn published_port_publish_ports(
    published_ports: &[PublishedPortRuntimeState],
) -> Vec<ResolvedPublishPort> {
    published_port_publish_ports_for_service(published_ports, None)
}

pub(in crate::up) fn published_port_publish_ports_for_service(
    published_ports: &[PublishedPortRuntimeState],
    service: Option<&str>,
) -> Vec<ResolvedPublishPort> {
    published_ports
        .iter()
        .filter(|port| {
            port.target.protocol == "tcp"
                && match service {
                    Some(service) => port.service == service,
                    None => true,
                }
        })
        .map(|port| ResolvedPublishPort {
            container: port.target.port,
            host: Some(port.planned.host_port),
            host_ip: match port.planned.host_ip_kind {
                PublishedPortHostIpKind::Omitted => None,
                PublishedPortHostIpKind::Explicit => port.planned.host_ip_value.clone(),
            },
            protocol: PortProtocol::Tcp,
        })
        .collect()
}

fn published_endpoint_reservation_host_ip(
    kind: PublishedPortHostIpKind,
    value: Option<&str>,
) -> String {
    match kind {
        PublishedPortHostIpKind::Omitted => OMITTED_PUBLISHED_HOST_IP_RESERVATION.to_owned(),
        PublishedPortHostIpKind::Explicit => value.unwrap_or_default().to_owned(),
    }
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
            redactions: Vec::new(),
            tty: false,
        },
    )
    .await?;
    let arch = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok((!arch.is_empty()).then_some(arch))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tempfile::TempDir;
    use tokio::{
        io::AsyncReadExt,
        net::UnixListener,
        task::JoinHandle,
        time::{Duration, timeout},
    };

    use super::*;

    #[test]
    fn cleanup_started_forward_sessions_sends_agent_shutdown_to_all_sessions() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let temp = TempDir::new().unwrap();

        runtime.block_on(async {
            let first_socket = temp.path().join("first.sock");
            let second_socket = temp.path().join("second.sock");
            let first_shutdown = capture_shutdown_request(&first_socket);
            let second_shutdown = capture_shutdown_request(&second_socket);
            let sessions = vec![
                ForwardSession::for_test(first_socket, "first-secret"),
                ForwardSession::for_test(second_socket, "second-secret"),
            ];

            stop_started_forward_sessions(sessions).await;

            let first_request = timeout(Duration::from_secs(1), first_shutdown)
                .await
                .unwrap()
                .unwrap();
            let second_request = timeout(Duration::from_secs(1), second_shutdown)
                .await
                .unwrap()
                .unwrap();
            assert!(first_request.contains(r#""shutdown":true"#));
            assert!(first_request.contains(r#""secret":"first-secret""#));
            assert!(second_request.contains(r#""shutdown":true"#));
            assert!(second_request.contains(r#""secret":"second-secret""#));
        });
    }

    fn capture_shutdown_request(socket_path: &Path) -> JoinHandle<String> {
        let listener = UnixListener::bind(socket_path).unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            stream.read_to_end(&mut bytes).await.unwrap();
            String::from_utf8(bytes).unwrap()
        })
    }
}
