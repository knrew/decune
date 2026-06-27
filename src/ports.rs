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
pub(crate) use types::{PortInventory, PortInventoryEntry, PortUsageType};

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
