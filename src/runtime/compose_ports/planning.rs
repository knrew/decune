use std::collections::BTreeMap;

use crate::{
    docker::ports::{
        HostPortReservation, ResolvedForwardPort, host_port_available,
        host_port_reservations_conflict, resolved_forward_port_reservations,
    },
    runtime::compose_ports::{
        ComposePortEligibility, ComposePortEntry, ComposePublishedPortAllocationReason,
        ComposePublishedPortEndpoint, ComposePublishedPortPlan, ComposePublishedPortPlanEntry,
        ComposePublishedPortPlanEntryType, ComposePublishedPortPlanSource,
        ComposePublishedPortPlanningInput, ComposePublishedPortReservation,
        ComposePublishedPortReservationSource,
    },
};

use super::endpoint::{
    endpoint_for_entry, host_ip_display_value, protocol_order, requested_host_port,
    reservation_host_ip, target_port_for_entry,
};

#[derive(Debug)]
pub(crate) enum ComposePublishedPortPlanError {
    NoRelocationCandidate {
        service: String,
        port_entry_index: usize,
        requested: ComposePublishedPortEndpoint,
    },
    HostPortAvailability {
        host_ip: String,
        host_port: u16,
        source: anyhow::Error,
    },
    InconsistentEntry {
        service: String,
        port_entry_index: usize,
        detail: String,
    },
}

impl std::fmt::Display for ComposePublishedPortPlanError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoRelocationCandidate {
                service,
                port_entry_index,
                requested,
            } => write!(
                formatter,
                "No Compose published port relocation candidate is available for service `{service}` port entry {port_entry_index} requested host port {}",
                requested.host_port
            ),
            Self::HostPortAvailability {
                host_ip,
                host_port,
                source,
            } => write!(
                formatter,
                "Failed to check Compose published port availability for {host_ip}:{host_port}: {source:#}"
            ),
            Self::InconsistentEntry {
                service,
                port_entry_index,
                detail,
            } => write!(
                formatter,
                "Internal Compose published port invariant failed for service `{service}` port entry {port_entry_index}: {detail}"
            ),
        }
    }
}

impl std::error::Error for ComposePublishedPortPlanError {}

pub(crate) fn plan_compose_published_ports_with_existing_project(
    input: &ComposePublishedPortPlanningInput,
    relocation_enabled: bool,
    existing_forward_ports: &[ResolvedForwardPort],
    existing_project_published_ports: &[ComposePublishedPortReservation],
) -> std::result::Result<ComposePublishedPortPlan, ComposePublishedPortPlanError> {
    plan_compose_published_ports_with(
        input,
        relocation_enabled,
        existing_forward_ports,
        existing_project_published_ports,
        host_port_available,
    )
}

pub(crate) fn plan_compose_published_ports_with<F>(
    input: &ComposePublishedPortPlanningInput,
    relocation_enabled: bool,
    existing_forward_ports: &[ResolvedForwardPort],
    existing_project_published_ports: &[ComposePublishedPortReservation],
    mut host_port_available: F,
) -> std::result::Result<ComposePublishedPortPlan, ComposePublishedPortPlanError>
where
    F: FnMut(&str, u16) -> anyhow::Result<bool>,
{
    if !relocation_enabled {
        return Ok(ComposePublishedPortPlan::default());
    }

    let mut reservations =
        resolved_forward_port_reservations(existing_forward_ports).collect::<Vec<_>>();
    let mut plan_entries = Vec::new();

    for entry in ordered_eligible_port_entries(input) {
        let requested = endpoint_for_entry(entry)?;
        let target_port = target_port_for_entry(entry)?;
        let requested_host_ip = reservation_host_ip(&requested);
        let existing_endpoint = existing_project_published_port_candidate(
            existing_project_published_ports,
            entry,
            &requested,
            &reservations,
            &mut host_port_available,
        )?;
        let requested_reserved =
            host_port_reservations_conflict(&reservations, requested_host_ip, requested.host_port);
        let (planned, allocation_reason) = if let Some(existing_endpoint) = existing_endpoint {
            let allocation_reason = if existing_endpoint == requested {
                ComposePublishedPortAllocationReason::Available
            } else {
                ComposePublishedPortAllocationReason::Unavailable
            };
            (existing_endpoint, allocation_reason)
        } else {
            let requested_available = if requested_reserved {
                false
            } else {
                host_port_available(requested_host_ip, requested.host_port).map_err(|source| {
                    ComposePublishedPortPlanError::HostPortAvailability {
                        host_ip: requested_host_ip.to_owned(),
                        host_port: requested.host_port,
                        source,
                    }
                })?
            };
            if requested_available {
                (
                    requested.clone(),
                    ComposePublishedPortAllocationReason::Available,
                )
            } else {
                let allocation_reason = if requested_reserved {
                    ComposePublishedPortAllocationReason::Reserved
                } else {
                    ComposePublishedPortAllocationReason::Unavailable
                };
                let planned_host_port = allocate_relocated_host_port(
                    entry,
                    &requested,
                    requested_host_ip,
                    &reservations,
                    &mut host_port_available,
                )?;
                (
                    ComposePublishedPortEndpoint {
                        host_ip: requested.host_ip.clone(),
                        host_port: planned_host_port,
                    },
                    allocation_reason,
                )
            }
        };
        reservations.push(HostPortReservation {
            host_ip: reservation_host_ip(&planned).to_owned(),
            host: planned.host_port,
        });
        let relocated = requested.host_port != planned.host_port;

        plan_entries.push(ComposePublishedPortPlanEntry {
            service: entry.service.clone(),
            port_entry_index: entry.entry_index,
            source: ComposePublishedPortPlanSource::Compose,
            kind: ComposePublishedPortPlanEntryType::Published,
            target_port,
            protocol: entry.protocol.clone(),
            requested,
            planned,
            relocated,
            allocation_reason,
        });
    }

    Ok(ComposePublishedPortPlan {
        entries: plan_entries,
    })
}

pub(crate) fn compose_published_port_plan_has_relocations(plan: &ComposePublishedPortPlan) -> bool {
    plan.entries.iter().any(|entry| entry.relocated)
}

pub(crate) fn compose_published_port_runtime_plan(
    input: &ComposePublishedPortPlanningInput,
    planned: &ComposePublishedPortPlan,
) -> std::result::Result<ComposePublishedPortPlan, ComposePublishedPortPlanError> {
    let planned_entries = planned
        .entries
        .iter()
        .map(|entry| ((entry.service.as_str(), entry.port_entry_index), entry))
        .collect::<BTreeMap<_, _>>();
    let mut entries = Vec::new();
    for entry in input
        .port_entries
        .iter()
        .filter(|entry| entry.eligibility == ComposePortEligibility::EligibleFixedTcp)
    {
        let requested = endpoint_for_entry(entry)?;
        let target_port = target_port_for_entry(entry)?;
        if let Some(planned) = planned_entries.get(&(entry.service.as_str(), entry.entry_index)) {
            entries.push((*planned).clone());
            continue;
        }
        entries.push(ComposePublishedPortPlanEntry {
            service: entry.service.clone(),
            port_entry_index: entry.entry_index,
            source: ComposePublishedPortPlanSource::Compose,
            kind: ComposePublishedPortPlanEntryType::Published,
            target_port,
            protocol: entry.protocol.clone(),
            requested: requested.clone(),
            planned: requested,
            relocated: false,
            allocation_reason: ComposePublishedPortAllocationReason::Available,
        });
    }

    Ok(ComposePublishedPortPlan { entries })
}

pub(super) fn ordered_eligible_port_entries(
    input: &ComposePublishedPortPlanningInput,
) -> Vec<&ComposePortEntry> {
    let service_order = input
        .services
        .ordered_services_for_planning()
        .into_iter()
        .enumerate()
        .map(|(index, service)| (service, index))
        .collect::<BTreeMap<_, _>>();
    let mut entries = input
        .port_entries
        .iter()
        .filter(|entry| entry.eligibility == ComposePortEligibility::EligibleFixedTcp)
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| {
        (
            service_order
                .get(&entry.service)
                .copied()
                .unwrap_or(usize::MAX),
            entry.entry_index,
            entry.target_port.unwrap_or(u16::MAX),
            protocol_order(&entry.protocol),
            requested_host_port(entry).unwrap_or(u16::MAX),
            host_ip_display_value(&entry.host_ip),
        )
    });
    entries
}

fn existing_project_published_port_candidate<F>(
    existing_project_published_ports: &[ComposePublishedPortReservation],
    entry: &ComposePortEntry,
    requested: &ComposePublishedPortEndpoint,
    reservations: &[HostPortReservation],
    host_port_available: &mut F,
) -> std::result::Result<Option<ComposePublishedPortEndpoint>, ComposePublishedPortPlanError>
where
    F: FnMut(&str, u16) -> anyhow::Result<bool>,
{
    for existing in existing_project_published_ports {
        if !existing_project_published_port_matches_entry(existing, entry, requested) {
            continue;
        }
        let existing_host_ip = reservation_host_ip(&existing.endpoint);
        if host_port_reservations_conflict(
            reservations,
            existing_host_ip,
            existing.endpoint.host_port,
        ) {
            continue;
        }
        match existing.source {
            ComposePublishedPortReservationSource::RunningContainer => {
                return Ok(Some(existing_project_published_port_planned_endpoint(
                    existing, requested,
                )));
            }
            ComposePublishedPortReservationSource::StoppedContainer => {
                let available = host_port_available(existing_host_ip, existing.endpoint.host_port)
                    .map_err(
                        |source| ComposePublishedPortPlanError::HostPortAvailability {
                            host_ip: existing_host_ip.to_owned(),
                            host_port: existing.endpoint.host_port,
                            source,
                        },
                    )?;
                if available {
                    return Ok(Some(existing_project_published_port_planned_endpoint(
                        existing, requested,
                    )));
                }
            }
        }
    }

    Ok(None)
}

fn existing_project_published_port_planned_endpoint(
    existing: &ComposePublishedPortReservation,
    requested: &ComposePublishedPortEndpoint,
) -> ComposePublishedPortEndpoint {
    let host_ip = if reservation_host_ip(&existing.endpoint) == reservation_host_ip(requested) {
        requested.host_ip.clone()
    } else {
        existing.endpoint.host_ip.clone()
    };
    ComposePublishedPortEndpoint {
        host_ip,
        host_port: existing.endpoint.host_port,
    }
}

fn existing_project_published_port_matches_entry(
    existing: &ComposePublishedPortReservation,
    entry: &ComposePortEntry,
    requested: &ComposePublishedPortEndpoint,
) -> bool {
    existing.service == entry.service
        && Some(existing.target_port) == entry.target_port
        && existing.protocol == entry.protocol
        && host_ips_conflict(&existing.endpoint, requested)
}

fn host_ips_conflict(
    existing: &ComposePublishedPortEndpoint,
    candidate: &ComposePublishedPortEndpoint,
) -> bool {
    const HOST_PORT_FOR_HOST_IP_MATCH: u16 = 1;
    host_port_reservations_conflict(
        [HostPortReservation {
            host_ip: reservation_host_ip(existing).to_owned(),
            host: HOST_PORT_FOR_HOST_IP_MATCH,
        }]
        .iter(),
        reservation_host_ip(candidate),
        HOST_PORT_FOR_HOST_IP_MATCH,
    )
}

fn allocate_relocated_host_port<F>(
    entry: &ComposePortEntry,
    requested: &ComposePublishedPortEndpoint,
    reservation_host_ip: &str,
    reservations: &[HostPortReservation],
    host_port_available: &mut F,
) -> std::result::Result<u16, ComposePublishedPortPlanError>
where
    F: FnMut(&str, u16) -> anyhow::Result<bool>,
{
    let Some(mut candidate) = requested.host_port.checked_add(1) else {
        return Err(no_relocation_candidate(entry, requested));
    };

    loop {
        let reserved =
            host_port_reservations_conflict(reservations, reservation_host_ip, candidate);
        let available = if reserved {
            false
        } else {
            host_port_available(reservation_host_ip, candidate).map_err(|source| {
                ComposePublishedPortPlanError::HostPortAvailability {
                    host_ip: reservation_host_ip.to_owned(),
                    host_port: candidate,
                    source,
                }
            })?
        };
        if available {
            return Ok(candidate);
        }
        let Some(next) = candidate.checked_add(1) else {
            return Err(no_relocation_candidate(entry, requested));
        };
        candidate = next;
    }
}

fn no_relocation_candidate(
    entry: &ComposePortEntry,
    requested: &ComposePublishedPortEndpoint,
) -> ComposePublishedPortPlanError {
    ComposePublishedPortPlanError::NoRelocationCandidate {
        service: entry.service.clone(),
        port_entry_index: entry.entry_index,
        requested: requested.clone(),
    }
}
#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::runtime::compose_ports::{
        ComposePortProtocol, ComposePublishedHostPort, ComposePublishedPortAllocationReason,
        ComposePublishedPortEndpoint, ComposePublishedPortHostIp, ComposePublishedPortReservation,
        test_support::{forward_port, plan_with_availability, planning_input},
    };

    #[test]
    fn planner_returns_empty_plan_when_relocation_is_disabled() {
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

        let plan = plan_compose_published_ports_with(&input, false, &[], &[], |_, _| {
            panic!("availability must not be checked when relocation is disabled")
        })
        .unwrap();

        assert!(plan.entries.is_empty());
    }

    #[test]
    fn planner_uses_requested_endpoint_when_available() {
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

        let plan = plan_with_availability(&input, &[]);

        assert_eq!(plan.entries.len(), 1);
        let entry = &plan.entries[0];
        assert_eq!(entry.service, "app");
        assert_eq!(entry.port_entry_index, 0);
        assert_eq!(entry.source, ComposePublishedPortPlanSource::Compose);
        assert_eq!(entry.kind, ComposePublishedPortPlanEntryType::Published);
        assert_eq!(entry.target_port, 3000);
        assert_eq!(entry.protocol, ComposePortProtocol::Tcp);
        assert_eq!(entry.requested.host_port, 3000);
        assert_eq!(entry.planned.host_port, 3000);
        assert!(!entry.relocated);
        assert_eq!(
            entry.allocation_reason,
            ComposePublishedPortAllocationReason::Available
        );
    }

    #[test]
    fn planner_allocates_next_candidate_when_requested_is_unavailable() {
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

        let plan = plan_with_availability(&input, &[3000, 3001]);

        assert_eq!(plan.entries[0].requested.host_port, 3000);
        assert_eq!(plan.entries[0].planned.host_port, 3002);
        assert!(plan.entries[0].relocated);
        assert_eq!(
            plan.entries[0].allocation_reason,
            ComposePublishedPortAllocationReason::Unavailable
        );
    }

    #[test]
    fn planner_returns_typed_error_when_no_candidate_exists() {
        let input = planning_input(
            json!({
                "services": {
                    "app": {
                        "ports": [{"target": 3000, "published": "65535"}]
                    }
                }
            }),
            "app",
            &[],
        );

        let error = plan_compose_published_ports_with(&input, true, &[], &[], |_, _| Ok(false))
            .expect_err("planner should fail without a relocation candidate");

        match error {
            ComposePublishedPortPlanError::NoRelocationCandidate {
                service,
                port_entry_index,
                requested,
            } => {
                assert_eq!(service, "app");
                assert_eq!(port_entry_index, 0);
                assert_eq!(requested.host_port, u16::MAX);
            }
            ComposePublishedPortPlanError::HostPortAvailability { .. }
            | ComposePublishedPortPlanError::InconsistentEntry { .. } => {
                panic!("expected no-candidate error")
            }
        }
    }

    #[test]
    fn planner_errors_on_inconsistent_eligible_entry() {
        let mut input = planning_input(
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
        input.port_entries[0].published_host_port = ComposePublishedHostPort::None;

        let error = plan_compose_published_ports_with(&input, true, &[], &[], |_, _| Ok(true))
            .expect_err("inconsistent eligible entry must fail");

        match error {
            ComposePublishedPortPlanError::InconsistentEntry {
                service,
                port_entry_index,
                detail,
            } => {
                assert_eq!(service, "app");
                assert_eq!(port_entry_index, 0);
                assert!(detail.contains("published host port"));
            }
            other @ (ComposePublishedPortPlanError::NoRelocationCandidate { .. }
            | ComposePublishedPortPlanError::HostPortAvailability { .. }) => {
                panic!("expected inconsistent entry error, got {other:?}")
            }
        }
    }

    #[test]
    fn runtime_plan_errors_on_inconsistent_eligible_entry() {
        let mut input = planning_input(
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
        input.port_entries[0].target_port = None;

        let error =
            compose_published_port_runtime_plan(&input, &ComposePublishedPortPlan::default())
                .expect_err("inconsistent eligible entry must fail");

        match error {
            ComposePublishedPortPlanError::InconsistentEntry {
                service,
                port_entry_index,
                detail,
            } => {
                assert_eq!(service, "app");
                assert_eq!(port_entry_index, 0);
                assert!(detail.contains("target port"));
            }
            other @ (ComposePublishedPortPlanError::NoRelocationCandidate { .. }
            | ComposePublishedPortPlanError::HostPortAvailability { .. }) => {
                panic!("expected inconsistent entry error, got {other:?}")
            }
        }
    }

    #[test]
    fn planner_treats_existing_project_requested_binding_as_available() {
        let input = planning_input(
            json!({
                "services": {
                    "app": {
                        "ports": [{"host_ip": "127.0.0.1", "target": 3000, "published": "65535"}]
                    }
                }
            }),
            "app",
            &[],
        );
        let existing_project_published_ports = vec![ComposePublishedPortReservation {
            service: "app".to_owned(),
            target_port: 3000,
            protocol: ComposePortProtocol::Tcp,
            endpoint: ComposePublishedPortEndpoint {
                host_ip: ComposePublishedPortHostIp::Explicit("127.0.0.1".to_owned()),
                host_port: u16::MAX,
            },
            source: ComposePublishedPortReservationSource::RunningContainer,
        }];

        let plan = plan_compose_published_ports_with(
            &input,
            true,
            &[],
            &existing_project_published_ports,
            |_, _| Ok(false),
        )
        .unwrap();

        assert_eq!(plan.entries[0].planned.host_port, u16::MAX);
        assert!(!plan.entries[0].relocated);
        assert_eq!(
            plan.entries[0].allocation_reason,
            ComposePublishedPortAllocationReason::Available
        );
    }

    #[test]
    fn planner_does_not_reuse_existing_same_service_binding_when_target_changes() {
        let input = planning_input(
            json!({
                "services": {
                    "app": {
                        "ports": [{"host_ip": "127.0.0.1", "target": 4000, "published": "3000"}]
                    }
                }
            }),
            "app",
            &[],
        );
        let existing_project_published_ports = vec![ComposePublishedPortReservation {
            service: "app".to_owned(),
            target_port: 3000,
            protocol: ComposePortProtocol::Tcp,
            endpoint: ComposePublishedPortEndpoint {
                host_ip: ComposePublishedPortHostIp::Explicit("127.0.0.1".to_owned()),
                host_port: 3000,
            },
            source: ComposePublishedPortReservationSource::RunningContainer,
        }];

        let error = plan_compose_published_ports_with(
            &input,
            true,
            &[],
            &existing_project_published_ports,
            |_, _| Ok(false),
        )
        .expect_err("different target binding must not suppress availability failure");

        assert!(matches!(
            error,
            ComposePublishedPortPlanError::NoRelocationCandidate { .. }
        ));
    }

    #[test]
    fn planner_reuses_existing_same_service_relocated_candidate_binding() {
        let input = planning_input(
            json!({
                "services": {
                    "app": {
                        "ports": [{"host_ip": "127.0.0.1", "target": 3000, "published": "3000"}]
                    }
                }
            }),
            "app",
            &[],
        );
        let existing_project_published_ports = vec![ComposePublishedPortReservation {
            service: "app".to_owned(),
            target_port: 3000,
            protocol: ComposePortProtocol::Tcp,
            endpoint: ComposePublishedPortEndpoint {
                host_ip: ComposePublishedPortHostIp::Explicit("127.0.0.1".to_owned()),
                host_port: 3001,
            },
            source: ComposePublishedPortReservationSource::RunningContainer,
        }];

        let plan = plan_compose_published_ports_with(
            &input,
            true,
            &[],
            &existing_project_published_ports,
            |_, _| Ok(false),
        )
        .unwrap();

        assert_eq!(plan.entries[0].planned.host_port, 3001);
        assert!(plan.entries[0].relocated);
        assert_eq!(
            plan.entries[0].allocation_reason,
            ComposePublishedPortAllocationReason::Unavailable
        );
    }

    #[test]
    fn planner_treats_existing_wildcard_binding_as_omitted_host_ip_when_port_matches() {
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
        let existing_project_published_ports = vec![ComposePublishedPortReservation {
            service: "app".to_owned(),
            target_port: 3000,
            protocol: ComposePortProtocol::Tcp,
            endpoint: ComposePublishedPortEndpoint {
                host_ip: ComposePublishedPortHostIp::Explicit("0.0.0.0".to_owned()),
                host_port: 3000,
            },
            source: ComposePublishedPortReservationSource::RunningContainer,
        }];

        let plan = plan_compose_published_ports_with(
            &input,
            true,
            &[],
            &existing_project_published_ports,
            |_, _| Ok(false),
        )
        .unwrap();

        assert_eq!(
            plan.entries[0].planned,
            ComposePublishedPortEndpoint {
                host_ip: ComposePublishedPortHostIp::Omitted,
                host_port: 3000,
            }
        );
        assert!(!plan.entries[0].relocated);
    }

    #[test]
    fn planner_reuses_stopped_existing_relocated_binding_when_available() {
        let input = planning_input(
            json!({
                "services": {
                    "app": {
                        "ports": [{"host_ip": "127.0.0.1", "target": 3000, "published": "3000"}]
                    }
                }
            }),
            "app",
            &[],
        );
        let existing_project_published_ports = vec![ComposePublishedPortReservation {
            service: "app".to_owned(),
            target_port: 3000,
            protocol: ComposePortProtocol::Tcp,
            endpoint: ComposePublishedPortEndpoint {
                host_ip: ComposePublishedPortHostIp::Explicit("127.0.0.1".to_owned()),
                host_port: 3001,
            },
            source: ComposePublishedPortReservationSource::StoppedContainer,
        }];

        let plan = plan_compose_published_ports_with(
            &input,
            true,
            &[],
            &existing_project_published_ports,
            |_, _| Ok(true),
        )
        .unwrap();

        assert_eq!(plan.entries[0].planned.host_port, 3001);
        assert!(plan.entries[0].relocated);
    }

    #[test]
    fn planner_relocates_when_stopped_existing_binding_is_unavailable() {
        let input = planning_input(
            json!({
                "services": {
                    "app": {
                        "ports": [{"host_ip": "127.0.0.1", "target": 3000, "published": "3000"}]
                    }
                }
            }),
            "app",
            &[],
        );
        let existing_project_published_ports = vec![ComposePublishedPortReservation {
            service: "app".to_owned(),
            target_port: 3000,
            protocol: ComposePortProtocol::Tcp,
            endpoint: ComposePublishedPortEndpoint {
                host_ip: ComposePublishedPortHostIp::Explicit("127.0.0.1".to_owned()),
                host_port: 3000,
            },
            source: ComposePublishedPortReservationSource::StoppedContainer,
        }];

        let plan = plan_compose_published_ports_with(
            &input,
            true,
            &[],
            &existing_project_published_ports,
            |_, port| Ok(port != 3000),
        )
        .unwrap();

        assert_eq!(plan.entries[0].planned.host_port, 3001);
        assert!(plan.entries[0].relocated);
        assert_eq!(
            plan.entries[0].allocation_reason,
            ComposePublishedPortAllocationReason::Unavailable
        );
    }

    #[test]
    fn planner_does_not_ignore_different_existing_project_service_binding() {
        let input = planning_input(
            json!({
                "services": {
                    "app": {
                        "ports": [{"host_ip": "127.0.0.1", "target": 3000, "published": "65535"}]
                    }
                }
            }),
            "app",
            &[],
        );
        let existing_project_published_ports = vec![ComposePublishedPortReservation {
            service: "db".to_owned(),
            target_port: 3000,
            protocol: ComposePortProtocol::Tcp,
            endpoint: ComposePublishedPortEndpoint {
                host_ip: ComposePublishedPortHostIp::Explicit("127.0.0.1".to_owned()),
                host_port: u16::MAX,
            },
            source: ComposePublishedPortReservationSource::RunningContainer,
        }];

        let error = plan_compose_published_ports_with(
            &input,
            true,
            &[],
            &existing_project_published_ports,
            |_, _| Ok(false),
        )
        .expect_err("different service binding must not suppress availability failure");

        assert!(matches!(
            error,
            ComposePublishedPortPlanError::NoRelocationCandidate { .. }
        ));
    }

    #[test]
    fn planner_preserves_omitted_and_explicit_host_ip_semantics() {
        let input = planning_input(
            json!({
                "services": {
                    "app": {
                        "ports": [
                            {"target": 3000, "published": "3000"},
                            {"host_ip": "127.0.0.1", "target": 3001, "published": "3001"},
                            {"host_ip": "0.0.0.0", "target": 3002, "published": "3002"}
                        ]
                    }
                }
            }),
            "app",
            &[],
        );

        let plan = plan_with_availability(&input, &[3000, 3001, 3002]);

        assert_eq!(
            plan.entries[0].planned.host_ip,
            ComposePublishedPortHostIp::Omitted
        );
        assert_eq!(
            plan.entries[1].planned.host_ip,
            ComposePublishedPortHostIp::Explicit("127.0.0.1".to_owned())
        );
        assert_eq!(
            plan.entries[2].planned.host_ip,
            ComposePublishedPortHostIp::Explicit("0.0.0.0".to_owned())
        );
    }

    #[test]
    fn planner_prioritizes_primary_service_for_duplicate_ports() {
        let input = planning_input(
            json!({
                "services": {
                    "admin": {
                        "ports": [{"target": 3000, "published": "3000"}]
                    },
                    "web": {
                        "ports": [{"target": 3000, "published": "3000"}]
                    }
                }
            }),
            "web",
            &[],
        );

        let plan = plan_with_availability(&input, &[]);

        assert_eq!(plan.entries[0].service, "web");
        assert_eq!(plan.entries[0].planned.host_port, 3000);
        assert_eq!(plan.entries[1].service, "admin");
        assert_eq!(plan.entries[1].planned.host_port, 3001);
        assert_eq!(
            plan.entries[1].allocation_reason,
            ComposePublishedPortAllocationReason::Reserved
        );
    }

    #[test]
    fn planner_uses_user_selected_service_order_before_remaining_services() {
        let selected = vec!["worker".to_owned(), "api".to_owned()];
        let input = planning_input(
            json!({
                "services": {
                    "api": {
                        "ports": [{"target": 3000, "published": "3000"}]
                    },
                    "app": {
                        "ports": [{"target": 3000, "published": "3000"}]
                    },
                    "worker": {
                        "ports": [{"target": 3000, "published": "3000"}]
                    }
                }
            }),
            "app",
            &selected,
        );

        let plan = plan_with_availability(&input, &[]);

        assert_eq!(
            plan.entries
                .iter()
                .map(|entry| (entry.service.as_str(), entry.planned.host_port))
                .collect::<Vec<_>>(),
            vec![("app", 3000), ("worker", 3001), ("api", 3002)]
        );
    }

    #[test]
    fn planner_conflicts_with_existing_forwarding_reservations() {
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
        let forwarding = vec![forward_port("127.0.0.1", 3000, 8080)];

        let plan =
            plan_compose_published_ports_with(&input, true, &forwarding, &[], |_, _| Ok(true))
                .unwrap();

        assert_eq!(plan.entries[0].planned.host_port, 3001);
        assert_eq!(
            plan.entries[0].allocation_reason,
            ComposePublishedPortAllocationReason::Reserved
        );
    }

    #[test]
    fn planner_uses_forwarding_wildcard_reservations_for_omitted_host_ip() {
        let input = planning_input(
            json!({
                "services": {
                    "app": {
                        "ports": [
                            {"target": 3000, "published": "3000"},
                            {"host_ip": "::1", "target": 3001, "published": "3000"}
                        ]
                    }
                }
            }),
            "app",
            &[],
        );
        let forwarding = vec![forward_port("0.0.0.0", 3000, 8080)];

        let plan =
            plan_compose_published_ports_with(&input, true, &forwarding, &[], |_, _| Ok(true))
                .unwrap();

        assert_eq!(plan.entries[0].planned.host_port, 3001);
        assert_eq!(
            plan.entries[0].allocation_reason,
            ComposePublishedPortAllocationReason::Reserved
        );
        assert_eq!(plan.entries[1].planned.host_port, 3000);
    }

    #[test]
    fn planner_uses_wildcard_collision_reasoning_for_reservations() {
        let input = planning_input(
            json!({
                "services": {
                    "app": {
                        "ports": [
                            {"host_ip": "0.0.0.0", "target": 3000, "published": "3000"},
                            {"host_ip": "127.0.0.1", "target": 3001, "published": "3000"},
                            {"host_ip": "::1", "target": 3002, "published": "3000"}
                        ]
                    }
                }
            }),
            "app",
            &[],
        );

        let plan = plan_with_availability(&input, &[]);

        assert_eq!(plan.entries[0].planned.host_port, 3000);
        assert_eq!(plan.entries[1].planned.host_port, 3001);
        assert_eq!(plan.entries[2].planned.host_port, 3000);
    }
}
