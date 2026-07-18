use serde::Serialize;

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
    pub(crate) port_entry_index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) target: Option<PortInventoryTarget>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) requested: Option<PortInventoryEndpoint>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) planned: Option<PortInventoryEndpoint>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) actual_bindings: Option<Vec<PortInventoryActualBinding>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) requested_host_ip_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) requested_host_ip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) requested_host_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) planned_host_ip_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) planned_host_ip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) planned_host_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) relocated: Option<bool>,
    pub(crate) label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ContainerPortSnapshot {
    pub(crate) host_ip: String,
    pub(crate) host_port: u16,
    #[serde(rename = "type")]
    pub(crate) kind: PortUsageType,
    pub(crate) service: Option<String>,
    pub(crate) container_port: u16,
    pub(crate) protocol: String,
    pub(crate) source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) port_entry_index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) target: Option<PortInventoryTarget>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) requested: Option<PortInventoryEndpoint>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) planned: Option<PortInventoryEndpoint>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) actual_bindings: Option<Vec<PortInventoryActualBinding>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) requested_host_ip_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) requested_host_ip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) requested_host_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) planned_host_ip_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) planned_host_ip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) planned_host_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) relocated: Option<bool>,
    pub(crate) label: Option<String>,
}

impl From<&PortInventoryEntry> for ContainerPortSnapshot {
    fn from(entry: &PortInventoryEntry) -> Self {
        Self {
            host_ip: entry.host_ip.clone(),
            host_port: entry.host_port,
            kind: entry.kind,
            service: entry.service.clone(),
            container_port: entry.container_port,
            protocol: entry.protocol.clone(),
            source: entry.source.clone(),
            port_entry_index: entry.port_entry_index,
            target: entry.target.clone(),
            requested: entry.requested.clone(),
            planned: entry.planned.clone(),
            actual_bindings: entry.actual_bindings.clone(),
            requested_host_ip_kind: entry.requested_host_ip_kind.clone(),
            requested_host_ip: entry.requested_host_ip.clone(),
            requested_host_port: entry.requested_host_port,
            planned_host_ip_kind: entry.planned_host_ip_kind.clone(),
            planned_host_ip: entry.planned_host_ip.clone(),
            planned_host_port: entry.planned_host_port,
            relocated: entry.relocated,
            label: entry.label.clone(),
        }
    }
}

impl From<ContainerPortSnapshot> for PortInventoryEntry {
    fn from(snapshot: ContainerPortSnapshot) -> Self {
        Self {
            workspace: None,
            workspace_id: None,
            host_ip: snapshot.host_ip,
            host_port: snapshot.host_port,
            kind: snapshot.kind,
            service: snapshot.service,
            container_port: snapshot.container_port,
            protocol: snapshot.protocol,
            source: snapshot.source,
            port_entry_index: snapshot.port_entry_index,
            target: snapshot.target,
            requested: snapshot.requested,
            planned: snapshot.planned,
            actual_bindings: snapshot.actual_bindings,
            requested_host_ip_kind: snapshot.requested_host_ip_kind,
            requested_host_ip: snapshot.requested_host_ip,
            requested_host_port: snapshot.requested_host_port,
            planned_host_ip_kind: snapshot.planned_host_ip_kind,
            planned_host_ip: snapshot.planned_host_ip,
            planned_host_port: snapshot.planned_host_port,
            relocated: snapshot.relocated,
            label: snapshot.label,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct PortInventoryTarget {
    pub(crate) port: u16,
    pub(crate) protocol: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct PortInventoryEndpoint {
    pub(crate) host_ip: Option<String>,
    pub(crate) host_port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct PortInventoryActualBinding {
    pub(crate) host_ip: String,
    pub(crate) host_port: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum PortUsageType {
    Forwarded,
    Published,
}

impl PortUsageType {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Forwarded => "forwarded",
            Self::Published => "published",
        }
    }
}
