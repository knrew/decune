use std::net::IpAddr;

use anyhow::{Context, Result, anyhow};

use crate::{
    docker::{
        client::DockerClient,
        container::ContainerInspect,
        ports::HostPortReservation,
        resource::{COMPOSE_PROJECT_LABEL, COMPOSE_SERVICE_LABEL},
    },
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

pub(super) async fn list_external_running_container_published_ports(
    client: &DockerClient,
    current_project_name: &str,
) -> Result<Vec<HostPortReservation>> {
    let containers = client
        .cli()
        .list_running_container_inspects()
        .await
        .context("Failed to list running Docker containers for published port reservations")?;
    containers
        .into_iter()
        .filter(|container| !container_belongs_to_compose_project(container, current_project_name))
        .map(external_running_container_published_ports)
        .collect::<Result<Vec<_>>>()
        .map(|reservations| reservations.into_iter().flatten().collect())
}

fn container_belongs_to_compose_project(container: &ContainerInspect, project_name: &str) -> bool {
    container
        .config
        .as_ref()
        .and_then(|config| config.labels.as_ref())
        .and_then(|labels| labels.get(COMPOSE_PROJECT_LABEL))
        .is_some_and(|project| project == project_name)
}

fn external_running_container_published_ports(
    container: ContainerInspect,
) -> Result<Vec<HostPortReservation>> {
    let container_name = container
        .name
        .as_deref()
        .unwrap_or("<unknown container>")
        .trim_start_matches('/')
        .to_owned();
    let Some(ports) = container
        .network_settings
        .and_then(|settings| settings.ports)
    else {
        return Ok(Vec::new());
    };
    let mut reservations = Vec::new();
    for (container_port, bindings) in ports {
        let (target_port, protocol) = container_port.split_once('/').ok_or_else(|| {
            anyhow!(
                "Failed to parse Docker published port key `{container_port}` for running container `{container_name}`"
            )
        })?;
        target_port.parse::<u16>().with_context(|| {
            format!(
                "Failed to parse Docker published target port `{target_port}` for running container `{container_name}`"
            )
        })?;
        if protocol != "tcp" {
            continue;
        }
        let Some(bindings) = bindings else {
            continue;
        };
        for binding in bindings {
            let Some(host_port) = binding.host_port else {
                continue;
            };
            let host = host_port.parse::<u16>().with_context(|| {
                format!(
                    "Failed to parse Docker published host port `{host_port}` for running container `{container_name}`"
                )
            })?;
            if host == 0 {
                return Err(anyhow!(
                    "Failed to parse Docker published host port `0` for running container `{container_name}`: port must be greater than zero"
                ));
            }
            let host_ip = binding
                .host_ip
                .filter(|host_ip| !host_ip.is_empty())
                .unwrap_or_else(|| "0.0.0.0".to_owned());
            host_ip.parse::<IpAddr>().with_context(|| {
                format!(
                    "Failed to parse Docker published host IP `{host_ip}` for running container `{container_name}`"
                )
            })?;
            reservations.push(HostPortReservation { host_ip, host });
        }
    }
    Ok(reservations)
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
        .and_then(|labels| labels.get(COMPOSE_SERVICE_LABEL))
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

    #[test]
    fn external_running_ports_use_actual_tcp_bindings_and_skip_udp() {
        let mut ports = port_map("80/tcp", "", "18080");
        ports.extend(port_map("53/udp", "0.0.0.0", "1053"));
        ports.extend(port_map("443/tcp", "::1", "18443"));
        let mut container = container_with_ports(true, Some(ports), None);
        container.name = Some("/external-app".to_owned());

        let mut reservations = external_running_container_published_ports(container).unwrap();
        reservations.sort_by_key(|reservation| reservation.host);

        assert_eq!(
            reservations,
            vec![
                HostPortReservation {
                    host_ip: "0.0.0.0".to_owned(),
                    host: 18080,
                },
                HostPortReservation {
                    host_ip: "::1".to_owned(),
                    host: 18443,
                },
            ]
        );
    }

    #[test]
    fn external_running_ports_reject_malformed_actual_binding_with_context() {
        let mut container = container_with_ports(
            true,
            Some(port_map("80/tcp", "localhost", "not-a-port")),
            None,
        );
        container.name = Some("/broken-app".to_owned());

        let error = external_running_container_published_ports(container).unwrap_err();
        let message = error.to_string();

        assert!(message.contains("not-a-port"));
        assert!(message.contains("broken-app"));
    }

    #[test]
    fn current_compose_project_is_excluded_from_external_reservations() {
        let mut current = container_with_ports(true, None, None);
        current
            .config
            .as_mut()
            .and_then(|config| config.labels.as_mut())
            .unwrap()
            .insert(COMPOSE_PROJECT_LABEL.to_owned(), "current".to_owned());

        assert!(container_belongs_to_compose_project(&current, "current"));
        assert!(!container_belongs_to_compose_project(&current, "other"));
    }

    fn container_with_ports(
        running: bool,
        network_ports: Option<HashMap<String, Option<Vec<ContainerPortBinding>>>>,
        host_port_bindings: Option<HashMap<String, Option<Vec<ContainerPortBinding>>>>,
    ) -> ContainerInspect {
        ContainerInspect {
            config: Some(ContainerInspectConfig {
                labels: Some(BTreeMap::from([(
                    COMPOSE_SERVICE_LABEL.to_owned(),
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
