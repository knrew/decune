use std::collections::{BTreeMap, BTreeSet};

use crate::{
    docker::{
        container::{ContainerInspect, ContainerPortBinding},
        resource::{
            COMPOSE_PROJECT_LABEL, COMPOSE_SERVICE_LABEL, compose_project_name_from_labels,
            managed_workspace_id_from_container, workspace_path_from_labels,
        },
    },
    state::{PublishedPortEndpointState, PublishedPortRuntimeState},
    text::non_empty_trimmed,
};

use super::{
    context::WorkspacePortContext,
    types::{
        PortInventoryActualBinding, PortInventoryEndpoint, PortInventoryEntry, PortInventoryTarget,
        PortUsageType,
    },
};

#[derive(Debug, Clone)]
pub(super) struct PublishedContainerInspect {
    pub(super) container: ContainerInspect,
    pub(super) context: Option<WorkspacePortContext>,
}

#[cfg(test)]
fn published_ports_from_containers(
    containers: Vec<ContainerInspect>,
    context: Option<&WorkspacePortContext>,
    include_workspace: bool,
) -> Vec<PortInventoryEntry> {
    let entries = containers
        .into_iter()
        .map(|container| PublishedContainerInspect {
            container,
            context: context.cloned(),
        })
        .collect();
    published_ports_from_container_entries(entries, include_workspace)
}

pub(super) fn published_ports_from_container_entries(
    containers: Vec<PublishedContainerInspect>,
    include_workspace: bool,
) -> Vec<PortInventoryEntry> {
    containers
        .into_iter()
        .filter(|entry| container_is_running(&entry.container))
        .flat_map(|entry| {
            let PublishedContainerInspect { container, context } = entry;
            published_ports_from_container(container, context.as_ref(), include_workspace)
        })
        .collect()
}

pub(super) fn dedupe_published_containers(
    containers: Vec<PublishedContainerInspect>,
) -> Vec<PublishedContainerInspect> {
    let mut positions = BTreeMap::<String, usize>::new();
    let mut deduped = Vec::<PublishedContainerInspect>::new();

    for entry in containers {
        let id = entry
            .container
            .id
            .as_deref()
            .filter(|id| !id.trim().is_empty())
            .map(str::to_owned);
        let Some(id) = id else {
            deduped.push(entry);
            continue;
        };

        if let Some(index) = positions.get(&id).copied() {
            if deduped[index].context.is_none() || entry.context.is_some() {
                deduped[index] = entry;
            }
        } else {
            positions.insert(id, deduped.len());
            deduped.push(entry);
        }
    }

    deduped
}

fn published_ports_from_container(
    container: ContainerInspect,
    context: Option<&WorkspacePortContext>,
    include_workspace: bool,
) -> Vec<PortInventoryEntry> {
    let labels = container
        .config
        .as_ref()
        .and_then(|config| config.labels.as_ref());
    let workspace_id = context
        .map(|context| context.workspace_id.clone())
        .or_else(|| container_workspace_id(&container));
    let workspace_path = context
        .and_then(|context| context.workspace_path.clone())
        .or_else(|| labels.and_then(workspace_path_from_labels));
    let compose_service = labels.and_then(compose_service_from_labels);
    let source = if compose_service.is_some() {
        "compose"
    } else {
        "appPort"
    };
    let service = compose_service.cloned();
    let ports = container
        .network_settings
        .and_then(|settings| settings.ports)
        .unwrap_or_default();

    let mut entries = Vec::new();
    let mut emitted_runtime_states = BTreeSet::<(String, usize)>::new();
    for (target, bindings) in ports {
        let Some((container_port, protocol)) = parse_docker_port_key(&target) else {
            continue;
        };
        let Some(bindings) = bindings else {
            continue;
        };
        for binding in &bindings {
            let runtime_state = context.and_then(|context| {
                runtime_state_for_published_binding(
                    &context.published_ports,
                    service.as_deref(),
                    container_port,
                    &protocol,
                    binding,
                )
            });
            if let Some(state) = runtime_state
                && let Some(entry) = compose_published_port_runtime_entry(
                    state,
                    actual_bindings_for_runtime_state_from_bindings(state, binding, &bindings),
                    PublishedPortRuntimeEntryInput {
                        workspace: include_workspace.then(|| workspace_path.clone()).flatten(),
                        workspace_id: include_workspace.then(|| workspace_id.clone()).flatten(),
                        service: service.clone(),
                        source,
                    },
                )
            {
                if emitted_runtime_states.insert((state.service.clone(), state.port_entry_index)) {
                    entries.push(entry);
                }
                continue;
            }
            if let Some(entry) = published_port_binding_entry(PublishedPortBindingEntryInput {
                binding: binding.clone(),
                workspace: include_workspace.then(|| workspace_path.clone()).flatten(),
                workspace_id: include_workspace.then(|| workspace_id.clone()).flatten(),
                service: service.clone(),
                container_port,
                protocol: protocol.clone(),
                source,
                runtime_state,
            }) {
                entries.push(entry);
            }
        }
    }

    entries
}

struct PublishedPortRuntimeEntryInput<'a> {
    workspace: Option<String>,
    workspace_id: Option<String>,
    service: Option<String>,
    source: &'a str,
}

fn compose_published_port_runtime_entry(
    state: &PublishedPortRuntimeState,
    actual_bindings: Vec<PortInventoryActualBinding>,
    input: PublishedPortRuntimeEntryInput<'_>,
) -> Option<PortInventoryEntry> {
    if actual_bindings.is_empty() {
        return None;
    }
    let service = input.service.or_else(|| Some(state.service.clone()));
    Some(PortInventoryEntry {
        workspace: input.workspace,
        workspace_id: input.workspace_id,
        host_ip: display_host_ip(&state.planned),
        host_port: state.planned.host_port,
        kind: PortUsageType::Published,
        service,
        container_port: state.target.port,
        protocol: state.target.protocol.clone(),
        source: input.source.to_owned(),
        port_entry_index: Some(state.port_entry_index),
        target: Some(PortInventoryTarget {
            port: state.target.port,
            protocol: state.target.protocol.clone(),
        }),
        requested: Some(inventory_endpoint(&state.requested)),
        planned: Some(inventory_endpoint(&state.planned)),
        actual_bindings: Some(actual_bindings),
        requested_host_ip_kind: Some(endpoint_kind_name(&state.requested).to_owned()),
        requested_host_ip: state.requested.ip_value.clone(),
        requested_host_port: Some(state.requested.host_port),
        planned_host_ip_kind: Some(endpoint_kind_name(&state.planned).to_owned()),
        planned_host_ip: state.planned.ip_value.clone(),
        planned_host_port: Some(state.planned.host_port),
        relocated: Some(state.relocated),
        label: None,
    })
}

fn actual_bindings_for_runtime_state_from_bindings(
    state: &PublishedPortRuntimeState,
    matched_binding: &ContainerPortBinding,
    bindings: &[ContainerPortBinding],
) -> Vec<PortInventoryActualBinding> {
    let Some(actual_host_port) = binding_host_port(matched_binding) else {
        return Vec::new();
    };
    bindings
        .iter()
        .filter_map(|binding| {
            let host_port = binding_host_port(binding)?;
            (host_port == actual_host_port).then(|| PortInventoryActualBinding {
                host_ip: binding_host_ip(binding),
                host_port,
            })
        })
        .filter(|binding| {
            binding.host_port == state.planned.host_port
                || state.actual_bindings.iter().any(|actual| {
                    actual.host_ip == binding.host_ip && actual.host_port == binding.host_port
                })
        })
        .collect()
}

struct PublishedPortBindingEntryInput<'a> {
    binding: ContainerPortBinding,
    workspace: Option<String>,
    workspace_id: Option<String>,
    service: Option<String>,
    container_port: u16,
    protocol: String,
    source: &'a str,
    runtime_state: Option<&'a PublishedPortRuntimeState>,
}

fn published_port_binding_entry(
    input: PublishedPortBindingEntryInput<'_>,
) -> Option<PortInventoryEntry> {
    let host_port = binding_host_port(&input.binding)?;
    let host_ip = binding_host_ip(&input.binding);
    Some(
        PortInventoryEntry {
            workspace: input.workspace,
            workspace_id: input.workspace_id,
            host_ip,
            host_port,
            kind: PortUsageType::Published,
            service: input.service,
            container_port: input.container_port,
            protocol: input.protocol,
            source: input.source.to_owned(),
            port_entry_index: None,
            target: None,
            requested: None,
            planned: None,
            actual_bindings: None,
            requested_host_ip_kind: requested_host_ip_kind(input.runtime_state),
            requested_host_ip: None,
            requested_host_port: None,
            planned_host_ip_kind: planned_host_ip_kind(input.runtime_state),
            planned_host_ip: input
                .runtime_state
                .and_then(|state| state.planned.ip_value.clone()),
            planned_host_port: input.runtime_state.map(|state| state.planned.host_port),
            relocated: input.runtime_state.map(|state| state.relocated),
            label: None,
        }
        .with_runtime_requested(input.runtime_state),
    )
}

impl PortInventoryEntry {
    fn with_runtime_requested(mut self, runtime_state: Option<&PublishedPortRuntimeState>) -> Self {
        let Some(state) = runtime_state else {
            return self;
        };
        self.requested_host_ip_kind = Some(endpoint_kind_name(&state.requested).to_owned());
        self.requested_host_ip.clone_from(&state.requested.ip_value);
        self.requested_host_port = Some(state.requested.host_port);
        self.port_entry_index = Some(state.port_entry_index);
        self.target = Some(PortInventoryTarget {
            port: state.target.port,
            protocol: state.target.protocol.clone(),
        });
        self.requested = Some(inventory_endpoint(&state.requested));
        self.planned = Some(inventory_endpoint(&state.planned));
        self.actual_bindings = Some(
            state
                .actual_bindings
                .iter()
                .map(inventory_actual_binding)
                .collect(),
        );
        self
    }
}

fn runtime_state_for_published_binding<'a>(
    states: &'a [PublishedPortRuntimeState],
    service: Option<&str>,
    container_port: u16,
    protocol: &str,
    binding: &ContainerPortBinding,
) -> Option<&'a PublishedPortRuntimeState> {
    let service = service?;
    let host_port = binding_host_port(binding)?;
    let host_ip = binding_host_ip(binding);
    states.iter().find(|state| {
        state.service == service
            && state.target.port == container_port
            && state.target.protocol == protocol
            && (state
                .actual_bindings
                .iter()
                .any(|actual| actual.host_ip == host_ip && actual.host_port == host_port)
                || state.planned.host_port == host_port)
    })
}

fn binding_host_port(binding: &ContainerPortBinding) -> Option<u16> {
    binding.host_port.as_deref()?.parse::<u16>().ok()
}

fn binding_host_ip(binding: &ContainerPortBinding) -> String {
    binding
        .host_ip
        .as_deref()
        .filter(|host_ip| !host_ip.trim().is_empty())
        .unwrap_or("0.0.0.0")
        .to_owned()
}

fn requested_host_ip_kind(state: Option<&PublishedPortRuntimeState>) -> Option<String> {
    state.map(|state| endpoint_kind_name(&state.requested).to_owned())
}

fn planned_host_ip_kind(state: Option<&PublishedPortRuntimeState>) -> Option<String> {
    state.map(|state| endpoint_kind_name(&state.planned).to_owned())
}

const fn endpoint_kind_name(endpoint: &PublishedPortEndpointState) -> &'static str {
    match endpoint.ip_kind {
        crate::state::PublishedPortHostIpKind::Omitted => "omitted",
        crate::state::PublishedPortHostIpKind::Explicit => "explicit",
    }
}

fn display_host_ip(endpoint: &PublishedPortEndpointState) -> String {
    match endpoint.ip_kind {
        crate::state::PublishedPortHostIpKind::Omitted => "*".to_owned(),
        crate::state::PublishedPortHostIpKind::Explicit => endpoint
            .ip_value
            .clone()
            .unwrap_or_else(|| "0.0.0.0".to_owned()),
    }
}

fn inventory_endpoint(endpoint: &PublishedPortEndpointState) -> PortInventoryEndpoint {
    PortInventoryEndpoint {
        host_ip: endpoint.ip_value.clone(),
        host_port: endpoint.host_port,
    }
}

fn inventory_actual_binding(
    binding: &crate::state::PublishedPortActualBinding,
) -> PortInventoryActualBinding {
    PortInventoryActualBinding {
        host_ip: binding.host_ip.clone(),
        host_port: binding.host_port,
    }
}

fn container_workspace_id(container: &ContainerInspect) -> Option<String> {
    managed_workspace_id_from_container(container).map(|(workspace_id, _)| workspace_id)
}

pub(super) fn compose_project_name_from_container(container: &ContainerInspect) -> Option<String> {
    container
        .config
        .as_ref()
        .and_then(|config| config.labels.as_ref())
        .and_then(compose_project_name_from_labels)
}

fn compose_service_from_labels(labels: &BTreeMap<String, String>) -> Option<&String> {
    labels
        .get(COMPOSE_PROJECT_LABEL)
        .and_then(|project_name| non_empty_trimmed(project_name))?;
    labels
        .get(COMPOSE_SERVICE_LABEL)
        .filter(|service| non_empty_trimmed(service).is_some())
}

fn container_is_running(container: &ContainerInspect) -> bool {
    container.state.as_ref().is_some_and(|state| {
        state.running == Some(true) || state.status.as_deref() == Some("running")
    })
}

fn parse_docker_port_key(value: &str) -> Option<(u16, String)> {
    let (port, protocol) = value.split_once('/')?;
    Some((port.parse().ok()?, protocol.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ports::{collect::test_context, render},
        state::{
            PublishedPortActualBinding, PublishedPortEndpointState, PublishedPortHostIpKind,
            PublishedPortRuntimeState, PublishedPortRuntimeType, PublishedPortSource,
            PublishedPortTarget,
        },
    };

    #[test]
    fn extracts_published_ports_from_standalone_container_inspect() {
        let containers = serde_json::from_slice(
            br#"[{
                "Id": "container-id",
                "Config": {
                    "Labels": {
                        "decune.managed": "true",
                        "decune.workspace_id": "123456abcdef",
                        "decune.workspace": "/workspace"
                    }
                },
                "State": { "Running": true },
                "NetworkSettings": {
                    "Ports": {
                        "8080/tcp": [{"HostIp": "127.0.0.1", "HostPort": "18080"}],
                        "5353/udp": [{"HostIp": "127.0.0.1", "HostPort": "15353"}],
                        "9000/tcp": null
                    }
                }
            }]"#,
        )
        .unwrap();

        let ports = published_ports_from_containers(containers, None, true);

        assert_eq!(ports.len(), 2);
        let tcp = ports
            .iter()
            .find(|port| port.container_port == 8080 && port.protocol == "tcp")
            .expect("expected tcp published port");
        assert_eq!(tcp.workspace.as_deref(), Some("/workspace"));
        assert_eq!(tcp.workspace_id.as_deref(), Some("123456abcdef"));
        assert_eq!(tcp.kind, PortUsageType::Published);
        assert_eq!(tcp.source, "appPort");
        assert_eq!(tcp.host_ip, "127.0.0.1");
        assert_eq!(tcp.host_port, 18080);

        let udp = ports
            .iter()
            .find(|port| port.container_port == 5353 && port.protocol == "udp")
            .expect("expected udp published port");
        assert_eq!(udp.workspace.as_deref(), Some("/workspace"));
        assert_eq!(udp.workspace_id.as_deref(), Some("123456abcdef"));
        assert_eq!(udp.kind, PortUsageType::Published);
        assert_eq!(udp.source, "appPort");
        assert_eq!(udp.host_ip, "127.0.0.1");
        assert_eq!(udp.host_port, 15353);
    }

    #[test]
    fn extracts_compose_service_name_from_published_ports() {
        let containers = serde_json::from_slice(
            br#"[{
                "Id": "container-id",
                "Config": {
                    "Labels": {
                        "decune.managed": "true",
                        "decune.workspace_id": "123456abcdef",
                        "decune.workspace": "/workspace",
                        "com.docker.compose.project": "decune-project-123456abcdef",
                        "com.docker.compose.service": "app"
                    }
                },
                "State": { "Status": "running" },
                "NetworkSettings": {
                    "Ports": {
                        "3000/tcp": [{"HostIp": "0.0.0.0", "HostPort": "3000"}]
                    }
                }
            }]"#,
        )
        .unwrap();

        let ports = published_ports_from_containers(containers, None, true);

        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].service.as_deref(), Some("app"));
        assert_eq!(ports[0].source, "compose");
        assert_eq!(render::format_target(&ports[0]), "app:3000/tcp");
    }

    #[test]
    fn enriches_compose_published_ports_from_runtime_state() {
        let containers = serde_json::from_slice::<Vec<ContainerInspect>>(
            br#"[{
                "Id": "container-id",
                "Config": {
                    "Labels": {
                        "com.docker.compose.project": "decune-project-123456abcdef",
                        "com.docker.compose.service": "app"
                    }
                },
                "State": { "Running": true },
                "NetworkSettings": {
                    "Ports": {
                        "3000/tcp": [{"HostIp": "0.0.0.0", "HostPort": "3001"}]
                    }
                }
            }]"#,
        )
        .unwrap();
        let mut context = test_context();
        context.published_ports = vec![PublishedPortRuntimeState {
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
            actual_bindings: vec![PublishedPortActualBinding {
                host_ip: "0.0.0.0".to_owned(),
                host_port: 3001,
            }],
            relocated: true,
        }];
        let entries = containers
            .into_iter()
            .map(|container| PublishedContainerInspect {
                container,
                context: Some(context.clone()),
            })
            .collect();

        let ports = published_ports_from_container_entries(entries, false);

        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].host_ip, "*");
        assert_eq!(ports[0].host_port, 3001);
        assert_eq!(ports[0].port_entry_index, Some(0));
        assert_eq!(
            ports[0].target,
            Some(PortInventoryTarget {
                port: 3000,
                protocol: "tcp".to_owned()
            })
        );
        assert_eq!(
            ports[0].requested,
            Some(PortInventoryEndpoint {
                host_ip: None,
                host_port: 3000
            })
        );
        assert_eq!(
            ports[0].planned,
            Some(PortInventoryEndpoint {
                host_ip: None,
                host_port: 3001
            })
        );
        assert_eq!(
            ports[0].actual_bindings,
            Some(vec![PortInventoryActualBinding {
                host_ip: "0.0.0.0".to_owned(),
                host_port: 3001
            }])
        );
        assert_eq!(ports[0].requested_host_ip_kind.as_deref(), Some("omitted"));
        assert_eq!(ports[0].requested_host_ip, None);
        assert_eq!(ports[0].requested_host_port, Some(3000));
        assert_eq!(ports[0].planned_host_ip_kind.as_deref(), Some("omitted"));
        assert_eq!(ports[0].planned_host_port, Some(3001));
        assert_eq!(ports[0].relocated, Some(true));
        assert_eq!(render::format_requested(&ports[0]), "*:3000");
        assert_eq!(render::format_port_state(&ports[0]), "relocated");
    }

    #[test]
    fn extracts_published_ports_from_non_managed_compose_container_with_context() {
        let containers = serde_json::from_slice::<Vec<ContainerInspect>>(
            br#"[{
                "Id": "db-id",
                "Config": {
                    "Labels": {
                        "com.docker.compose.project": "decune-project-123456abcdef",
                        "com.docker.compose.service": "db"
                    }
                },
                "State": { "Running": true },
                "NetworkSettings": {
                    "Ports": {
                        "5432/tcp": [{"HostIp": "127.0.0.1", "HostPort": "15432"}]
                    }
                }
            }]"#,
        )
        .unwrap();
        let entries = containers
            .into_iter()
            .map(|container| PublishedContainerInspect {
                container,
                context: Some(test_context()),
            })
            .collect();

        let ports = published_ports_from_container_entries(entries, true);

        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].workspace.as_deref(), Some("/workspace"));
        assert_eq!(ports[0].workspace_id.as_deref(), Some("123456abcdef"));
        assert_eq!(ports[0].service.as_deref(), Some("db"));
        assert_eq!(ports[0].source, "compose");
        assert_eq!(ports[0].container_port, 5432);
        assert_eq!(ports[0].host_port, 15432);
        assert_eq!(render::format_target(&ports[0]), "db:5432/tcp");
    }

    #[test]
    fn dedupe_published_containers_prefers_later_context_entry() {
        let container = serde_json::from_slice::<Vec<ContainerInspect>>(
            br#"[{
                "Id": "container-id",
                "Config": { "Labels": {} },
                "State": { "Running": true }
            }]"#,
        )
        .unwrap()
        .pop()
        .unwrap();
        let deduped = dedupe_published_containers(vec![
            PublishedContainerInspect {
                container: container.clone(),
                context: None,
            },
            PublishedContainerInspect {
                container,
                context: Some(test_context()),
            },
        ]);

        assert_eq!(deduped.len(), 1);
        assert_eq!(
            deduped[0]
                .context
                .as_ref()
                .map(|context| context.workspace_id.as_str()),
            Some("123456abcdef")
        );
    }

    #[test]
    fn skips_stopped_published_port_containers() {
        let containers = serde_json::from_slice(
            br#"[{
                "Id": "container-id",
                "Config": {
                    "Labels": {
                        "decune.managed": "true",
                        "decune.workspace_id": "123456abcdef"
                    }
                },
                "State": { "Running": false },
                "NetworkSettings": {
                    "Ports": {
                        "8080/tcp": [{"HostIp": "127.0.0.1", "HostPort": "18080"}]
                    }
                }
            }]"#,
        )
        .unwrap();

        let ports = published_ports_from_containers(containers, None, true);

        assert!(ports.is_empty());
    }
}
