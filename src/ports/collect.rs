use std::collections::BTreeMap;

#[cfg(test)]
use std::path::PathBuf;

use anyhow::Result;

use crate::{
    docker::{
        client::DockerClient,
        resource::{managed_workspace_id_from_container, workspace_path_from_labels},
    },
    host::forward::{ActiveForwardPort, forward_status_dir, list_active_forward_status_ports},
    state::load_state_file,
    workspace::{Workspace, decune_state_root},
};

use super::{
    context::{WorkspacePortContext, context_for_workspace_id, load_port_states},
    published::{
        PublishedContainerInspect, compose_project_name_from_container,
        dedupe_published_containers, published_ports_from_container_entries,
    },
    types::{PortInventory, PortInventoryEntry, PortUsageType},
};

pub(crate) async fn collect_workspace_ports(
    workspace: &Workspace,
    include_workspace: bool,
) -> Result<PortInventory> {
    let mut context = WorkspacePortContext {
        workspace_id: workspace.id().to_owned(),
        workspace_path: Some(workspace.root().display().to_string()),
        runtime_dir: workspace.paths().runtime_dir().to_path_buf(),
        published_ports: Vec::new(),
    };
    let mut inventory = collect_forwarded_ports(&context, include_workspace).await?;
    let mut containers = Vec::new();
    let mut compose_projects = BTreeMap::<String, WorkspacePortContext>::new();

    let state_exists = workspace.paths().state_dir().exists();
    match load_state_file(workspace.paths().state_dir()) {
        Ok(Some(state)) => {
            context.published_ports = state.published_ports;
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
                context.published_ports = state.published_ports;
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
        port_entry_index: None,
        target: None,
        requested: None,
        planned: None,
        actual_bindings: None,
        requested_host_ip_kind: None,
        requested_host_ip: requested.as_ref().map(|(host_ip, _)| host_ip.clone()),
        requested_host_port: requested.map(|(_, host_port)| host_port),
        planned_host_ip_kind: None,
        planned_host_ip: None,
        planned_host_port: None,
        relocated: None,
        label: port.label,
    }
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

#[cfg(test)]
pub(super) fn test_context() -> WorkspacePortContext {
    WorkspacePortContext {
        workspace_id: "123456abcdef".to_owned(),
        workspace_path: Some("/workspace".to_owned()),
        runtime_dir: PathBuf::from("/tmp/decune/123456abcdef"),
        published_ports: Vec::new(),
    }
}
