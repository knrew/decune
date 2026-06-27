use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};
use serde_json::Value as JsonValue;

use crate::runtime::{
    compose_cli::ComposeOverridePortEntry,
    compose_ports::{
        ComposePortEntry, ComposePublishedPortPlan, compose_published_port_plan_has_relocations,
    },
};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ComposePublishedPortOverride {
    service_ports: BTreeMap<String, Vec<ComposeOverridePortEntry>>,
}

impl ComposePublishedPortOverride {
    #[cfg(test)]
    pub(crate) fn from_service_ports(
        service_ports: BTreeMap<String, Vec<ComposeOverridePortEntry>>,
    ) -> Self {
        Self { service_ports }
    }

    pub(crate) fn ports_for(&self, service: &str) -> Option<&[ComposeOverridePortEntry]> {
        self.service_ports.get(service).map(Vec::as_slice)
    }

    pub(crate) fn services(
        &self,
    ) -> impl Iterator<Item = (&String, &Vec<ComposeOverridePortEntry>)> {
        self.service_ports.iter()
    }
}

pub(crate) fn compose_published_port_override(
    port_entries: &[ComposePortEntry],
    plan: &ComposePublishedPortPlan,
) -> Result<ComposePublishedPortOverride> {
    if !compose_published_port_plan_has_relocations(plan) {
        return Ok(ComposePublishedPortOverride::default());
    }

    let relocated_services = plan
        .entries
        .iter()
        .filter(|entry| entry.relocated)
        .map(|entry| entry.service.clone())
        .collect::<BTreeSet<_>>();
    let planned_entries = plan
        .entries
        .iter()
        .map(|entry| ((entry.service.as_str(), entry.port_entry_index), entry))
        .collect::<BTreeMap<_, _>>();
    let mut service_ports = BTreeMap::new();

    for service in relocated_services {
        let mut service_entries = port_entries
            .iter()
            .filter(|entry| entry.service == service)
            .collect::<Vec<_>>();
        service_entries.sort_by_key(|entry| entry.entry_index);
        let mut ports = Vec::with_capacity(service_entries.len());

        for entry in service_entries {
            ensure_compose_port_entry_can_be_overridden(entry)?;
            let mut fields = entry.original_fields.clone();
            if let Some(planned) = planned_entries.get(&(entry.service.as_str(), entry.entry_index))
            {
                fields.insert(
                    "published".to_owned(),
                    JsonValue::String(planned.planned.host_port.to_string()),
                );
            }
            ports.push(fields);
        }

        service_ports.insert(service, ports);
    }

    Ok(ComposePublishedPortOverride { service_ports })
}

fn ensure_compose_port_entry_can_be_overridden(entry: &ComposePortEntry) -> Result<()> {
    if entry.original_fields.is_empty() {
        bail!(
            "Cannot apply Compose published port relocation for service `{}` port entry {} because Docker Compose config did not return a long-syntax port object",
            entry.service,
            entry.entry_index
        );
    }

    Ok(())
}
#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::runtime::compose_ports::{
        ComposePortProtocol, ComposePublishedPortAllocationReason, ComposePublishedPortEndpoint,
        ComposePublishedPortHostIpKind, ComposePublishedPortPlan, ComposePublishedPortPlanEntry,
        ComposePublishedPortPlanEntryType, ComposePublishedPortPlanSource, test_support::entries,
    };

    #[test]
    fn override_replaces_relocated_service_ports_and_preserves_field_semantics() {
        let port_entries = entries(json!({
            "services": {
                "app": {
                    "ports": [
                        {"target": 3000, "published": "3000", "protocol": "tcp"},
                        {
                            "host_ip": "127.0.0.1",
                            "target": 3001,
                            "published": "3001",
                            "protocol": "tcp",
                            "app_protocol": "http",
                            "name": "loopback",
                            "mode": "host"
                        },
                        {"host_ip": "0.0.0.0", "target": 3002, "published": "3002"},
                        {"target": 8125, "published": "8125", "protocol": "udp"}
                    ]
                },
                "worker": {
                    "ports": [{"target": 4000, "published": "4000"}]
                }
            }
        }));
        let plan = ComposePublishedPortPlan {
            entries: vec![
                ComposePublishedPortPlanEntry {
                    service: "app".to_owned(),
                    port_entry_index: 0,
                    source: ComposePublishedPortPlanSource::Compose,
                    kind: ComposePublishedPortPlanEntryType::Published,
                    target_port: 3000,
                    protocol: ComposePortProtocol::Tcp,
                    requested: ComposePublishedPortEndpoint {
                        host_ip_kind: ComposePublishedPortHostIpKind::Omitted,
                        host_ip_value: None,
                        host_port: 3000,
                    },
                    planned: ComposePublishedPortEndpoint {
                        host_ip_kind: ComposePublishedPortHostIpKind::Omitted,
                        host_ip_value: None,
                        host_port: 3005,
                    },
                    relocated: true,
                    allocation_reason: ComposePublishedPortAllocationReason::Unavailable,
                },
                ComposePublishedPortPlanEntry {
                    service: "app".to_owned(),
                    port_entry_index: 1,
                    source: ComposePublishedPortPlanSource::Compose,
                    kind: ComposePublishedPortPlanEntryType::Published,
                    target_port: 3001,
                    protocol: ComposePortProtocol::Tcp,
                    requested: ComposePublishedPortEndpoint {
                        host_ip_kind: ComposePublishedPortHostIpKind::Explicit,
                        host_ip_value: Some("127.0.0.1".to_owned()),
                        host_port: 3001,
                    },
                    planned: ComposePublishedPortEndpoint {
                        host_ip_kind: ComposePublishedPortHostIpKind::Explicit,
                        host_ip_value: Some("127.0.0.1".to_owned()),
                        host_port: 3006,
                    },
                    relocated: true,
                    allocation_reason: ComposePublishedPortAllocationReason::Unavailable,
                },
                ComposePublishedPortPlanEntry {
                    service: "app".to_owned(),
                    port_entry_index: 2,
                    source: ComposePublishedPortPlanSource::Compose,
                    kind: ComposePublishedPortPlanEntryType::Published,
                    target_port: 3002,
                    protocol: ComposePortProtocol::Tcp,
                    requested: ComposePublishedPortEndpoint {
                        host_ip_kind: ComposePublishedPortHostIpKind::Explicit,
                        host_ip_value: Some("0.0.0.0".to_owned()),
                        host_port: 3002,
                    },
                    planned: ComposePublishedPortEndpoint {
                        host_ip_kind: ComposePublishedPortHostIpKind::Explicit,
                        host_ip_value: Some("0.0.0.0".to_owned()),
                        host_port: 3007,
                    },
                    relocated: true,
                    allocation_reason: ComposePublishedPortAllocationReason::Unavailable,
                },
            ],
        };

        let port_override = compose_published_port_override(&port_entries, &plan).unwrap();
        let app_ports = port_override.ports_for("app").unwrap();

        assert_eq!(app_ports.len(), 4);
        assert_eq!(app_ports[0].get("published"), Some(&json!("3005")));
        assert_eq!(app_ports[0].get("host_ip"), None);
        assert_eq!(app_ports[1].get("published"), Some(&json!("3006")));
        assert_eq!(app_ports[1].get("host_ip"), Some(&json!("127.0.0.1")));
        assert_eq!(app_ports[1].get("app_protocol"), Some(&json!("http")));
        assert_eq!(app_ports[1].get("name"), Some(&json!("loopback")));
        assert_eq!(app_ports[1].get("mode"), Some(&json!("host")));
        assert_eq!(app_ports[2].get("published"), Some(&json!("3007")));
        assert_eq!(app_ports[2].get("host_ip"), Some(&json!("0.0.0.0")));
        assert_eq!(app_ports[3].get("published"), Some(&json!("8125")));
        assert_eq!(port_override.ports_for("worker"), None);
    }
}
