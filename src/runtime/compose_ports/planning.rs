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
    },
};

use super::endpoint::{
    endpoint_for_entry, host_ip_display_value, protocol_order, requested_host_port,
    reservation_host_ip,
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
        let requested = endpoint_for_entry(entry);
        let requested_host_ip = reservation_host_ip(&requested);
        let requested_reserved =
            host_port_reservations_conflict(&reservations, requested_host_ip, requested.host_port);
        let requested_available = if requested_reserved {
            false
        } else if existing_project_published_port_matches(
            existing_project_published_ports,
            entry,
            &requested,
        ) {
            true
        } else {
            host_port_available(requested_host_ip, requested.host_port).map_err(|source| {
                ComposePublishedPortPlanError::HostPortAvailability {
                    host_ip: requested_host_ip.to_owned(),
                    host_port: requested.host_port,
                    source,
                }
            })?
        };
        let (planned_host_port, allocation_reason) = if requested_available {
            (
                requested.host_port,
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
                existing_project_published_ports,
                &mut host_port_available,
            )?;
            (planned_host_port, allocation_reason)
        };
        let planned = ComposePublishedPortEndpoint {
            ip_kind: requested.ip_kind,
            ip_value: requested.ip_value.clone(),
            host_port: planned_host_port,
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
            target_port: entry
                .target_port
                .expect("eligible Compose published port entry has target port"),
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
) -> ComposePublishedPortPlan {
    let planned_entries = planned
        .entries
        .iter()
        .map(|entry| ((entry.service.as_str(), entry.port_entry_index), entry))
        .collect::<BTreeMap<_, _>>();
    let entries = input
        .port_entries
        .iter()
        .filter(|entry| entry.eligibility == ComposePortEligibility::EligibleFixedTcp)
        .map(|entry| {
            if let Some(planned) = planned_entries.get(&(entry.service.as_str(), entry.entry_index))
            {
                return (*planned).clone();
            }
            let requested = endpoint_for_entry(entry);
            ComposePublishedPortPlanEntry {
                service: entry.service.clone(),
                port_entry_index: entry.entry_index,
                source: ComposePublishedPortPlanSource::Compose,
                kind: ComposePublishedPortPlanEntryType::Published,
                target_port: entry
                    .target_port
                    .expect("eligible Compose published port entry has target port"),
                protocol: entry.protocol.clone(),
                requested: requested.clone(),
                planned: requested,
                relocated: false,
                allocation_reason: ComposePublishedPortAllocationReason::Available,
            }
        })
        .collect();

    ComposePublishedPortPlan { entries }
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

fn existing_project_published_port_matches(
    existing_project_published_ports: &[ComposePublishedPortReservation],
    entry: &ComposePortEntry,
    requested: &ComposePublishedPortEndpoint,
) -> bool {
    existing_project_published_ports.iter().any(|existing| {
        existing.service == entry.service
            && existing.protocol == entry.protocol
            && existing.endpoint.host_port == requested.host_port
            && host_port_reservations_conflict(
                [HostPortReservation {
                    host_ip: reservation_host_ip(&existing.endpoint).to_owned(),
                    host: existing.endpoint.host_port,
                }]
                .iter(),
                reservation_host_ip(requested),
                requested.host_port,
            )
    })
}

fn allocate_relocated_host_port<F>(
    entry: &ComposePortEntry,
    requested: &ComposePublishedPortEndpoint,
    reservation_host_ip: &str,
    reservations: &[HostPortReservation],
    existing_project_published_ports: &[ComposePublishedPortReservation],
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
        let endpoint = ComposePublishedPortEndpoint {
            ip_kind: requested.ip_kind,
            ip_value: requested.ip_value.clone(),
            host_port: candidate,
        };
        let available = if reserved {
            false
        } else if existing_project_published_port_matches(
            existing_project_published_ports,
            entry,
            &endpoint,
        ) {
            true
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
        ComposePortProtocol, ComposePublishedPortAllocationReason, ComposePublishedPortEndpoint,
        ComposePublishedPortHostIpKind, ComposePublishedPortReservation,
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
            ComposePublishedPortPlanError::HostPortAvailability { .. } => {
                panic!("expected no-candidate error")
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
                ip_kind: ComposePublishedPortHostIpKind::Explicit,
                ip_value: Some("127.0.0.1".to_owned()),
                host_port: u16::MAX,
            },
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
    fn planner_treats_existing_same_service_binding_as_available_when_target_changes() {
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
                ip_kind: ComposePublishedPortHostIpKind::Explicit,
                ip_value: Some("127.0.0.1".to_owned()),
                host_port: 3000,
            },
        }];

        let plan = plan_compose_published_ports_with(
            &input,
            true,
            &[],
            &existing_project_published_ports,
            |_, _| Ok(false),
        )
        .unwrap();

        assert_eq!(plan.entries[0].planned.host_port, 3000);
        assert!(!plan.entries[0].relocated);
        assert_eq!(
            plan.entries[0].allocation_reason,
            ComposePublishedPortAllocationReason::Available
        );
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
                ip_kind: ComposePublishedPortHostIpKind::Explicit,
                ip_value: Some("127.0.0.1".to_owned()),
                host_port: 3001,
            },
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
                ip_kind: ComposePublishedPortHostIpKind::Explicit,
                ip_value: Some("127.0.0.1".to_owned()),
                host_port: u16::MAX,
            },
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
            plan.entries[0].planned.ip_kind,
            ComposePublishedPortHostIpKind::Omitted
        );
        assert_eq!(plan.entries[0].planned.ip_value, None);
        assert_eq!(
            plan.entries[1].planned.ip_kind,
            ComposePublishedPortHostIpKind::Explicit
        );
        assert_eq!(
            plan.entries[1].planned.ip_value.as_deref(),
            Some("127.0.0.1")
        );
        assert_eq!(plan.entries[2].planned.ip_value.as_deref(), Some("0.0.0.0"));
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
