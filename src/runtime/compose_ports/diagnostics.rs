use crate::runtime::compose_ports::{
    ComposePortEligibility, ComposePortProtocol, ComposePublishedPortEndpoint,
    ComposePublishedPortPlanningInput, compose_port_protocol_name,
    compose_published_port_endpoint_display,
};

pub(crate) use crate::config::schema::COMPOSE_PUBLISHED_PORT_MAPPING_INVALID;

use super::endpoint::{endpoint_for_entry, target_port_for_entry};
use super::planning::ComposePublishedPortPlanError;

pub(crate) const COMPOSE_PUBLISHED_PORT_MULTI_REPLICA_UNSUPPORTED: &str =
    "compose_published_port_multi_replica_unsupported";
pub(crate) const COMPOSE_PUBLISHED_PORT_UNSUPPORTED: &str = "compose_published_port_unsupported";
pub(crate) const COMPOSE_PUBLISHED_PORT_INVALID: &str = "compose_published_port_invalid";
pub(crate) const COMPOSE_PUBLISHED_PORT_COLLISION: &str = "compose_published_port_collision";
pub(crate) const COMPOSE_PUBLISHED_PORT_AUTOMATIC_RELOCATION_FAILED: &str =
    "compose_published_port_automatic_relocation_failed";
pub(crate) const COMPOSE_PUBLISHED_PORT_BIND_RACE: &str = "compose_published_port_bind_race";
pub(crate) const COMPOSE_PUBLISHED_PORT_MAPPING_CONFLICT: &str =
    "compose_published_port_mapping_conflict";

#[derive(Debug)]
pub(crate) enum ComposePublishedPortDiagnostic {
    MultiReplicaUnsupported {
        service: String,
        replica_count: u64,
        requested: ComposePublishedPortEndpoint,
        target_port: u16,
        protocol: ComposePortProtocol,
    },
    Collision {
        service: String,
        requested: ComposePublishedPortEndpoint,
        target_port: u16,
        protocol: ComposePortProtocol,
    },
    BindRace {
        service: String,
        requested: ComposePublishedPortEndpoint,
        planned: ComposePublishedPortEndpoint,
        target_port: u16,
        protocol: ComposePortProtocol,
    },
    Unsupported {
        service: String,
        port_entry_index: usize,
        requested: String,
        target_port: u16,
        protocol: ComposePortProtocol,
        reason: String,
    },
    Invalid {
        detail: String,
    },
    MappingInvalid {
        detail: String,
    },
    MappingConflict {
        detail: String,
    },
    AutomaticRelocationFailed {
        detail: String,
    },
}

impl ComposePublishedPortDiagnostic {
    pub(crate) fn from_plan_error(error: ComposePublishedPortPlanError) -> Self {
        match error {
            ComposePublishedPortPlanError::NoAutomaticRelocationCandidate {
                service,
                port_entry_index,
                requested,
            } => Self::AutomaticRelocationFailed {
                detail: format!(
                    "No automatic relocation candidate is available for service `{service}` port entry {port_entry_index} requested endpoint {}",
                    compose_published_port_endpoint_display(&requested)
                ),
            },
            ComposePublishedPortPlanError::HostPortAvailability {
                host_ip,
                host_port,
                source,
            } => Self::Invalid {
                detail: format!(
                    "Failed to check Compose published port availability for {host_ip}:{host_port}: {source:#}"
                ),
            },
            ComposePublishedPortPlanError::InconsistentEntry {
                service,
                port_entry_index,
                detail,
            } => Self::Invalid {
                detail: format!(
                    "Internal Compose published port state is inconsistent for service `{service}` port entry {port_entry_index}: {detail}"
                ),
            },
            ComposePublishedPortPlanError::MappingConflict { detail } => {
                Self::MappingConflict { detail }
            }
        }
    }
}

impl std::fmt::Display for ComposePublishedPortDiagnostic {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MultiReplicaUnsupported {
                service,
                replica_count,
                requested,
                target_port,
                protocol,
            } => write!(
                formatter,
                "{COMPOSE_PUBLISHED_PORT_MULTI_REPLICA_UNSUPPORTED}: Docker Compose service `{service}` has {replica_count} replicas and a fixed Compose published port. requested: {}; target: {service}:{target_port}/{}; source: compose. decune does not allocate separate published host ports per replica. Suggested actions: use a container-only Compose port, split replicas into explicit services with separate ports, use a Compose port range, or set the replica count to 1.",
                compose_published_port_endpoint_display(requested),
                compose_port_protocol_name(protocol)
            ),
            Self::Collision {
                service,
                requested,
                target_port,
                protocol,
            } => write!(
                formatter,
                "{COMPOSE_PUBLISHED_PORT_COLLISION}: Compose published port collision. service: `{service}`; requested: {}; target: {service}:{target_port}/{}; source: compose. The requested Docker/Compose published host port is unavailable. This is not a decune forwarding listener. decune automatic forwarding does not replace Compose published ports. Suggested actions: stop the process, Docker container, or workspace using the requested endpoint; change the Compose published port; use a container-only Compose port when appropriate; or enable automatic published port relocation explicitly.",
                compose_published_port_endpoint_display(requested),
                compose_port_protocol_name(protocol)
            ),
            Self::BindRace {
                service,
                requested,
                planned,
                target_port,
                protocol,
            } => write!(
                formatter,
                "{COMPOSE_PUBLISHED_PORT_BIND_RACE}: Compose published port bind race. service: `{service}`; requested: {}; planned: {}; target: {service}:{target_port}/{}; source: compose. The planned Docker/Compose published host port was selected by decune before startup, but Docker Compose failed to bind it. Another process may have taken it concurrently. decune does not retry Compose startup automatically.",
                compose_published_port_endpoint_display(requested),
                compose_published_port_endpoint_display(planned),
                compose_port_protocol_name(protocol)
            ),
            Self::Unsupported {
                service,
                port_entry_index,
                requested,
                target_port,
                protocol,
                reason,
            } => write!(
                formatter,
                "{COMPOSE_PUBLISHED_PORT_UNSUPPORTED}: Compose published port startup failure involves an unsupported port entry. service: `{service}`; port entry: {port_entry_index}; requested: {requested}; target: {service}:{target_port}/{}; source: compose. {reason}. decune does not relocate this Compose published port entry.",
                compose_port_protocol_name(protocol)
            ),
            Self::Invalid { detail } => {
                write!(formatter, "{COMPOSE_PUBLISHED_PORT_INVALID}: {detail}")
            }
            Self::MappingInvalid { detail } => {
                write!(
                    formatter,
                    "{COMPOSE_PUBLISHED_PORT_MAPPING_INVALID}: {detail}"
                )
            }
            Self::MappingConflict { detail } => write!(
                formatter,
                "{COMPOSE_PUBLISHED_PORT_MAPPING_CONFLICT}: {detail}"
            ),
            Self::AutomaticRelocationFailed { detail } => write!(
                formatter,
                "{COMPOSE_PUBLISHED_PORT_AUTOMATIC_RELOCATION_FAILED}: {detail}"
            ),
        }
    }
}

impl std::error::Error for ComposePublishedPortDiagnostic {}

pub(crate) fn validate_compose_published_port_diagnostics(
    input: &ComposePublishedPortPlanningInput,
) -> std::result::Result<(), ComposePublishedPortDiagnostic> {
    for entry in &input.port_entries {
        if entry.service_uses_host_network
            || entry.service_replica_count <= 1
            || entry.eligibility != ComposePortEligibility::EligibleFixedTcp
        {
            continue;
        }

        let target_port = target_port_for_entry(entry)
            .map_err(ComposePublishedPortDiagnostic::from_plan_error)?;
        let requested =
            endpoint_for_entry(entry).map_err(ComposePublishedPortDiagnostic::from_plan_error)?;
        return Err(ComposePublishedPortDiagnostic::MultiReplicaUnsupported {
            service: entry.service.clone(),
            replica_count: entry.service_replica_count,
            requested,
            target_port,
            protocol: entry.protocol.clone(),
        });
    }

    Ok(())
}

pub(crate) fn compose_published_port_invalid_config_error(
    project_name: &str,
    stderr: &str,
) -> ComposePublishedPortDiagnostic {
    ComposePublishedPortDiagnostic::Invalid {
        detail: format!(
            "Docker Compose project {project_name} contains an invalid Compose published port configuration: {}",
            stderr.trim()
        ),
    }
}
#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::runtime::compose_ports::{
        planning::plan_compose_published_ports_with,
        test_support::{plan_with_availability, planning_input},
    };

    #[test]
    fn planner_availability_error_maps_to_invalid_diagnostic() {
        let input = planning_input(
            json!({
                "services": {
                    "app": {
                        "ports": [{"target": 3000, "published": "3000"}]
                    }
                }
            }),
            "app",
            &[],
        );

        let error = plan_compose_published_ports_with(&input, true, &[], &[], |_, _| {
            Err(anyhow::anyhow!("socket probe failed"))
        })
        .expect_err("availability error should be returned");
        let diagnostic = ComposePublishedPortDiagnostic::from_plan_error(error).to_string();

        assert!(diagnostic.contains(COMPOSE_PUBLISHED_PORT_INVALID));
        assert!(diagnostic.contains("socket probe failed"));
        assert!(!diagnostic.contains("compose_published_port_collision"));
    }

    #[test]
    fn missing_candidate_maps_to_automatic_relocation_failed_diagnostic() {
        let diagnostic = ComposePublishedPortDiagnostic::from_plan_error(
            ComposePublishedPortPlanError::NoAutomaticRelocationCandidate {
                service: "app".to_owned(),
                port_entry_index: 0,
                requested: ComposePublishedPortEndpoint {
                    host_ip: crate::runtime::compose_ports::ComposePublishedPortHostIp::Omitted,
                    host_port: 3000,
                },
            },
        )
        .to_string();

        assert!(diagnostic.contains(COMPOSE_PUBLISHED_PORT_AUTOMATIC_RELOCATION_FAILED));
        assert!(diagnostic.contains("No automatic relocation candidate"));
        assert!(!diagnostic.contains("compose_published_port_relocation_failed"));
    }

    #[test]
    fn validates_multi_replica_fixed_tcp_published_port_as_unsupported() {
        let input = planning_input(
            json!({
                "services": {
                    "app": {
                        "scale": 2,
                        "ports": [{"target": 3000, "published": "3000"}]
                    }
                }
            }),
            "app",
            &[],
        );

        let error = validate_compose_published_port_diagnostics(&input)
            .expect_err("multi-replica fixed published port must fail");
        let message = error.to_string();

        assert!(message.contains(COMPOSE_PUBLISHED_PORT_MULTI_REPLICA_UNSUPPORTED));
        assert!(message.contains("service `app`"));
        assert!(message.contains("2 replicas"));
        assert!(message.contains("<host_ip omitted>:3000"));
        assert!(message.contains("app:3000/tcp"));
        assert!(message.contains("container-only Compose port"));
    }

    #[test]
    fn multi_replica_policy_errors_on_inconsistent_eligible_entry() {
        let mut input = planning_input(
            json!({
                "services": {
                    "app": {
                        "scale": 2,
                        "ports": [{"target": 3000, "published": "3000"}]
                    }
                }
            }),
            "app",
            &[],
        );
        input.port_entries[0].target_port = None;

        let error = validate_compose_published_port_diagnostics(&input)
            .expect_err("inconsistent eligible entry must fail");
        let message = error.to_string();

        assert!(message.contains(COMPOSE_PUBLISHED_PORT_INVALID));
        assert!(message.contains("service `app` port entry 0"));
        assert!(message.contains("target port"));
    }

    #[test]
    fn multi_replica_policy_ignores_unsupported_and_host_network_entries() {
        let input = planning_input(
            json!({
                "services": {
                    "app": {
                        "deploy": {"replicas": 3},
                        "ports": [
                            {"target": 3000},
                            {"target": 8125, "published": "8125", "protocol": "udp"},
                            {"target": 3001, "published": "3001-3003"}
                        ]
                    },
                    "debug": {
                        "scale": 2,
                        "network_mode": "host",
                        "ports": [{"target": 4000, "published": "4000"}]
                    }
                }
            }),
            "app",
            &[],
        );

        validate_compose_published_port_diagnostics(&input).unwrap();
        assert!(plan_with_availability(&input, &[]).entries.is_empty());
    }
}
