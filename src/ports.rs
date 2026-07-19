use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::{ui, workspace::Workspace};

mod collect;
mod context;
mod published;
mod render;
mod types;

pub(crate) use collect::{collect_all_ports, collect_workspace_ports};
pub(crate) use render::{render_ports_table, sort_ports};
pub(crate) use types::{ContainerPortSnapshot, PortInventory, PortInventoryEntry, PortUsageType};

pub(crate) use collect::container_forwarded_port_snapshots;
pub(crate) use published::container_published_port_snapshots;

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

pub(crate) fn container_port_inventory(ports: &[ContainerPortSnapshot]) -> Vec<PortInventoryEntry> {
    let mut inventory = ports
        .iter()
        .cloned()
        .map(PortInventoryEntry::from)
        .collect::<Vec<_>>();
    sort_ports(&mut inventory);
    inventory
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the container query daemon connects this pure seam in follow-up task #431"
    )
)]
pub(crate) fn render_container_ports_text(ports: &[ContainerPortSnapshot]) -> String {
    render_ports_table(&container_port_inventory(ports), false)
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the container query daemon connects this pure seam in follow-up task #431"
    )
)]
pub(crate) fn render_container_ports_json(ports: &[ContainerPortSnapshot]) -> Result<String> {
    let inventory = container_port_inventory(ports);
    let mut output = serde_json::to_string_pretty(&inventory)
        .context("Failed to serialize container port snapshot")?;
    output.push('\n');
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn port(workspace: Option<&str>, workspace_id: Option<&str>) -> PortInventoryEntry {
        PortInventoryEntry {
            workspace: workspace.map(str::to_owned),
            workspace_id: workspace_id.map(str::to_owned),
            host_ip: "127.0.0.1".to_owned(),
            host_port: 3000,
            kind: PortUsageType::Forwarded,
            service: Some("app".to_owned()),
            container_port: 3000,
            protocol: "tcp".to_owned(),
            source: "configured".to_owned(),
            port_entry_index: None,
            target: None,
            requested: None,
            planned: None,
            actual_bindings: None,
            requested_host_ip_kind: None,
            requested_host_ip: None,
            requested_host_port: None,
            planned_host_ip_kind: None,
            planned_host_ip: None,
            planned_host_port: None,
            relocated: None,
            label: Some("web".to_owned()),
        }
    }

    #[test]
    fn container_snapshot_drops_workspace_identity_and_uses_single_workspace_schema() {
        let snapshot = ContainerPortSnapshot::from(&port(
            Some("/host/secret/workspace"),
            Some("123456abcdef"),
        ));

        let inventory = container_port_inventory(std::slice::from_ref(&snapshot));
        let text = render_container_ports_text(std::slice::from_ref(&snapshot));
        let json = render_container_ports_json(&[snapshot]).unwrap();

        assert_eq!(inventory.len(), 1);
        assert_eq!(inventory[0].workspace, None);
        assert_eq!(inventory[0].workspace_id, None);
        assert!(text.starts_with("LOCAL"));
        assert!(!text.contains("WORKSPACE"));
        assert!(!text.contains("/host/secret/workspace"));
        assert!(!json.contains("\"workspace\""));
        assert!(!json.contains("\"workspace_id\""));
        assert!(json.ends_with('\n'));
        assert!(!json.ends_with("\n\n"));
    }

    #[test]
    fn container_ports_share_host_sort_order_for_single_workspace_output() {
        let mut higher = port(None, None);
        higher.host_port = 4000;
        let mut lower = port(None, None);
        lower.host_port = 3000;
        let snapshots = [
            ContainerPortSnapshot::from(&higher),
            ContainerPortSnapshot::from(&lower),
        ];

        let inventory = container_port_inventory(&snapshots);

        assert_eq!(
            inventory
                .iter()
                .map(|entry| entry.host_port)
                .collect::<Vec<_>>(),
            vec![3000, 4000]
        );
    }
}
