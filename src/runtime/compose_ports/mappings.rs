use crate::{
    config::{resolved::ResolvedComposePublishedPortMapping, types::PortProtocol},
    runtime::{
        compose_cli::ComposeConfigModel,
        compose_ports::{
            ComposePortEligibility, ComposePortProtocol, ComposePublishedPortDiagnostic,
            ComposePublishedPortHostIp, ComposePublishedPortMapping,
            ComposePublishedPortPlanningInput,
        },
    },
};

use super::endpoint::endpoint_for_entry;

pub(crate) fn resolve_compose_published_port_mappings(
    full_model: &ComposeConfigModel,
    input: &ComposePublishedPortPlanningInput,
    mappings: &[ResolvedComposePublishedPortMapping],
) -> Result<Vec<ComposePublishedPortMapping>, ComposePublishedPortDiagnostic> {
    let mut resolved = Vec::new();
    for mapping in mappings {
        if !full_model.has_service(&mapping.service) {
            return Err(mapping_invalid(format!(
                "Compose published port mapping references missing service `{}`; target: {}/tcp",
                mapping.service, mapping.target
            )));
        }
        if !input.services.contains(&mapping.service) {
            continue;
        }

        let protocol = compose_protocol(mapping.protocol);
        let matches = input
            .port_entries
            .iter()
            .filter(|entry| {
                entry.service == mapping.service
                    && entry.target_port == Some(mapping.target)
                    && entry.protocol == protocol
            })
            .collect::<Vec<_>>();
        let [entry] = matches.as_slice() else {
            let detail = if matches.is_empty() {
                format!(
                    "Compose published port mapping does not match a port entry for service `{}`, target {}/tcp",
                    mapping.service, mapping.target
                )
            } else {
                format!(
                    "Compose published port mapping is ambiguous because service `{}` has multiple port entries for target {}/tcp",
                    mapping.service, mapping.target
                )
            };
            return Err(mapping_invalid(detail));
        };
        if entry.eligibility != ComposePortEligibility::EligibleFixedTcp {
            return Err(mapping_invalid(format!(
                "Compose published port mapping requires one fixed TCP published port for service `{}`, target {}/tcp: {}",
                mapping.service,
                mapping.target,
                entry
                    .unsupported_reason
                    .as_deref()
                    .unwrap_or("port entry is not eligible for explicit mapping")
            )));
        }
        let requested =
            endpoint_for_entry(entry).map_err(ComposePublishedPortDiagnostic::from_plan_error)?;
        resolved.push(ComposePublishedPortMapping {
            service: mapping.service.clone(),
            port_entry_index: entry.entry_index,
            target_port: mapping.target,
            protocol,
            endpoint: crate::runtime::compose_ports::ComposePublishedPortEndpoint {
                host_ip: mapping
                    .host_ip
                    .clone()
                    .map_or(requested.host_ip, ComposePublishedPortHostIp::Explicit),
                host_port: mapping.host,
            },
        });
    }
    Ok(resolved)
}

const fn compose_protocol(protocol: PortProtocol) -> ComposePortProtocol {
    match protocol {
        PortProtocol::Tcp => ComposePortProtocol::Tcp,
        PortProtocol::Udp => ComposePortProtocol::Udp,
    }
}

const fn mapping_invalid(detail: String) -> ComposePublishedPortDiagnostic {
    ComposePublishedPortDiagnostic::MappingInvalid { detail }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{
        config::resolved::ResolvedComposePublishedPortMapping,
        runtime::compose_ports::{
            ComposePublishedPortEndpoint, ComposePublishedPortHostIp,
            diagnostics::COMPOSE_PUBLISHED_PORT_MAPPING_INVALID,
            test_support::{model, planning_input},
        },
    };

    fn mapping(host: u16, host_ip: Option<&str>) -> ResolvedComposePublishedPortMapping {
        ResolvedComposePublishedPortMapping {
            service: "app".to_owned(),
            target: 502,
            protocol: PortProtocol::Tcp,
            host,
            host_ip: host_ip.map(str::to_owned),
        }
    }

    #[test]
    fn mapping_resolves_by_service_protocol_and_target_and_inherits_host_ip() {
        let value = json!({
            "services": {
                "app": {
                    "ports": [{"host_ip": "127.0.0.1", "target": 502, "published": "502"}]
                }
            }
        });
        let full_model = model(value.clone());
        let input = planning_input(value, "app", &[]);

        let inherited =
            resolve_compose_published_port_mappings(&full_model, &input, &[mapping(1502, None)])
                .unwrap();
        let overridden = resolve_compose_published_port_mappings(
            &full_model,
            &input,
            &[mapping(2502, Some("0.0.0.0"))],
        )
        .unwrap();

        assert_eq!(inherited[0].port_entry_index, 0);
        assert_eq!(
            inherited[0].endpoint,
            ComposePublishedPortEndpoint {
                host_ip: ComposePublishedPortHostIp::Explicit("127.0.0.1".to_owned()),
                host_port: 1502,
            }
        );
        assert_eq!(
            overridden[0].endpoint,
            ComposePublishedPortEndpoint {
                host_ip: ComposePublishedPortHostIp::Explicit("0.0.0.0".to_owned()),
                host_port: 2502,
            }
        );
    }

    #[test]
    fn mapping_for_inactive_existing_service_is_ignored() {
        let full_value = json!({
            "services": {
                "app": {"ports": [{"target": 502, "published": "502"}]},
                "worker": {"ports": [{"target": 502, "published": "502"}]}
            }
        });
        let active_value = json!({
            "services": {
                "app": {"ports": [{"target": 502, "published": "502"}]}
            }
        });
        let full_model = model(full_value);
        let input = planning_input(active_value, "app", &[]);
        let mapping = ResolvedComposePublishedPortMapping {
            service: "worker".to_owned(),
            target: 502,
            protocol: PortProtocol::Tcp,
            host: 1502,
            host_ip: None,
        };

        let resolved =
            resolve_compose_published_port_mappings(&full_model, &input, &[mapping]).unwrap();

        assert!(resolved.is_empty());
    }

    #[test]
    fn mapping_rejects_missing_service_and_ambiguous_port_identity() {
        let value = json!({
            "services": {
                "app": {
                    "ports": [
                        {"host_ip": "127.0.0.1", "target": 502, "published": "502"},
                        {"host_ip": "0.0.0.0", "target": 502, "published": "1502"}
                    ]
                }
            }
        });
        let full_model = model(value.clone());
        let input = planning_input(value, "app", &[]);

        let ambiguous =
            resolve_compose_published_port_mappings(&full_model, &input, &[mapping(2502, None)])
                .expect_err("duplicate port identity should be ambiguous")
                .to_string();
        let missing = resolve_compose_published_port_mappings(
            &full_model,
            &input,
            &[ResolvedComposePublishedPortMapping {
                service: "missing".to_owned(),
                ..mapping(2502, None)
            }],
        )
        .expect_err("missing service should fail")
        .to_string();

        assert!(ambiguous.contains(COMPOSE_PUBLISHED_PORT_MAPPING_INVALID));
        assert!(ambiguous.contains("ambiguous"));
        assert!(missing.contains(COMPOSE_PUBLISHED_PORT_MAPPING_INVALID));
        assert!(missing.contains("missing service `missing`"));
    }
}
