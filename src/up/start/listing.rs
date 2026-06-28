use anyhow::{Context, Result};

use crate::{
    docker::{client::DockerClient, container::ContainerInspect},
    runtime::compose_ports::{
        ComposePortProtocol, ComposePublishedPortEndpoint, ComposePublishedPortHostIpKind,
        ComposePublishedPortReservation,
    },
    up::types::UpContainerSummary,
};

pub(in crate::up) async fn list_workspace_containers(
    client: &DockerClient,
    workspace_id: &str,
) -> Result<Vec<UpContainerSummary>> {
    list_workspace_containers_inner(client, workspace_id).await
}

async fn list_workspace_containers_inner(
    client: &DockerClient,
    workspace_id: &str,
) -> Result<Vec<UpContainerSummary>> {
    client
        .cli()
        .list_workspace_containers(workspace_id)
        .await
        .with_context(|| format!("Failed to list Docker containers for workspace: {workspace_id}"))
}

pub(super) async fn list_compose_primary_containers(
    client: &DockerClient,
    workspace_id: &str,
    project_name: &str,
    service: &str,
) -> Result<Vec<UpContainerSummary>> {
    client
        .cli()
        .list_compose_service_containers(workspace_id, project_name, service)
        .await
        .with_context(|| {
            format!(
                "Failed to list Docker Compose containers for workspace {workspace_id} service `{service}`"
            )
        })
}

pub(super) async fn list_compose_forwarding_service_containers(
    client: &DockerClient,
    workspace_id: &str,
    project_name: &str,
    service: &str,
) -> Result<Vec<UpContainerSummary>> {
    client
        .cli()
        .list_compose_service_containers_by_project(project_name, service)
        .await
        .with_context(|| {
            format!(
                "Failed to list Docker Compose containers for workspace {workspace_id} service `{service}`"
            )
        })
}

pub(super) async fn list_compose_project_containers(
    client: &DockerClient,
    workspace_id: &str,
    project_name: &str,
) -> Result<Vec<UpContainerSummary>> {
    client
        .cli()
        .list_compose_project_containers(workspace_id, project_name)
        .await
        .with_context(|| {
            format!("Failed to list Docker Compose containers for workspace {workspace_id} project `{project_name}`")
        })
}

pub(super) async fn list_existing_compose_project_published_ports(
    client: &DockerClient,
    project_name: &str,
) -> Result<Vec<ComposePublishedPortReservation>> {
    let containers = client
        .cli()
        .list_compose_project_container_inspects_by_project(project_name)
        .await
        .with_context(|| {
            format!("Failed to list Docker Compose containers for project `{project_name}`")
        })?;
    Ok(containers
        .into_iter()
        .flat_map(existing_compose_project_published_ports_from_container)
        .collect())
}

fn existing_compose_project_published_ports_from_container(
    container: ContainerInspect,
) -> Vec<ComposePublishedPortReservation> {
    let running = container
        .state
        .as_ref()
        .and_then(|state| state.running)
        .unwrap_or(false);
    if !running {
        return Vec::new();
    }

    let Some(service) = container
        .config
        .as_ref()
        .and_then(|config| config.labels.as_ref())
        .and_then(|labels| labels.get("com.docker.compose.service"))
        .cloned()
    else {
        return Vec::new();
    };
    let Some(ports) = container
        .network_settings
        .and_then(|settings| settings.ports)
    else {
        return Vec::new();
    };

    let mut reservations = Vec::new();
    for (container_port, bindings) in ports {
        let Some((target_port, protocol)) = parse_container_port_key(&container_port) else {
            continue;
        };
        let Some(bindings) = bindings else {
            continue;
        };
        for binding in bindings {
            let Some(host_port) = binding
                .host_port
                .as_deref()
                .and_then(|host_port| host_port.parse::<u16>().ok())
            else {
                continue;
            };
            let host_ip = binding
                .host_ip
                .filter(|host_ip| !host_ip.is_empty())
                .unwrap_or_else(|| "0.0.0.0".to_owned());
            reservations.push(ComposePublishedPortReservation {
                service: service.clone(),
                target_port,
                protocol: protocol.clone(),
                endpoint: ComposePublishedPortEndpoint {
                    ip_kind: ComposePublishedPortHostIpKind::Explicit,
                    ip_value: Some(host_ip),
                    host_port,
                },
            });
        }
    }
    reservations
}

fn parse_container_port_key(value: &str) -> Option<(u16, ComposePortProtocol)> {
    let (target_port, protocol) = value.split_once('/')?;
    let target_port = target_port.parse::<u16>().ok()?;
    let protocol = match protocol {
        "tcp" => ComposePortProtocol::Tcp,
        "udp" => ComposePortProtocol::Udp,
        other => ComposePortProtocol::Other(other.to_owned()),
    };
    Some((target_port, protocol))
}
