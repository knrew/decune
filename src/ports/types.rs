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
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Forwarded => "forwarded",
            Self::Published => "published",
        }
    }
}
