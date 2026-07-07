use anyhow::{Context, Result};

use crate::{
    docker::{client::DockerClient, container::ContainerInspect},
    runtime::compose_ports::{
        ComposePortProtocol, ComposePublishedPortEndpoint, ComposePublishedPortHostIp,
        ComposePublishedPortReservation, ComposePublishedPortReservationSource,
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
    let source = if running {
        ComposePublishedPortReservationSource::RunningContainer
    } else {
        ComposePublishedPortReservationSource::StoppedContainer
    };

    let Some(service) = container
        .config
        .as_ref()
        .and_then(|config| config.labels.as_ref())
        .and_then(|labels| labels.get("com.docker.compose.service"))
        .cloned()
    else {
        return Vec::new();
    };
    let ports = if running {
        container
            .network_settings
            .and_then(|settings| settings.ports)
    } else {
        container
            .host_config
            .and_then(|host_config| host_config.port_bindings)
    };
    let Some(ports) = ports else {
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
                    host_ip: ComposePublishedPortHostIp::Explicit(host_ip),
                    host_port,
                },
                source,
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

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};

    use super::*;
    use crate::docker::container::{
        ContainerInspectConfig, ContainerInspectHostConfig, ContainerNetworkSettings,
        ContainerPortBinding, ContainerState,
    };

    #[test]
    fn compose_project_published_ports_include_stopped_host_config_bindings() {
        let reservations = existing_compose_project_published_ports_from_container(
            container_with_ports(false, None, Some(port_map("80/tcp", "127.0.0.1", "18300"))),
        );

        assert_eq!(reservations.len(), 1);
        assert_eq!(reservations[0].service, "app");
        assert_eq!(reservations[0].target_port, 80);
        assert_eq!(reservations[0].protocol, ComposePortProtocol::Tcp);
        assert_eq!(
            reservations[0].endpoint,
            ComposePublishedPortEndpoint {
                host_ip: ComposePublishedPortHostIp::Explicit("127.0.0.1".to_owned()),
                host_port: 18300,
            }
        );
        assert_eq!(
            reservations[0].source,
            ComposePublishedPortReservationSource::StoppedContainer
        );
    }

    #[test]
    fn compose_project_published_ports_mark_running_network_bindings() {
        let reservations = existing_compose_project_published_ports_from_container(
            container_with_ports(true, Some(port_map("80/tcp", "0.0.0.0", "18300")), None),
        );

        assert_eq!(reservations.len(), 1);
        assert_eq!(
            reservations[0].source,
            ComposePublishedPortReservationSource::RunningContainer
        );
    }

    fn container_with_ports(
        running: bool,
        network_ports: Option<HashMap<String, Option<Vec<ContainerPortBinding>>>>,
        host_port_bindings: Option<HashMap<String, Option<Vec<ContainerPortBinding>>>>,
    ) -> ContainerInspect {
        ContainerInspect {
            config: Some(ContainerInspectConfig {
                labels: Some(BTreeMap::from([(
                    "com.docker.compose.service".to_owned(),
                    "app".to_owned(),
                )])),
                ..ContainerInspectConfig::default()
            }),
            state: Some(ContainerState {
                running: Some(running),
                ..ContainerState::default()
            }),
            host_config: Some(ContainerInspectHostConfig {
                port_bindings: host_port_bindings,
            }),
            network_settings: Some(ContainerNetworkSettings {
                ports: network_ports,
            }),
            ..ContainerInspect::default()
        }
    }

    fn port_map(
        container_port: &str,
        host_ip: &str,
        host_port: &str,
    ) -> HashMap<String, Option<Vec<ContainerPortBinding>>> {
        HashMap::from([(
            container_port.to_owned(),
            Some(vec![ContainerPortBinding {
                host_ip: Some(host_ip.to_owned()),
                host_port: Some(host_port.to_owned()),
            }]),
        )])
    }
}
