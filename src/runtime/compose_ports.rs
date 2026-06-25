use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map as JsonMap, Value as JsonValue};

use crate::runtime::compose_cli::ComposeConfigModel;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposePortEntry {
    pub(crate) service: String,
    pub(crate) entry_index: usize,
    pub(crate) syntax: ComposePortSyntax,
    pub(crate) target_port: Option<u16>,
    pub(crate) published_host_port: ComposePublishedHostPort,
    pub(crate) host_ip: ComposePortHostIp,
    pub(crate) protocol: ComposePortProtocol,
    pub(crate) original_fields: BTreeMap<String, JsonValue>,
    pub(crate) eligibility: ComposePortEligibility,
    pub(crate) unsupported_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ComposePortSyntax {
    EffectiveObject,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ComposePublishedHostPort {
    Single(u16),
    None,
    Range(String),
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ComposePortHostIp {
    Omitted,
    Explicit(String),
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ComposePortProtocol {
    Tcp,
    Udp,
    Other(String),
    Invalid(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ComposePortEligibility {
    EligibleFixedTcp,
    UnsupportedContainerOnly,
    UnsupportedUdp,
    UnsupportedRange,
    UnsupportedInvalid,
    UnsupportedOther,
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposePublishedPortPlanningInput {
    pub(crate) services: ComposeActiveServiceSet,
    pub(crate) port_entries: Vec<ComposePortEntry>,
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

pub(crate) fn classify_compose_published_ports(
    model: &ComposeConfigModel,
) -> Vec<ComposePortEntry> {
    model
        .services()
        .flat_map(|(service_name, service)| {
            service
                .ports
                .iter()
                .enumerate()
                .map(|(entry_index, value)| {
                    classify_port_entry(service_name.clone(), entry_index, value)
                })
        })
        .collect()
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

fn classify_port_entry(service: String, entry_index: usize, value: &JsonValue) -> ComposePortEntry {
    let Some(fields) = value.as_object() else {
        return ComposePortEntry {
            service,
            entry_index,
            syntax: ComposePortSyntax::EffectiveObject,
            target_port: None,
            published_host_port: ComposePublishedHostPort::Invalid("non-object".to_owned()),
            host_ip: ComposePortHostIp::Omitted,
            protocol: ComposePortProtocol::Tcp,
            original_fields: BTreeMap::new(),
            eligibility: ComposePortEligibility::UnsupportedInvalid,
            unsupported_reason: Some("Compose port entry is not an object".to_owned()),
        };
    };

    let target_port = parse_target_port(fields.get("target"));
    let published_host_port = parse_published_host_port(fields.get("published"));
    let host_ip = parse_host_ip(fields.get("host_ip"));
    let protocol = parse_protocol(fields.get("protocol"));
    let original_fields = clone_fields(fields);
    let (eligibility, unsupported_reason) =
        classify_eligibility(&target_port, &published_host_port, &host_ip, &protocol);

    ComposePortEntry {
        service,
        entry_index,
        syntax: ComposePortSyntax::EffectiveObject,
        target_port: target_port.ok(),
        published_host_port,
        host_ip,
        protocol,
        original_fields,
        eligibility,
        unsupported_reason,
    }
}

fn parse_target_port(value: Option<&JsonValue>) -> Result<u16, String> {
    let Some(value) = value else {
        return Err("target port is missing".to_owned());
    };
    parse_port_value(value, "target port")
}

fn parse_published_host_port(value: Option<&JsonValue>) -> ComposePublishedHostPort {
    let Some(value) = value else {
        return ComposePublishedHostPort::None;
    };
    match value {
        JsonValue::String(value) if value.contains('-') => {
            ComposePublishedHostPort::Range(value.clone())
        }
        _ => match parse_port_value(value, "published host port") {
            Ok(port) => ComposePublishedHostPort::Single(port),
            Err(reason) => ComposePublishedHostPort::Invalid(reason),
        },
    }
}

fn parse_host_ip(value: Option<&JsonValue>) -> ComposePortHostIp {
    match value {
        None | Some(JsonValue::Null) => ComposePortHostIp::Omitted,
        Some(JsonValue::String(value)) => ComposePortHostIp::Explicit(value.clone()),
        Some(value) => ComposePortHostIp::Invalid(format!("host_ip must be a string: {value}")),
    }
}

fn parse_protocol(value: Option<&JsonValue>) -> ComposePortProtocol {
    match value {
        None | Some(JsonValue::Null) => ComposePortProtocol::Tcp,
        Some(JsonValue::String(value)) => match value.as_str() {
            "tcp" => ComposePortProtocol::Tcp,
            "udp" => ComposePortProtocol::Udp,
            other => ComposePortProtocol::Other(other.to_owned()),
        },
        Some(value) => ComposePortProtocol::Invalid(format!("protocol must be a string: {value}")),
    }
}

fn parse_port_value(value: &JsonValue, field: &str) -> Result<u16, String> {
    match value {
        JsonValue::Number(number) => {
            let Some(port) = number.as_u64() else {
                return Err(format!("{field} must be an unsigned integer: {number}"));
            };
            let port =
                u16::try_from(port).map_err(|_| format!("{field} is out of range: {port}"))?;
            validate_nonzero_port(port, field)
        }
        JsonValue::String(value) => {
            if value.contains('-') {
                return Err(format!("{field} is a range: {value}"));
            }
            let port = value
                .parse::<u16>()
                .map_err(|_| format!("{field} is not a valid port: {value}"))?;
            validate_nonzero_port(port, field)
        }
        other => Err(format!("{field} must be a number or string: {other}")),
    }
}

fn validate_nonzero_port(port: u16, field: &str) -> Result<u16, String> {
    if port == 0 {
        return Err(format!("{field} must be greater than 0: {port}"));
    }
    Ok(port)
}

fn classify_eligibility(
    target_port: &Result<u16, String>,
    published_host_port: &ComposePublishedHostPort,
    host_ip: &ComposePortHostIp,
    protocol: &ComposePortProtocol,
) -> (ComposePortEligibility, Option<String>) {
    if let Err(reason) = target_port {
        return invalid(reason);
    }
    if let ComposePortHostIp::Invalid(reason) = host_ip {
        return invalid(reason);
    }
    if let ComposePortProtocol::Invalid(reason) = protocol {
        return invalid(reason);
    }
    match published_host_port {
        ComposePublishedHostPort::Invalid(reason) => return invalid(reason),
        ComposePublishedHostPort::Range(range) => {
            return (
                ComposePortEligibility::UnsupportedRange,
                Some(format!("Published host port range is unsupported: {range}")),
            );
        }
        ComposePublishedHostPort::None => {
            return (
                ComposePortEligibility::UnsupportedContainerOnly,
                Some("Container-only Compose port is not relocation-eligible".to_owned()),
            );
        }
        ComposePublishedHostPort::Single(_) => {}
    }
    match protocol {
        ComposePortProtocol::Tcp => (ComposePortEligibility::EligibleFixedTcp, None),
        ComposePortProtocol::Udp => (
            ComposePortEligibility::UnsupportedUdp,
            Some("UDP Compose published ports are not relocation-eligible".to_owned()),
        ),
        ComposePortProtocol::Other(protocol) => (
            ComposePortEligibility::UnsupportedOther,
            Some(format!(
                "Compose published port protocol is not supported for relocation: {protocol}"
            )),
        ),
        ComposePortProtocol::Invalid(_) => unreachable!("invalid protocol handled above"),
    }
}

fn invalid(reason: &str) -> (ComposePortEligibility, Option<String>) {
    (
        ComposePortEligibility::UnsupportedInvalid,
        Some(reason.to_owned()),
    )
}

fn clone_fields(fields: &JsonMap<String, JsonValue>) -> BTreeMap<String, JsonValue> {
    fields
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn model(value: JsonValue) -> ComposeConfigModel {
        serde_json::from_value(value).unwrap()
    }

    fn entries(value: JsonValue) -> Vec<ComposePortEntry> {
        classify_compose_published_ports(&model(value))
    }

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

    #[test]
    fn classifies_fixed_tcp_published_port_with_omitted_host_ip() {
        let ports = entries(json!({
            "services": {
                "app": {
                    "ports": [{
                        "mode": "ingress",
                        "target": 3000,
                        "published": "3000",
                        "protocol": "tcp"
                    }]
                }
            }
        }));

        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].service, "app");
        assert_eq!(ports[0].entry_index, 0);
        assert_eq!(ports[0].target_port, Some(3000));
        assert_eq!(
            ports[0].published_host_port,
            ComposePublishedHostPort::Single(3000)
        );
        assert_eq!(ports[0].host_ip, ComposePortHostIp::Omitted);
        assert_eq!(ports[0].protocol, ComposePortProtocol::Tcp);
        assert_eq!(
            ports[0].eligibility,
            ComposePortEligibility::EligibleFixedTcp
        );
        assert_eq!(ports[0].unsupported_reason, None);
    }

    #[test]
    fn distinguishes_explicit_loopback_wildcard_and_ipv6_host_ips() {
        let ports = entries(json!({
            "services": {
                "app": {
                    "ports": [
                        {"host_ip": "127.0.0.1", "target": 3000, "published": "3000"},
                        {"host_ip": "0.0.0.0", "target": 3001, "published": "3001"},
                        {"host_ip": "::1", "target": 3002, "published": "3002"}
                    ]
                }
            }
        }));

        assert_eq!(
            ports
                .iter()
                .map(|port| port.host_ip.clone())
                .collect::<Vec<_>>(),
            vec![
                ComposePortHostIp::Explicit("127.0.0.1".to_owned()),
                ComposePortHostIp::Explicit("0.0.0.0".to_owned()),
                ComposePortHostIp::Explicit("::1".to_owned()),
            ]
        );
        assert!(
            ports
                .iter()
                .all(|port| port.eligibility == ComposePortEligibility::EligibleFixedTcp)
        );
    }

    #[test]
    fn preserves_long_syntax_metadata_fields() {
        let ports = entries(json!({
            "services": {
                "app": {
                    "ports": [{
                        "name": "web",
                        "mode": "host",
                        "target": 3000,
                        "published": "3000",
                        "protocol": "tcp",
                        "app_protocol": "http"
                    }]
                }
            }
        }));

        assert_eq!(ports[0].original_fields.get("name"), Some(&json!("web")));
        assert_eq!(ports[0].original_fields.get("mode"), Some(&json!("host")));
        assert_eq!(
            ports[0].original_fields.get("app_protocol"),
            Some(&json!("http"))
        );
    }

    #[test]
    fn treats_omitted_protocol_as_tcp() {
        let ports = entries(json!({
            "services": {
                "app": {
                    "ports": [{"target": 3000, "published": "3000"}]
                }
            }
        }));

        assert_eq!(ports[0].protocol, ComposePortProtocol::Tcp);
        assert_eq!(
            ports[0].eligibility,
            ComposePortEligibility::EligibleFixedTcp
        );
    }

    #[test]
    fn classifies_udp_and_other_protocols_as_unsupported() {
        let ports = entries(json!({
            "services": {
                "app": {
                    "ports": [
                        {"target": 8125, "published": "8125", "protocol": "udp"},
                        {"target": 8126, "published": "8126", "protocol": "sctp"}
                    ]
                }
            }
        }));

        assert_eq!(ports[0].protocol, ComposePortProtocol::Udp);
        assert_eq!(ports[0].eligibility, ComposePortEligibility::UnsupportedUdp);
        assert_eq!(
            ports[1].protocol,
            ComposePortProtocol::Other("sctp".to_owned())
        );
        assert_eq!(
            ports[1].eligibility,
            ComposePortEligibility::UnsupportedOther
        );
    }

    #[test]
    fn classifies_container_only_ports_as_unsupported() {
        let ports = entries(json!({
            "services": {
                "app": {
                    "ports": [{"target": 3000, "protocol": "tcp"}]
                }
            }
        }));

        assert_eq!(ports[0].published_host_port, ComposePublishedHostPort::None);
        assert_eq!(
            ports[0].eligibility,
            ComposePortEligibility::UnsupportedContainerOnly
        );
    }

    #[test]
    fn classifies_ephemeral_published_zero_as_unsupported_invalid() {
        let ports = entries(json!({
            "services": {
                "app": {
                    "ports": [
                        {"target": 3000, "published": "0"},
                        {"target": 3001, "published": 0}
                    ]
                }
            }
        }));

        assert_eq!(ports.len(), 2);
        for port in ports {
            assert!(matches!(
                port.published_host_port,
                ComposePublishedHostPort::Invalid(_)
            ));
            assert_eq!(port.eligibility, ComposePortEligibility::UnsupportedInvalid);
            let reason = port
                .unsupported_reason
                .as_deref()
                .expect("expected unsupported reason");
            assert!(reason.contains("published host port"));
            assert!(reason.contains("greater than 0"));
            assert!(reason.contains('0'));
        }
    }

    #[test]
    fn classifies_effective_range_strings_as_unsupported_range() {
        let ports = entries(json!({
            "services": {
                "app": {
                    "ports": [
                        {"target": 3000, "published": "3000-3005"},
                        {"target": "3000-3005", "published": "3000"}
                    ]
                }
            }
        }));

        assert_eq!(
            ports[0].published_host_port,
            ComposePublishedHostPort::Range("3000-3005".to_owned())
        );
        assert_eq!(
            ports[0].eligibility,
            ComposePortEligibility::UnsupportedRange
        );
        assert_eq!(
            ports[1].eligibility,
            ComposePortEligibility::UnsupportedInvalid
        );
    }

    #[test]
    fn classifies_invalid_values_as_unsupported_invalid() {
        let ports = entries(json!({
            "services": {
                "app": {
                    "ports": [
                        "3000:3000",
                        {"target": "not-a-port", "published": "3000"},
                        {"target": 3000, "published": "invalid"},
                        {"target": 3000, "published": "3000", "host_ip": 127001},
                        {"target": 3000, "published": "3000", "protocol": 123}
                    ]
                }
            }
        }));

        assert!(
            ports
                .iter()
                .all(|port| port.eligibility == ComposePortEligibility::UnsupportedInvalid)
        );
    }

    #[test]
    fn preserves_service_order_and_entry_identity() {
        let ports = entries(json!({
            "services": {
                "web": {
                    "ports": [
                        {"target": 3000, "published": "3000"},
                        {"target": 3001, "published": "3001"}
                    ]
                },
                "admin": {
                    "ports": [{"target": 4000, "published": "4000"}]
                }
            }
        }));

        assert_eq!(
            ports
                .iter()
                .map(|port| (port.service.as_str(), port.entry_index))
                .collect::<Vec<_>>(),
            vec![("admin", 0), ("web", 0), ("web", 1)]
        );
    }
}
