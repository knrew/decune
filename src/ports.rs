use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs, io,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::{
    docker::{
        client::DockerClient,
        container::{ContainerInspect, ContainerPortBinding},
        resource::{managed_workspace_id_from_container, workspace_path_from_labels},
    },
    host::forward::{ActiveForwardPort, forward_status_dir, list_active_forward_status_ports},
    state::{WorkspaceState, load_state_file},
    ui,
    workspace::{
        Workspace, decune_state_root, is_valid_workspace_id, runtime_dir_for_workspace_id,
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PortsOptions {
    pub(crate) workspace: Option<PathBuf>,
    pub(crate) all: bool,
    pub(crate) json: bool,
}

pub(crate) async fn run_ports(options: PortsOptions) -> Result<()> {
    let mut inventory = if options.all {
        collect_all_ports().await?
    } else {
        let workspace =
            Workspace::resolve(options.workspace.unwrap_or_else(|| PathBuf::from(".")))?;
        collect_workspace_ports(&workspace, false).await?
    };
    for warning in inventory.warnings {
        ui::warn(&warning);
    }
    sort_ports(&mut inventory.ports);

    if options.json {
        let output = serde_json::to_string_pretty(&inventory.ports)
            .context("Failed to serialize active ports")?;
        println!("{output}");
    } else {
        print!("{}", render_ports_table(&inventory.ports, options.all));
    }

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct PortInventory {
    pub(crate) ports: Vec<PortInventoryEntry>,
    pub(crate) warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct PortInventoryEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) workspace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) workspace_id: Option<String>,
    pub(crate) host_ip: String,
    pub(crate) host_port: u16,
    #[serde(rename = "type")]
    pub(crate) kind: PortUsageType,
    pub(crate) service: Option<String>,
    pub(crate) container_port: u16,
    pub(crate) protocol: String,
    pub(crate) source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) requested_host_ip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) requested_host_port: Option<u16>,
    pub(crate) label: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum PortUsageType {
    Forwarded,
    Published,
}

impl PortUsageType {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Forwarded => "forwarded",
            Self::Published => "published",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkspacePortContext {
    workspace_id: String,
    workspace_path: Option<String>,
    runtime_dir: PathBuf,
}

#[derive(Debug, Clone)]
struct StatePortEntry {
    workspace_id: String,
    state: Result<WorkspaceState, String>,
}

#[derive(Debug, Clone)]
struct PublishedContainerInspect {
    container: ContainerInspect,
    context: Option<WorkspacePortContext>,
}

pub(crate) async fn collect_workspace_ports(
    workspace: &Workspace,
    include_workspace: bool,
) -> Result<PortInventory> {
    let context = WorkspacePortContext {
        workspace_id: workspace.id().to_owned(),
        workspace_path: Some(workspace.root().display().to_string()),
        runtime_dir: workspace.paths().runtime_dir().to_path_buf(),
    };
    let mut inventory = collect_forwarded_ports(&context, include_workspace).await?;
    let mut containers = Vec::new();
    let mut compose_projects = BTreeMap::<String, WorkspacePortContext>::new();

    let state_exists = workspace.paths().state_dir().exists();
    match load_state_file(workspace.paths().state_dir()) {
        Ok(Some(state)) => {
            add_compose_project_context(
                &mut compose_projects,
                state.compose_project_name.as_deref(),
                &context,
            );
        }
        Ok(None) => {}
        Err(error) if state_exists => inventory.warnings.push(format!(
            "Failed to read decune state file while listing ports for workspace {}: {error:#}",
            workspace.id()
        )),
        Err(_) => {}
    }

    match DockerClient::connect_from_env() {
        Ok(client) => match client
            .cli()
            .list_workspace_container_inspects(workspace.id())
            .await
        {
            Ok(discovered) => {
                for container in discovered {
                    add_compose_project_context(
                        &mut compose_projects,
                        compose_project_name_from_container(&container).as_deref(),
                        &context,
                    );
                    containers.push(PublishedContainerInspect {
                        container,
                        context: Some(context.clone()),
                    });
                }

                for (project_name, project_context) in compose_projects {
                    match client
                        .cli()
                        .list_compose_project_container_inspects_by_project(&project_name)
                        .await
                    {
                        Ok(project_containers) => {
                            containers.extend(project_containers.into_iter().map(|container| {
                                PublishedContainerInspect {
                                    container,
                                    context: Some(project_context.clone()),
                                }
                            }));
                        }
                        Err(error) => inventory.warnings.push(format!(
                            "Failed to read Docker Compose project containers for workspace {} project {}: {error:#}",
                            project_context.workspace_id, project_name
                        )),
                    }
                }
            }
            Err(error) if state_exists => inventory.warnings.push(format!(
                "Failed to read Docker published ports for workspace {}: {error:#}",
                workspace.id()
            )),
            Err(_) => {}
        },
        Err(error) if state_exists => inventory.warnings.push(format!(
            "Failed to read Docker published ports for workspace {}: {error:#}",
            workspace.id()
        )),
        Err(_) => {}
    }
    inventory
        .ports
        .extend(published_ports_from_container_entries(
            dedupe_published_containers(containers),
            include_workspace,
        ));

    Ok(inventory)
}

pub(crate) async fn collect_all_ports() -> Result<PortInventory> {
    let states = load_port_states(&decune_state_root()?)?;
    let mut contexts = BTreeMap::<String, WorkspacePortContext>::new();
    let mut compose_projects = BTreeMap::<String, WorkspacePortContext>::new();
    let mut containers = Vec::new();
    let mut warnings = Vec::new();

    for entry in states {
        let context = contexts
            .entry(entry.workspace_id.clone())
            .or_insert_with(|| context_for_workspace_id(&entry.workspace_id));
        match entry.state {
            Ok(state) => {
                context.workspace_path.get_or_insert(state.workspace);
                add_compose_project_context(
                    &mut compose_projects,
                    state.compose_project_name.as_deref(),
                    context,
                );
            }
            Err(error) => warnings.push(format!(
                "Ignoring invalid decune state file for workspace id {} while listing ports: {error}",
                entry.workspace_id
            )),
        }
    }

    match DockerClient::connect_from_env() {
        Ok(client) => match client.cli().list_all_managed_container_inspects().await {
            Ok(discovered) => {
                for container in discovered {
                    let Some((workspace_id, labels)) =
                        managed_workspace_id_from_container(&container)
                    else {
                        continue;
                    };
                    let context = contexts
                        .entry(workspace_id.clone())
                        .or_insert_with(|| context_for_workspace_id(&workspace_id));
                    if let Some(workspace_path) = workspace_path_from_labels(labels) {
                        context.workspace_path.get_or_insert(workspace_path);
                    }
                    add_compose_project_context(
                        &mut compose_projects,
                        compose_project_name_from_container(&container).as_deref(),
                        context,
                    );
                    containers.push(PublishedContainerInspect {
                        container,
                        context: Some(context.clone()),
                    });
                }

                for (project_name, project_context) in compose_projects.clone() {
                    match client
                        .cli()
                        .list_compose_project_container_inspects_by_project(&project_name)
                        .await
                    {
                        Ok(project_containers) => {
                            containers.extend(project_containers.into_iter().map(|container| {
                                PublishedContainerInspect {
                                    container,
                                    context: Some(project_context.clone()),
                                }
                            }));
                        }
                        Err(error) => warnings.push(format!(
                            "Failed to read Docker Compose project containers for workspace {} project {} while listing ports: {error:#}",
                            project_context.workspace_id, project_name
                        )),
                    }
                }
            }
            Err(error) => warnings.push(format!(
                "Failed to read decune-managed Docker containers while listing ports: {error:#}"
            )),
        },
        Err(error) => warnings.push(format!(
            "Failed to connect to Docker while listing published ports: {error:#}"
        )),
    }

    let mut inventory = PortInventory {
        ports: Vec::new(),
        warnings,
    };
    for context in contexts.values() {
        let mut forwarded = collect_forwarded_ports(context, true).await?;
        inventory.ports.append(&mut forwarded.ports);
        inventory.warnings.append(&mut forwarded.warnings);
    }
    inventory
        .ports
        .extend(published_ports_from_container_entries(
            dedupe_published_containers(containers),
            true,
        ));

    Ok(inventory)
}

async fn collect_forwarded_ports(
    context: &WorkspacePortContext,
    include_workspace: bool,
) -> Result<PortInventory> {
    let status_dir = forward_status_dir(&context.runtime_dir);
    let status = list_active_forward_status_ports(status_dir).await?;
    Ok(PortInventory {
        ports: status
            .ports
            .into_iter()
            .map(|port| forwarded_inventory_entry(port, context, include_workspace))
            .collect(),
        warnings: status.warnings,
    })
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

fn published_ports_from_container_entries(
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

fn dedupe_published_containers(
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
    for (target, bindings) in ports {
        let Some((container_port, protocol)) = parse_docker_port_key(&target) else {
            continue;
        };
        let Some(bindings) = bindings else {
            continue;
        };
        for binding in bindings {
            if let Some(entry) = published_port_binding_entry(
                binding,
                include_workspace.then(|| workspace_path.clone()).flatten(),
                include_workspace.then(|| workspace_id.clone()).flatten(),
                service.clone(),
                container_port,
                protocol.clone(),
                source,
            ) {
                entries.push(entry);
            }
        }
    }

    entries
}

fn published_port_binding_entry(
    binding: ContainerPortBinding,
    workspace: Option<String>,
    workspace_id: Option<String>,
    service: Option<String>,
    container_port: u16,
    protocol: String,
    source: &str,
) -> Option<PortInventoryEntry> {
    let host_port = binding.host_port.as_deref()?.parse::<u16>().ok()?;
    let host_ip = binding
        .host_ip
        .filter(|host_ip| !host_ip.trim().is_empty())
        .unwrap_or_else(|| "0.0.0.0".to_owned());
    Some(PortInventoryEntry {
        workspace,
        workspace_id,
        host_ip,
        host_port,
        kind: PortUsageType::Published,
        service,
        container_port,
        protocol,
        source: source.to_owned(),
        requested_host_ip: None,
        requested_host_port: None,
        label: None,
    })
}

fn forwarded_inventory_entry(
    port: ActiveForwardPort,
    context: &WorkspacePortContext,
    include_workspace: bool,
) -> PortInventoryEntry {
    let requested = (port.host_port != port.requested_host_port)
        .then_some((port.host_ip.clone(), port.requested_host_port));
    PortInventoryEntry {
        workspace: include_workspace
            .then(|| context.workspace_path.clone())
            .flatten(),
        workspace_id: include_workspace.then(|| context.workspace_id.clone()),
        host_ip: port.host_ip,
        host_port: port.host_port,
        kind: PortUsageType::Forwarded,
        service: port.service,
        container_port: port.container_port,
        protocol: port.protocol,
        source: port.source.as_str().to_owned(),
        requested_host_ip: requested.as_ref().map(|(host_ip, _)| host_ip.clone()),
        requested_host_port: requested.map(|(_, host_port)| host_port),
        label: port.label,
    }
}

pub(crate) fn sort_ports(ports: &mut [PortInventoryEntry]) {
    ports.sort_by(|left, right| {
        (
            left.workspace.as_deref().unwrap_or("\u{10ffff}"),
            left.workspace_id.as_deref().unwrap_or(""),
            &left.host_ip,
            left.host_port,
            left.kind,
            left.service.as_deref(),
            left.container_port,
            &left.protocol,
            &left.source,
            left.label.as_deref(),
        )
            .cmp(&(
                right.workspace.as_deref().unwrap_or("\u{10ffff}"),
                right.workspace_id.as_deref().unwrap_or(""),
                &right.host_ip,
                right.host_port,
                right.kind,
                right.service.as_deref(),
                right.container_port,
                &right.protocol,
                &right.source,
                right.label.as_deref(),
            ))
    });
}

pub(crate) fn render_ports_table(ports: &[PortInventoryEntry], include_workspace: bool) -> String {
    if ports.is_empty() {
        return if include_workspace {
            "No active ports\n".to_owned()
        } else {
            "No active ports for this workspace\n".to_owned()
        };
    }

    let headers = if include_workspace {
        vec![
            "WORKSPACE",
            "ID",
            "LOCAL",
            "TYPE",
            "TARGET",
            "SOURCE",
            "REQUESTED",
            "LABEL",
        ]
    } else {
        vec!["LOCAL", "TYPE", "TARGET", "SOURCE", "REQUESTED", "LABEL"]
    };
    let rows = ports.iter().map(port_row).collect::<Vec<_>>();
    let mut widths = headers
        .iter()
        .map(|header| header.len())
        .collect::<Vec<_>>();
    for row in &rows {
        let columns = row.columns(include_workspace);
        for (index, column) in columns.iter().enumerate() {
            widths[index] = widths[index].max(column.len());
        }
    }

    let mut output = String::new();
    write_row(&mut output, &headers, &widths);
    for row in &rows {
        write_row(&mut output, &row.columns(include_workspace), &widths);
    }
    output
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PortRow {
    workspace: String,
    workspace_id: String,
    local: String,
    kind: String,
    target: String,
    source: String,
    requested: String,
    label: String,
}

impl PortRow {
    fn columns(&self, include_workspace: bool) -> Vec<&str> {
        if include_workspace {
            vec![
                self.workspace.as_str(),
                self.workspace_id.as_str(),
                self.local.as_str(),
                self.kind.as_str(),
                self.target.as_str(),
                self.source.as_str(),
                self.requested.as_str(),
                self.label.as_str(),
            ]
        } else {
            vec![
                self.local.as_str(),
                self.kind.as_str(),
                self.target.as_str(),
                self.source.as_str(),
                self.requested.as_str(),
                self.label.as_str(),
            ]
        }
    }
}

fn port_row(port: &PortInventoryEntry) -> PortRow {
    PortRow {
        workspace: port.workspace.as_deref().unwrap_or("<unknown>").to_owned(),
        workspace_id: port.workspace_id.as_deref().unwrap_or("-").to_owned(),
        local: format_endpoint(&port.host_ip, port.host_port),
        kind: port.kind.as_str().to_owned(),
        target: format_target(port),
        source: port.source.clone(),
        requested: format_requested(port),
        label: port
            .label
            .as_deref()
            .filter(|label| !label.is_empty())
            .unwrap_or("-")
            .to_owned(),
    }
}

fn write_row(output: &mut String, columns: &[&str], widths: &[usize]) {
    for (index, column) in columns.iter().enumerate() {
        if index > 0 {
            output.push_str("  ");
        }
        let _ = write!(output, "{:<width$}", column, width = widths[index]);
    }
    output.push('\n');
}

fn format_requested(port: &PortInventoryEntry) -> String {
    match (&port.requested_host_ip, port.requested_host_port) {
        (Some(host_ip), Some(host_port)) => format_endpoint(host_ip, host_port),
        _ => "-".to_owned(),
    }
}

fn format_target(port: &PortInventoryEntry) -> String {
    let target = port.service.as_deref().unwrap_or("container");
    format!("{target}:{}/{}", port.container_port, port.protocol)
}

fn format_endpoint(host_ip: &str, port: u16) -> String {
    if host_ip.contains(':') && !host_ip.starts_with('[') {
        format!("[{host_ip}]:{port}")
    } else {
        format!("{host_ip}:{port}")
    }
}

fn load_port_states(root: &Path) -> Result<Vec<StatePortEntry>> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to read decune state root: {}", root.display()));
        }
    };
    let mut states = Vec::new();

    for entry in entries {
        let entry = entry.with_context(|| {
            format!("Failed to read decune state root entry: {}", root.display())
        })?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(workspace_id) = path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
        else {
            continue;
        };
        if !is_valid_workspace_id(&workspace_id) {
            continue;
        }
        match load_state_file(&path) {
            Ok(Some(state)) => states.push(StatePortEntry {
                workspace_id,
                state: Ok(state),
            }),
            Ok(None) => {}
            Err(error) => states.push(StatePortEntry {
                workspace_id,
                state: Err(format!("{error:#}")),
            }),
        }
    }

    Ok(states)
}

fn context_for_workspace_id(workspace_id: &str) -> WorkspacePortContext {
    WorkspacePortContext {
        workspace_id: workspace_id.to_owned(),
        workspace_path: None,
        runtime_dir: runtime_dir_for_workspace_id(workspace_id)
            .unwrap_or_else(|_| PathBuf::from(workspace_id)),
    }
}

fn container_workspace_id(container: &ContainerInspect) -> Option<String> {
    managed_workspace_id_from_container(container).map(|(workspace_id, _)| workspace_id)
}

fn add_compose_project_context(
    projects: &mut BTreeMap<String, WorkspacePortContext>,
    project_name: Option<&str>,
    context: &WorkspacePortContext,
) {
    let Some(project_name) = project_name
        .map(str::trim)
        .filter(|project_name| !project_name.is_empty())
    else {
        return;
    };
    projects
        .entry(project_name.to_owned())
        .or_insert_with(|| context.clone());
}

fn compose_project_name_from_container(container: &ContainerInspect) -> Option<String> {
    container
        .config
        .as_ref()
        .and_then(|config| config.labels.as_ref())
        .and_then(|labels| labels.get("com.docker.compose.project"))
        .filter(|project_name| !project_name.trim().is_empty())
        .cloned()
}

fn compose_service_from_labels(labels: &BTreeMap<String, String>) -> Option<&String> {
    labels
        .get("com.docker.compose.project")
        .filter(|project_name| !project_name.trim().is_empty())?;
    labels
        .get("com.docker.compose.service")
        .filter(|service| !service.trim().is_empty())
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
    use crate::host::forward::ForwardStatusSource;

    fn port(host_port: u16, requested_host_port: u16) -> PortInventoryEntry {
        forwarded_inventory_entry(
            ActiveForwardPort {
                host_ip: "127.0.0.1".to_owned(),
                host_port,
                requested_host_port,
                service: None,
                container_port: 3000,
                protocol: "tcp".to_owned(),
                source: ForwardStatusSource::Configured,
                label: Some("web".to_owned()),
            },
            &context(),
            false,
        )
    }

    fn context() -> WorkspacePortContext {
        WorkspacePortContext {
            workspace_id: "123456abcdef".to_owned(),
            workspace_path: Some("/workspace".to_owned()),
            runtime_dir: PathBuf::from("/tmp/decune/123456abcdef"),
        }
    }

    #[test]
    fn renders_no_active_ports() {
        assert_eq!(
            render_ports_table(&[], false),
            "No active ports for this workspace\n"
        );
        assert_eq!(render_ports_table(&[], true), "No active ports\n");
    }

    #[test]
    fn renders_active_ports_table() {
        let mut ports = vec![port(3001, 3000)];
        ports[0].service = Some("app".to_owned());
        ports[0].source = ForwardStatusSource::Auto.as_str().to_owned();

        let table = render_ports_table(&ports, false);

        assert!(table.contains("LOCAL"));
        assert!(table.contains("TYPE"));
        assert!(table.contains("127.0.0.1:3001"));
        assert!(table.contains("forwarded"));
        assert!(table.contains("app:3000/tcp"));
        assert!(table.contains("auto"));
        assert!(table.contains("127.0.0.1:3000"));
        assert!(table.contains("web"));
    }

    #[test]
    fn renders_all_ports_table_with_workspace_identity() {
        let mut ports = vec![port(3000, 3000)];
        ports[0].workspace = Some("/workspace".to_owned());
        ports[0].workspace_id = Some("123456abcdef".to_owned());

        let table = render_ports_table(&ports, true);

        assert!(table.contains("WORKSPACE"));
        assert!(table.contains("ID"));
        assert!(table.contains("/workspace"));
        assert!(table.contains("123456abcdef"));
    }

    #[test]
    fn formats_ipv6_endpoints_with_brackets() {
        assert_eq!(format_endpoint("::1", 3000), "[::1]:3000");
        assert_eq!(format_endpoint("[::1]", 3000), "[::1]:3000");
    }

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
        assert_eq!(format_target(&ports[0]), "app:3000/tcp");
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
                context: Some(context()),
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
        assert_eq!(format_target(&ports[0]), "db:5432/tcp");
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
                context: Some(context()),
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
