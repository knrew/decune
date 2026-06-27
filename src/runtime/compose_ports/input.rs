use std::collections::BTreeSet;

use crate::runtime::{
    compose_cli::ComposeConfigModel,
    compose_ports::{ComposePortEntry, ComposePublishedPortPlanningInput},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposeActiveServiceSet {
    pub(crate) primary_service: String,
    pub(crate) selected_services: Vec<String>,
    active_services: BTreeSet<String>,
}

impl ComposeActiveServiceSet {
    pub(crate) fn new(
        model: &ComposeConfigModel,
        primary_service: &str,
        selected_services: &[String],
    ) -> Self {
        let active_services = model
            .services()
            .map(|(service_name, _)| service_name.clone())
            .collect::<BTreeSet<_>>();
        let selected_services = unique_active_services(selected_services, &active_services);

        Self {
            primary_service: primary_service.to_owned(),
            selected_services,
            active_services,
        }
    }

    pub(crate) fn contains(&self, service: &str) -> bool {
        self.active_services.contains(service)
    }

    pub(crate) fn ordered_services_for_planning(&self) -> Vec<String> {
        let mut services = Vec::new();
        push_active_service(
            &mut services,
            &self.active_services,
            self.primary_service.as_str(),
        );
        for service in &self.selected_services {
            push_active_service(&mut services, &self.active_services, service);
        }
        for service in &self.active_services {
            push_active_service(&mut services, &self.active_services, service);
        }
        services
    }
}

pub(crate) fn compose_published_port_planning_input(
    model: &ComposeConfigModel,
    published_port_entries: &[ComposePortEntry],
    primary_service: &str,
    selected_services: &[String],
) -> ComposePublishedPortPlanningInput {
    let services = ComposeActiveServiceSet::new(model, primary_service, selected_services);
    let port_entries = published_port_entries
        .iter()
        .filter(|entry| services.contains(&entry.service))
        .cloned()
        .collect();

    ComposePublishedPortPlanningInput {
        services,
        port_entries,
    }
}

fn unique_active_services(services: &[String], active_services: &BTreeSet<String>) -> Vec<String> {
    let mut unique = Vec::new();
    for service in services {
        if active_services.contains(service) && !unique.iter().any(|existing| existing == service) {
            unique.push(service.clone());
        }
    }
    unique
}

fn push_active_service(
    services: &mut Vec<String>,
    active_services: &BTreeSet<String>,
    service: &str,
) {
    if active_services.contains(service) && !services.iter().any(|existing| existing == service) {
        services.push(service.to_owned());
    }
}
#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::*;
    use crate::runtime::compose_ports::{
        ComposePortEligibility, ComposePortEntry, ComposePortHostIp, ComposePortProtocol,
        ComposePortSyntax, ComposePublishedHostPort, test_support::model,
    };

    #[test]
    fn active_service_set_preserves_selected_order_and_prioritizes_primary() {
        let model = model(json!({
            "services": {
                "app": {},
                "db": {},
                "worker": {},
                "z-sidecar": {}
            }
        }));
        let selected = vec![
            "worker".to_owned(),
            "app".to_owned(),
            "worker".to_owned(),
            "missing".to_owned(),
        ];

        let services = ComposeActiveServiceSet::new(&model, "app", &selected);

        assert_eq!(services.primary_service, "app");
        assert_eq!(services.selected_services, ["worker", "app"]);
        assert!(services.contains("db"));
        assert!(services.contains("z-sidecar"));
        assert!(!services.contains("missing"));
        assert_eq!(
            services.ordered_services_for_planning(),
            ["app", "worker", "db", "z-sidecar"]
        );
    }

    #[test]
    fn active_service_set_orders_primary_first_when_whole_project_is_selected() {
        let model = model(json!({
            "services": {
                "app": {},
                "db": {},
                "worker": {}
            }
        }));

        let services = ComposeActiveServiceSet::new(&model, "worker", &[]);

        assert_eq!(
            services.ordered_services_for_planning(),
            ["worker", "app", "db"]
        );
    }

    #[test]
    fn planning_input_filters_port_entries_to_active_services() {
        let model = model(json!({
            "services": {
                "app": {
                    "ports": [{"target": 3000, "published": "3000"}]
                },
                "db": {
                    "ports": [{"target": 5432, "published": "5432"}]
                }
            }
        }));
        let all_entries = vec![
            ComposePortEntry {
                service: "app".to_owned(),
                entry_index: 0,
                service_replica_count: 1,
                service_uses_host_network: false,
                syntax: ComposePortSyntax::EffectiveObject,
                target_port: Some(3000),
                published_host_port: ComposePublishedHostPort::Single(3000),
                host_ip: ComposePortHostIp::Omitted,
                protocol: ComposePortProtocol::Tcp,
                original_fields: BTreeMap::new(),
                eligibility: ComposePortEligibility::EligibleFixedTcp,
                unsupported_reason: None,
            },
            ComposePortEntry {
                service: "idle".to_owned(),
                entry_index: 0,
                service_replica_count: 1,
                service_uses_host_network: false,
                syntax: ComposePortSyntax::EffectiveObject,
                target_port: Some(9000),
                published_host_port: ComposePublishedHostPort::Single(9000),
                host_ip: ComposePortHostIp::Omitted,
                protocol: ComposePortProtocol::Tcp,
                original_fields: BTreeMap::new(),
                eligibility: ComposePortEligibility::EligibleFixedTcp,
                unsupported_reason: None,
            },
        ];

        let input = compose_published_port_planning_input(&model, &all_entries, "app", &[]);

        assert_eq!(input.port_entries.len(), 1);
        assert_eq!(input.port_entries[0].service, "app");
        assert_eq!(
            input.services.ordered_services_for_planning(),
            ["app", "db"]
        );
    }
}
