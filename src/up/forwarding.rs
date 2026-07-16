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
    let (published_host_reservations, published_ports) = {
        let state = started.state.borrow();
        (
            published_port_host_reservations(&state.published_ports)?,
            published_port_publish_ports_for_service(
                &state.published_ports,
                primary_compose_service(&started.plan),
            )?,
        )
    };
    let mut targets = plan_forwarding_agent_targets_with_host_reservations(
        &started.plan,
        primary_runtime_dir,
        published_host_reservations,
        published_ports,
    );
    if targets.is_empty() {
        return Ok(Vec::new());
    }

    for target in &mut targets {
        if let Some(service) = target.service.as_deref() {
            let container = resolve_compose_forwarding_container(started, service).await?;
            target.container_name = compose_container_name(container);
        } else {
            target
                .container_name
                .clone_from(&started.outcome.container_name);
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
) -> Vec<ForwardingAgentTarget> {
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
) -> Vec<ForwardingAgentTarget> {
    let auto_forward = AutoForwardConfig::from_config_with_runtime_ports(
        &plan.config,
        host_port_reservations,
        publish_ports,
    );
    if plan.forward_ports.is_empty() && auto_forward.is_none() {
        return Vec::new();
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

    targets
}

pub(in crate::up) fn published_port_host_reservations(
    published_ports: &[PublishedPortRuntimeState],
) -> Result<Vec<HostPortReservation>> {
    let mut reservations = BTreeSet::<(String, u16)>::new();
    for port in published_ports {
        if port.target.protocol != "tcp" {
            continue;
        }
        reservations.insert((
            published_endpoint_reservation_host_ip(port, &port.planned, "planned")?,
            port.planned.host_port,
        ));
        for binding in &port.actual_bindings {
            reservations.insert((binding.host_ip.clone(), binding.host_port));
        }
    }

    Ok(reservations
        .into_iter()
        .map(|(host_ip, host)| HostPortReservation { host_ip, host })
        .collect())
}

#[cfg(test)]
pub(in crate::up) fn published_port_publish_ports(
    published_ports: &[PublishedPortRuntimeState],
) -> Result<Vec<ResolvedPublishPort>> {
    published_port_publish_ports_for_service(published_ports, None)
}

pub(in crate::up) fn published_port_publish_ports_for_service(
    published_ports: &[PublishedPortRuntimeState],
    service: Option<&str>,
) -> Result<Vec<ResolvedPublishPort>> {
    published_ports
        .iter()
        .filter(|port| {
            port.target.protocol == "tcp" && service.is_none_or(|service| port.service == service)
        })
        .map(|port| {
            Ok(ResolvedPublishPort {
                container: port.target.port,
                host: Some(port.planned.host_port),
                host_ip: published_endpoint_publish_host_ip(port, &port.planned, "planned")?,
                protocol: PortProtocol::Tcp,
            })
        })
        .collect()
}

fn published_endpoint_reservation_host_ip(
    port: &PublishedPortRuntimeState,
    endpoint: &crate::state::PublishedPortEndpointState,
    endpoint_name: &str,
) -> Result<String> {
    Ok(match endpoint.ip_kind {
        PublishedPortHostIpKind::Omitted => OMITTED_PUBLISHED_HOST_IP_RESERVATION.to_owned(),
        PublishedPortHostIpKind::Explicit => {
            explicit_published_endpoint_host_ip(port, endpoint, endpoint_name)?
        }
    })
}

fn published_endpoint_publish_host_ip(
    port: &PublishedPortRuntimeState,
    endpoint: &crate::state::PublishedPortEndpointState,
    endpoint_name: &str,
) -> Result<Option<String>> {
    match endpoint.ip_kind {
        PublishedPortHostIpKind::Omitted => Ok(None),
        PublishedPortHostIpKind::Explicit => {
            explicit_published_endpoint_host_ip(port, endpoint, endpoint_name).map(Some)
        }
    }
}

fn explicit_published_endpoint_host_ip(
    port: &PublishedPortRuntimeState,
    endpoint: &crate::state::PublishedPortEndpointState,
    endpoint_name: &str,
) -> Result<String> {
    endpoint
        .ip_value
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .with_context(|| {
            format!(
                "Compose published port runtime state for service `{}` port entry {} has {endpoint_name}.host_ip_kind explicit without {endpoint_name}.host_ip_value",
                port.service, port.port_entry_index
            )
        })
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

    use crate::{
        config::{resolved::ResolvedPublishPort, types::PortProtocol},
        docker::ports::HostPortReservation,
        state::{
            PublishedPortActualBinding, PublishedPortEndpointState, PublishedPortRuntimeState,
            PublishedPortRuntimeType, PublishedPortSource, PublishedPortTarget,
        },
        up::test_support::{compose_config, forward_port_for_service, test_up_plan_with_config},
    };
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
    #[test]
    fn auto_only_forwarding_skips_unsupported_container_architecture() {
        assert_eq!(
        super::decide_forward_agent_start(false, true, Some("riscv64")),
        super::ForwardAgentStartDecision::SkipAutoWithWarning(
            "Automatic port forwarding is disabled because the container architecture is not supported by the port forwarding agent: riscv64".to_owned()
        )
    );
        assert_eq!(
            super::decide_forward_agent_start(true, true, Some("riscv64")),
            super::ForwardAgentStartDecision::Start
        );
        assert_eq!(
            super::decide_forward_agent_start(false, true, Some("x86_64")),
            super::ForwardAgentStartDecision::Start
        );
        assert_eq!(
            super::decide_forward_agent_start(false, true, Some("aarch64")),
            super::ForwardAgentStartDecision::Start
        );
    }
    #[test]
    fn compose_forwarding_targets_split_primary_and_sidecar_services() {
        let mut plan = test_up_plan_with_config(compose_config("app"));
        plan.forward_ports = vec![
            forward_port_for_service(None, 3000),
            forward_port_for_service(Some("app"), 3001),
            forward_port_for_service(Some("db"), 5432),
        ];

        let targets =
            plan_forwarding_agent_targets(&plan, PathBuf::from("/tmp/decune-runtime").as_path());

        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].service.as_deref(), None);
        assert_eq!(
            targets[0]
                .forward_ports
                .iter()
                .map(|port| port.container)
                .collect::<Vec<_>>(),
            vec![3000, 3001]
        );
        assert!(targets[0].auto_forward.is_some());
        assert_eq!(targets[1].service.as_deref(), Some("db"));
        assert_eq!(targets[1].forward_ports[0].container, 5432);
        assert!(targets[1].auto_forward.is_none());
    }
    #[test]
    fn compose_automatic_forwarding_targets_primary_service_only() {
        let mut plan = test_up_plan_with_config(compose_config("app"));
        plan.forward_ports = vec![forward_port_for_service(Some("db"), 5432)];

        let targets =
            plan_forwarding_agent_targets(&plan, PathBuf::from("/tmp/decune-runtime").as_path());

        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].service.as_deref(), None);
        assert!(targets[0].forward_ports.is_empty());
        assert!(targets[0].auto_forward.is_some());
        assert_eq!(targets[1].service.as_deref(), Some("db"));
        assert_eq!(targets[1].forward_ports[0].container, 5432);
        assert!(targets[1].auto_forward.is_none());
    }
    #[test]
    fn compose_published_ports_become_auto_forward_host_reservations() {
        let published_ports = vec![
            PublishedPortRuntimeState {
                source: PublishedPortSource::Compose,
                kind: PublishedPortRuntimeType::Published,
                service: "app".to_owned(),
                port_entry_index: 0,
                target: PublishedPortTarget {
                    port: 3000,
                    protocol: "tcp".to_owned(),
                },
                requested: PublishedPortEndpointState {
                    ip_kind: PublishedPortHostIpKind::Omitted,
                    ip_value: None,
                    host_port: 3000,
                },
                planned: PublishedPortEndpointState {
                    ip_kind: PublishedPortHostIpKind::Omitted,
                    ip_value: None,
                    host_port: 3001,
                },
                actual_bindings: vec![
                    PublishedPortActualBinding {
                        host_ip: "0.0.0.0".to_owned(),
                        host_port: 3001,
                    },
                    PublishedPortActualBinding {
                        host_ip: "::".to_owned(),
                        host_port: 3001,
                    },
                ],
                relocated: true,
            },
            PublishedPortRuntimeState {
                source: PublishedPortSource::Compose,
                kind: PublishedPortRuntimeType::Published,
                service: "app".to_owned(),
                port_entry_index: 1,
                target: PublishedPortTarget {
                    port: 8125,
                    protocol: "udp".to_owned(),
                },
                requested: PublishedPortEndpointState {
                    ip_kind: PublishedPortHostIpKind::Explicit,
                    ip_value: Some("127.0.0.1".to_owned()),
                    host_port: 8125,
                },
                planned: PublishedPortEndpointState {
                    ip_kind: PublishedPortHostIpKind::Explicit,
                    ip_value: Some("127.0.0.1".to_owned()),
                    host_port: 8125,
                },
                actual_bindings: Vec::new(),
                relocated: false,
            },
        ];

        let reservations = published_port_host_reservations(&published_ports).unwrap();

        assert_eq!(
            reservations,
            vec![
                HostPortReservation {
                    host_ip: "0.0.0.0".to_owned(),
                    host: 3001,
                },
                HostPortReservation {
                    host_ip: "::".to_owned(),
                    host: 3001,
                },
            ]
        );
        assert_eq!(
            published_port_publish_ports(&published_ports).unwrap(),
            vec![ResolvedPublishPort {
                container: 3000,
                host: Some(3001),
                host_ip: None,
                protocol: PortProtocol::Tcp,
            }]
        );
    }
    #[test]
    fn compose_auto_forward_publish_exclusions_use_primary_service_only() {
        let published_ports = vec![
            PublishedPortRuntimeState {
                source: PublishedPortSource::Compose,
                kind: PublishedPortRuntimeType::Published,
                service: "db".to_owned(),
                port_entry_index: 0,
                target: PublishedPortTarget {
                    port: 3000,
                    protocol: "tcp".to_owned(),
                },
                requested: PublishedPortEndpointState {
                    ip_kind: PublishedPortHostIpKind::Omitted,
                    ip_value: None,
                    host_port: 3000,
                },
                planned: PublishedPortEndpointState {
                    ip_kind: PublishedPortHostIpKind::Omitted,
                    ip_value: None,
                    host_port: 3000,
                },
                actual_bindings: Vec::new(),
                relocated: false,
            },
            PublishedPortRuntimeState {
                source: PublishedPortSource::Compose,
                kind: PublishedPortRuntimeType::Published,
                service: "app".to_owned(),
                port_entry_index: 0,
                target: PublishedPortTarget {
                    port: 8080,
                    protocol: "tcp".to_owned(),
                },
                requested: PublishedPortEndpointState {
                    ip_kind: PublishedPortHostIpKind::Explicit,
                    ip_value: Some("127.0.0.1".to_owned()),
                    host_port: 18080,
                },
                planned: PublishedPortEndpointState {
                    ip_kind: PublishedPortHostIpKind::Explicit,
                    ip_value: Some("127.0.0.1".to_owned()),
                    host_port: 18080,
                },
                actual_bindings: Vec::new(),
                relocated: false,
            },
        ];

        assert_eq!(
            published_port_host_reservations(&published_ports).unwrap(),
            vec![
                HostPortReservation {
                    host_ip: "0.0.0.0".to_owned(),
                    host: 3000,
                },
                HostPortReservation {
                    host_ip: "127.0.0.1".to_owned(),
                    host: 18080,
                },
            ]
        );
        assert_eq!(
            published_port_publish_ports_for_service(&published_ports, Some("app")).unwrap(),
            vec![ResolvedPublishPort {
                container: 8080,
                host: Some(18080),
                host_ip: Some("127.0.0.1".to_owned()),
                protocol: PortProtocol::Tcp,
            }]
        );
    }
    #[test]
    fn compose_published_port_state_requires_explicit_host_ip_value_for_forwarding() {
        let published_ports = vec![PublishedPortRuntimeState {
            source: PublishedPortSource::Compose,
            kind: PublishedPortRuntimeType::Published,
            service: "app".to_owned(),
            port_entry_index: 0,
            target: PublishedPortTarget {
                port: 3000,
                protocol: "tcp".to_owned(),
            },
            requested: PublishedPortEndpointState {
                ip_kind: PublishedPortHostIpKind::Explicit,
                ip_value: Some("127.0.0.1".to_owned()),
                host_port: 3000,
            },
            planned: PublishedPortEndpointState {
                ip_kind: PublishedPortHostIpKind::Explicit,
                ip_value: None,
                host_port: 3001,
            },
            actual_bindings: Vec::new(),
            relocated: true,
        }];

        let reservation_error = published_port_host_reservations(&published_ports)
            .expect_err("explicit host_ip_kind without host_ip_value must fail");
        assert!(
            reservation_error
                .to_string()
                .contains("planned.host_ip_kind explicit without planned.host_ip_value")
        );

        let publish_error = published_port_publish_ports(&published_ports)
            .expect_err("explicit host_ip_kind without host_ip_value must fail");
        assert!(
            publish_error
                .to_string()
                .contains("planned.host_ip_kind explicit without planned.host_ip_value")
        );
    }
    #[test]
    fn auto_forward_target_receives_compose_published_port_reservations() {
        let plan = test_up_plan_with_config(compose_config("app"));
        let reservations = vec![HostPortReservation {
            host_ip: "0.0.0.0".to_owned(),
            host: 3000,
        }];

        let targets = plan_forwarding_agent_targets_with_host_reservations(
            &plan,
            PathBuf::from("/tmp/decune-runtime").as_path(),
            reservations.clone(),
            vec![ResolvedPublishPort {
                container: 3000,
                host: Some(3000),
                host_ip: None,
                protocol: PortProtocol::Tcp,
            }],
        );

        assert_eq!(targets.len(), 1);
        assert!(targets[0].forward_ports.is_empty());
        assert_eq!(
            targets[0]
                .auto_forward
                .as_ref()
                .unwrap()
                .host_port_reservations(),
            reservations.as_slice()
        );
        assert_eq!(
            targets[0].auto_forward.as_ref().unwrap().publish_ports(),
            [ResolvedPublishPort {
                container: 3000,
                host: Some(3000),
                host_ip: None,
                protocol: PortProtocol::Tcp,
            }]
        );
    }
    #[test]
    fn automatic_compose_published_port_relocation_does_not_enable_auto_forwarding() {
        let mut config = compose_config("app");
        config.ports.auto.enabled = false;
        config.compose.published_ports.automatic_relocation = true;
        let plan = test_up_plan_with_config(config);

        let targets = plan_forwarding_agent_targets_with_host_reservations(
            &plan,
            PathBuf::from("/tmp/decune-runtime").as_path(),
            vec![HostPortReservation {
                host_ip: "0.0.0.0".to_owned(),
                host: 3000,
            }],
            vec![ResolvedPublishPort {
                container: 3000,
                host: Some(3000),
                host_ip: None,
                protocol: PortProtocol::Tcp,
            }],
        );

        assert!(targets.is_empty());
    }
}
