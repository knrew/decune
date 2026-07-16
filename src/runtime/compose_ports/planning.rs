use std::collections::BTreeMap;

use crate::{
    docker::ports::{
        HostPortProbe, HostPortReservation, ResolvedForwardPort, host_port_probe,
        host_port_reservations_conflict, resolved_forward_port_reservations,
    },
    runtime::compose_ports::{
        ComposePortEligibility, ComposePortEntry, ComposePublishedPortAllocationReason,
        ComposePublishedPortEndpoint, ComposePublishedPortMapping, ComposePublishedPortPlan,
        ComposePublishedPortPlanEntry, ComposePublishedPortPlanEntryType,
        ComposePublishedPortPlanSource, ComposePublishedPortPlannedEndpointProbe,
        ComposePublishedPortPlanningInput, ComposePublishedPortReservation,
        ComposePublishedPortReservationSource,
    },
};

use super::endpoint::{
    compose_published_port_endpoint_display, endpoint_for_entry, host_ip_display_value,
    protocol_order, requested_host_port, reservation_host_ip, target_port_for_entry,
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
    MappingConflict {
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
            Self::MappingConflict { detail } => formatter.write_str(detail),
        }
    }
}

impl std::error::Error for ComposePublishedPortPlanError {}

pub(crate) fn plan_compose_published_ports_with_existing_project(
    input: &ComposePublishedPortPlanningInput,
    relocation_enabled: bool,
    existing_forward_ports: &[ResolvedForwardPort],
    mappings: &[ComposePublishedPortMapping],
    existing_project_published_ports: &[ComposePublishedPortReservation],
    preserve_existing_bindings: bool,
    external_host_reservations: &[HostPortReservation],
) -> std::result::Result<ComposePublishedPortPlan, ComposePublishedPortPlanError> {
    plan_compose_published_ports_inner(
        input,
        relocation_enabled,
        existing_forward_ports,
        mappings,
        ExistingProjectBindings::new(existing_project_published_ports, preserve_existing_bindings),
        external_host_reservations,
        host_port_probe,
    )
}

#[cfg(test)]
pub(crate) fn plan_compose_published_ports_with<F>(
    input: &ComposePublishedPortPlanningInput,
    relocation_enabled: bool,
    existing_forward_ports: &[ResolvedForwardPort],
    existing_project_published_ports: &[ComposePublishedPortReservation],
    host_port_probe: F,
) -> std::result::Result<ComposePublishedPortPlan, ComposePublishedPortPlanError>
where
    F: FnMut(&str, u16) -> anyhow::Result<HostPortProbe>,
{
    plan_compose_published_ports_inner(
        input,
        relocation_enabled,
        existing_forward_ports,
        &[],
        ExistingProjectBindings::new(existing_project_published_ports, true),
        &[],
        host_port_probe,
    )
}

#[cfg(test)]
pub(crate) fn plan_compose_published_ports_with_mappings<F>(
    input: &ComposePublishedPortPlanningInput,
    relocation_enabled: bool,
    existing_forward_ports: &[ResolvedForwardPort],
    mappings: &[ComposePublishedPortMapping],
    existing_project_published_ports: &[ComposePublishedPortReservation],
    external_host_reservations: &[HostPortReservation],
    host_port_probe: F,
) -> std::result::Result<ComposePublishedPortPlan, ComposePublishedPortPlanError>
where
    F: FnMut(&str, u16) -> anyhow::Result<HostPortProbe>,
{
    plan_compose_published_ports_inner(
        input,
        relocation_enabled,
        existing_forward_ports,
        mappings,
        ExistingProjectBindings::new(existing_project_published_ports, true),
        external_host_reservations,
        host_port_probe,
    )
}

fn plan_compose_published_ports_inner<F>(
    input: &ComposePublishedPortPlanningInput,
    relocation_enabled: bool,
    existing_forward_ports: &[ResolvedForwardPort],
    mappings: &[ComposePublishedPortMapping],
    existing_project_bindings: ExistingProjectBindings<'_>,
    external_host_reservations: &[HostPortReservation],
    mut host_port_probe: F,
) -> std::result::Result<ComposePublishedPortPlan, ComposePublishedPortPlanError>
where
    F: FnMut(&str, u16) -> anyhow::Result<HostPortProbe>,
{
    if !relocation_enabled && mappings.is_empty() {
        return Ok(ComposePublishedPortPlan::default());
    }

    let mut reservations =
        resolved_forward_port_reservations(existing_forward_ports).collect::<Vec<_>>();
    reservations.extend(external_host_reservations.iter().cloned());
    let mut state = PublishedPortPlanningState {
        existing_project_published_ports: existing_project_bindings.all,
        reusable_existing_project_published_ports: existing_project_bindings.reusable,
        running_project_reservations: running_project_published_port_reservations(
            existing_project_bindings.reusable,
        ),
        reservations,
        plan_entries: Vec::new(),
    };
    let ordered_entries = ordered_eligible_port_entries(input);

    plan_explicit_mappings(&ordered_entries, mappings, &mut state, &mut host_port_probe)?;
    plan_unmapped_entries(
        &ordered_entries,
        relocation_enabled,
        mappings,
        &mut state,
        &mut host_port_probe,
    )?;

    let order = ordered_entries
        .iter()
        .enumerate()
        .map(|(index, entry)| ((entry.service.as_str(), entry.entry_index), index))
        .collect::<BTreeMap<_, _>>();
    state.plan_entries.sort_by_key(|entry| {
        order
            .get(&(entry.service.as_str(), entry.port_entry_index))
            .copied()
            .unwrap_or(usize::MAX)
    });

    Ok(ComposePublishedPortPlan {
        entries: state.plan_entries,
    })
}

#[derive(Clone, Copy)]
struct ExistingProjectBindings<'a> {
    all: &'a [ComposePublishedPortReservation],
    reusable: &'a [ComposePublishedPortReservation],
}

impl<'a> ExistingProjectBindings<'a> {
    const fn new(all: &'a [ComposePublishedPortReservation], preserve: bool) -> Self {
        Self {
            all,
            reusable: if preserve { all } else { &[] },
        }
    }
}

struct PublishedPortPlanningState<'a> {
    existing_project_published_ports: &'a [ComposePublishedPortReservation],
    reusable_existing_project_published_ports: &'a [ComposePublishedPortReservation],
    running_project_reservations: Vec<HostPortReservation>,
    reservations: Vec<HostPortReservation>,
    plan_entries: Vec<ComposePublishedPortPlanEntry>,
}

fn plan_explicit_mappings<F>(
    ordered_entries: &[&ComposePortEntry],
    mappings: &[ComposePublishedPortMapping],
    state: &mut PublishedPortPlanningState<'_>,
    host_port_probe: &mut F,
) -> std::result::Result<(), ComposePublishedPortPlanError>
where
    F: FnMut(&str, u16) -> anyhow::Result<HostPortProbe>,
{
    for mapping in mappings {
        let entry = ordered_entries
            .iter()
            .copied()
            .find(|entry| mapping_matches_entry(mapping, entry))
            .ok_or_else(|| ComposePublishedPortPlanError::InconsistentEntry {
                service: mapping.service.clone(),
                port_entry_index: mapping.port_entry_index,
                detail: "resolved mapping does not match an eligible Compose port entry".to_owned(),
            })?;
        let requested = endpoint_for_entry(entry)?;
        let target_port = target_port_for_entry(entry)?;
        let effective_reservations = state
            .reservations
            .iter()
            .chain(
                running_project_published_port_reservations_excluding_entry(
                    state.existing_project_published_ports,
                    entry,
                )
                .iter(),
            )
            .cloned()
            .collect::<Vec<_>>();
        let desired_endpoint_is_held_by_current_entry = state
            .existing_project_published_ports
            .iter()
            .any(|reservation| {
                reservation.source == ComposePublishedPortReservationSource::RunningContainer
                    && existing_project_published_port_identity_matches_entry(reservation, entry)
                    && host_ips_conflict(&reservation.endpoint, &mapping.endpoint)
                    && reservation.endpoint.host_port == mapping.endpoint.host_port
            });
        let decision = decision_for_mapping(
            mapping,
            &requested,
            &effective_reservations,
            desired_endpoint_is_held_by_current_entry,
            host_port_probe,
        )?;
        state.reservations.push(HostPortReservation {
            host_ip: reservation_host_ip(&decision.planned).to_owned(),
            host: decision.planned.host_port,
        });
        state
            .plan_entries
            .push(plan_entry(entry, requested, target_port, decision));
    }
    Ok(())
}

fn plan_unmapped_entries<F>(
    ordered_entries: &[&ComposePortEntry],
    relocation_enabled: bool,
    mappings: &[ComposePublishedPortMapping],
    state: &mut PublishedPortPlanningState<'_>,
    host_port_probe: &mut F,
) -> std::result::Result<(), ComposePublishedPortPlanError>
where
    F: FnMut(&str, u16) -> anyhow::Result<HostPortProbe>,
{
    for &entry in ordered_entries {
        if mappings
            .iter()
            .any(|mapping| mapping_matches_entry(mapping, entry))
        {
            continue;
        }
        let requested = endpoint_for_entry(entry)?;
        let target_port = target_port_for_entry(entry)?;
        if !relocation_enabled {
            plan_unrelocated_entry(entry, requested, target_port, mappings, state)?;
            continue;
        }
        let requested_host_ip = reservation_host_ip(&requested);
        let existing_endpoint = existing_project_published_port_candidate(
            state.reusable_existing_project_published_ports,
            entry,
            &requested,
            &state.reservations,
            host_port_probe,
        )?;
        let effective_reservations = state
            .reservations
            .iter()
            .chain(state.running_project_reservations.iter())
            .cloned()
            .collect::<Vec<_>>();
        let requested_reserved = host_port_reservations_conflict(
            &effective_reservations,
            requested_host_ip,
            requested.host_port,
        );
        let decision = if let Some(existing_candidate) = existing_endpoint {
            decision_for_existing_project_candidate(existing_candidate, &requested)
        } else {
            decision_for_requested_or_relocated_endpoint(
                entry,
                &requested,
                requested_host_ip,
                requested_reserved,
                &effective_reservations,
                host_port_probe,
            )?
        };
        state.reservations.push(HostPortReservation {
            host_ip: reservation_host_ip(&decision.planned).to_owned(),
            host: decision.planned.host_port,
        });
        state
            .plan_entries
            .push(plan_entry(entry, requested, target_port, decision));
    }
    Ok(())
}

fn plan_unrelocated_entry(
    entry: &ComposePortEntry,
    requested: ComposePublishedPortEndpoint,
    target_port: u16,
    mappings: &[ComposePublishedPortMapping],
    state: &mut PublishedPortPlanningState<'_>,
) -> std::result::Result<(), ComposePublishedPortPlanError> {
    if let Some(mapping) = mappings.iter().find(|mapping| {
        host_ips_conflict(&mapping.endpoint, &requested)
            && mapping.endpoint.host_port == requested.host_port
    }) {
        return Err(ComposePublishedPortPlanError::MappingConflict {
            detail: format!(
                "Compose published port mapping for service `{}`, target {}/tcp reserves desired endpoint {}, which conflicts with the unchanged requested endpoint for service `{}`, target {target_port}/tcp while automatic relocation is disabled",
                mapping.service,
                mapping.target_port,
                compose_published_port_endpoint_display(&mapping.endpoint),
                entry.service
            ),
        });
    }
    let decision = PlannedEndpointDecision {
        planned: requested.clone(),
        allocation_reason: ComposePublishedPortAllocationReason::Available,
        planned_endpoint_probe: ComposePublishedPortPlannedEndpointProbe::Unprobeable,
    };
    state.reservations.push(HostPortReservation {
        host_ip: reservation_host_ip(&requested).to_owned(),
        host: requested.host_port,
    });
    state
        .plan_entries
        .push(plan_entry(entry, requested, target_port, decision));
    Ok(())
}

fn decision_for_mapping<F>(
    mapping: &ComposePublishedPortMapping,
    requested: &ComposePublishedPortEndpoint,
    reservations: &[HostPortReservation],
    desired_endpoint_is_held_by_current_entry: bool,
    host_port_probe: &mut F,
) -> std::result::Result<PlannedEndpointDecision, ComposePublishedPortPlanError>
where
    F: FnMut(&str, u16) -> anyhow::Result<HostPortProbe>,
{
    let desired_host_ip = reservation_host_ip(&mapping.endpoint);
    let reserved =
        host_port_reservations_conflict(reservations, desired_host_ip, mapping.endpoint.host_port);
    let probe = if reserved {
        HostPortProbe::Occupied
    } else if desired_endpoint_is_held_by_current_entry {
        HostPortProbe::Available
    } else {
        probe_compose_published_host_port(
            host_port_probe,
            desired_host_ip,
            mapping.endpoint.host_port,
        )?
    };
    if probe == HostPortProbe::Occupied {
        return Err(ComposePublishedPortPlanError::MappingConflict {
            detail: format!(
                "Compose published port mapping for service `{}`, target {}/tcp cannot use desired endpoint {} because it is already reserved or occupied; requested endpoint: {}. Explicit mappings do not fall back to automatic relocation",
                mapping.service,
                mapping.target_port,
                compose_published_port_endpoint_display(&mapping.endpoint),
                compose_published_port_endpoint_display(requested)
            ),
        });
    }
    Ok(PlannedEndpointDecision {
        planned: mapping.endpoint.clone(),
        allocation_reason: ComposePublishedPortAllocationReason::Mapping,
        planned_endpoint_probe: if probe == HostPortProbe::Available {
            ComposePublishedPortPlannedEndpointProbe::Available
        } else {
            ComposePublishedPortPlannedEndpointProbe::Unprobeable
        },
    })
}

fn plan_entry(
    entry: &ComposePortEntry,
    requested: ComposePublishedPortEndpoint,
    target_port: u16,
    decision: PlannedEndpointDecision,
) -> ComposePublishedPortPlanEntry {
    let relocated = requested != decision.planned;
    ComposePublishedPortPlanEntry {
        service: entry.service.clone(),
        port_entry_index: entry.entry_index,
        source: ComposePublishedPortPlanSource::Compose,
        kind: ComposePublishedPortPlanEntryType::Published,
        target_port,
        protocol: entry.protocol.clone(),
        requested,
        planned: decision.planned,
        planned_endpoint_probe: decision.planned_endpoint_probe,
        relocated,
        allocation_reason: decision.allocation_reason,
    }
}

fn mapping_matches_entry(mapping: &ComposePublishedPortMapping, entry: &ComposePortEntry) -> bool {
    (
        mapping.service.as_str(),
        mapping.port_entry_index,
        mapping.target_port,
        &mapping.protocol,
    ) == (
        entry.service.as_str(),
        entry.entry_index,
        entry.target_port.unwrap_or_default(),
        &entry.protocol,
    )
}

fn decision_for_existing_project_candidate(
    existing_candidate: ExistingProjectPublishedPortCandidate,
    requested: &ComposePublishedPortEndpoint,
) -> PlannedEndpointDecision {
    let allocation_reason = if existing_candidate.endpoint == *requested {
        ComposePublishedPortAllocationReason::Available
    } else {
        ComposePublishedPortAllocationReason::Unavailable
    };
    PlannedEndpointDecision {
        planned: existing_candidate.endpoint,
        allocation_reason,
        planned_endpoint_probe: existing_candidate.planned_endpoint_probe,
    }
}

fn decision_for_requested_or_relocated_endpoint<F>(
    entry: &ComposePortEntry,
    requested: &ComposePublishedPortEndpoint,
    requested_host_ip: &str,
    requested_reserved: bool,
    reservations: &[HostPortReservation],
    host_port_probe: &mut F,
) -> std::result::Result<PlannedEndpointDecision, ComposePublishedPortPlanError>
where
    F: FnMut(&str, u16) -> anyhow::Result<HostPortProbe>,
{
    let requested_probe = if requested_reserved {
        HostPortProbe::Occupied
    } else {
        probe_compose_published_host_port(host_port_probe, requested_host_ip, requested.host_port)?
    };
    match requested_probe {
        HostPortProbe::Available => Ok(PlannedEndpointDecision {
            planned: requested.clone(),
            allocation_reason: ComposePublishedPortAllocationReason::Available,
            planned_endpoint_probe: ComposePublishedPortPlannedEndpointProbe::Available,
        }),
        HostPortProbe::Unprobeable => Ok(PlannedEndpointDecision {
            planned: requested.clone(),
            allocation_reason: ComposePublishedPortAllocationReason::Available,
            planned_endpoint_probe: ComposePublishedPortPlannedEndpointProbe::Unprobeable,
        }),
        HostPortProbe::Occupied => {
            let allocation_reason = if requested_reserved {
                ComposePublishedPortAllocationReason::Reserved
            } else {
                ComposePublishedPortAllocationReason::Unavailable
            };
            let planned_host_port = allocate_relocated_host_port(
                entry,
                requested,
                requested_host_ip,
                reservations,
                host_port_probe,
            )?;
            Ok(PlannedEndpointDecision {
                planned: ComposePublishedPortEndpoint {
                    host_ip: requested.host_ip.clone(),
                    host_port: planned_host_port,
                },
                allocation_reason,
                planned_endpoint_probe: ComposePublishedPortPlannedEndpointProbe::Available,
            })
        }
    }
}

struct PlannedEndpointDecision {
    planned: ComposePublishedPortEndpoint,
    allocation_reason: ComposePublishedPortAllocationReason,
    planned_endpoint_probe: ComposePublishedPortPlannedEndpointProbe,
}

fn running_project_published_port_reservations(
    existing_project_published_ports: &[ComposePublishedPortReservation],
) -> Vec<HostPortReservation> {
    running_project_published_port_reservations_with_filter(
        existing_project_published_ports,
        |_| true,
    )
}

fn running_project_published_port_reservations_excluding_entry(
    existing_project_published_ports: &[ComposePublishedPortReservation],
    entry: &ComposePortEntry,
) -> Vec<HostPortReservation> {
    running_project_published_port_reservations_with_filter(
        existing_project_published_ports,
        |reservation| !existing_project_published_port_identity_matches_entry(reservation, entry),
    )
}

fn running_project_published_port_reservations_with_filter<F>(
    existing_project_published_ports: &[ComposePublishedPortReservation],
    mut include: F,
) -> Vec<HostPortReservation>
where
    F: FnMut(&ComposePublishedPortReservation) -> bool,
{
    existing_project_published_ports
        .iter()
        .filter(|reservation| {
            reservation.source == ComposePublishedPortReservationSource::RunningContainer
                && include(reservation)
        })
        .map(|reservation| HostPortReservation {
            host_ip: reservation_host_ip(&reservation.endpoint).to_owned(),
            host: reservation.endpoint.host_port,
        })
        .collect()
}

fn existing_project_published_port_identity_matches_entry(
    existing: &ComposePublishedPortReservation,
    entry: &ComposePortEntry,
) -> bool {
    existing.service == entry.service
        && Some(existing.target_port) == entry.target_port
        && existing.protocol == entry.protocol
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
            planned_endpoint_probe: ComposePublishedPortPlannedEndpointProbe::Available,
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
    host_port_probe: &mut F,
) -> std::result::Result<Option<ExistingProjectPublishedPortCandidate>, ComposePublishedPortPlanError>
where
    F: FnMut(&str, u16) -> anyhow::Result<HostPortProbe>,
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
                return Ok(Some(ExistingProjectPublishedPortCandidate {
                    endpoint: existing_project_published_port_planned_endpoint(existing, requested),
                    planned_endpoint_probe: ComposePublishedPortPlannedEndpointProbe::Available,
                }));
            }
            ComposePublishedPortReservationSource::StoppedContainer => {
                let existing_probe = probe_compose_published_host_port(
                    host_port_probe,
                    existing_host_ip,
                    existing.endpoint.host_port,
                )?;
                let planned_endpoint_probe = match existing_probe {
                    HostPortProbe::Available => ComposePublishedPortPlannedEndpointProbe::Available,
                    HostPortProbe::Unprobeable => {
                        ComposePublishedPortPlannedEndpointProbe::Unprobeable
                    }
                    HostPortProbe::Occupied => continue,
                };
                return Ok(Some(ExistingProjectPublishedPortCandidate {
                    endpoint: existing_project_published_port_planned_endpoint(existing, requested),
                    planned_endpoint_probe,
                }));
            }
        }
    }

    Ok(None)
}

struct ExistingProjectPublishedPortCandidate {
    endpoint: ComposePublishedPortEndpoint,
    planned_endpoint_probe: ComposePublishedPortPlannedEndpointProbe,
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
    host_port_probe: &mut F,
) -> std::result::Result<u16, ComposePublishedPortPlanError>
where
    F: FnMut(&str, u16) -> anyhow::Result<HostPortProbe>,
{
    let Some(mut candidate) = requested.host_port.checked_add(1) else {
        return Err(no_relocation_candidate(entry, requested));
    };

    loop {
        let reserved =
            host_port_reservations_conflict(reservations, reservation_host_ip, candidate);
        let candidate_probe = if reserved {
            HostPortProbe::Occupied
        } else {
            probe_compose_published_host_port(host_port_probe, reservation_host_ip, candidate)?
        };
        if candidate_probe == HostPortProbe::Available {
            return Ok(candidate);
        }
        let Some(next) = candidate.checked_add(1) else {
            return Err(no_relocation_candidate(entry, requested));
        };
        candidate = next;
    }
}

fn probe_compose_published_host_port<F>(
    host_port_probe: &mut F,
    host_ip: &str,
    host_port: u16,
) -> std::result::Result<HostPortProbe, ComposePublishedPortPlanError>
where
    F: FnMut(&str, u16) -> anyhow::Result<HostPortProbe>,
{
    host_port_probe(host_ip, host_port).map_err(|source| {
        ComposePublishedPortPlanError::HostPortAvailability {
            host_ip: host_ip.to_owned(),
            host_port,
            source,
        }
    })
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
        ComposePublishedPortEndpoint, ComposePublishedPortHostIp, ComposePublishedPortMapping,
        ComposePublishedPortPlannedEndpointProbe, ComposePublishedPortReservation,
        test_support::{
            forward_port, host_port_probe_from_availability, plan_with_availability, planning_input,
        },
    };

    fn mapping(host_ip: ComposePublishedPortHostIp, host_port: u16) -> ComposePublishedPortMapping {
        ComposePublishedPortMapping {
            service: "app".to_owned(),
            port_entry_index: 0,
            target_port: 502,
            protocol: ComposePortProtocol::Tcp,
            endpoint: ComposePublishedPortEndpoint { host_ip, host_port },
        }
    }

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
        assert_eq!(
            entry.planned_endpoint_probe,
            ComposePublishedPortPlannedEndpointProbe::Available
        );
    }

    #[test]
    fn planner_keeps_requested_endpoint_when_probe_is_unprobeable() {
        let input = planning_input(
            json!({
                "services": {
                    "app": {
                        "ports": [{"target": 502, "published": "502"}]
                    }
                }
            }),
            "app",
            &[],
        );

        let plan = plan_compose_published_ports_with(&input, true, &[], &[], |_, port| {
            assert_eq!(port, 502);
            Ok(HostPortProbe::Unprobeable)
        })
        .unwrap();

        let entry = &plan.entries[0];
        assert_eq!(entry.requested.host_port, 502);
        assert_eq!(entry.planned.host_port, 502);
        assert!(!entry.relocated);
        assert_eq!(
            entry.allocation_reason,
            ComposePublishedPortAllocationReason::Available
        );
        assert_eq!(
            entry.planned_endpoint_probe,
            ComposePublishedPortPlannedEndpointProbe::Unprobeable
        );
    }

    #[test]
    fn planner_reserves_running_project_binding_before_accepting_unprobeable_requested_endpoint() {
        let input = planning_input(
            json!({
                "services": {
                    "app": {
                        "ports": [{"target": 502, "published": "502"}]
                    }
                }
            }),
            "app",
            &[],
        );
        let existing_project_published_ports = vec![ComposePublishedPortReservation {
            service: "stale".to_owned(),
            target_port: 502,
            protocol: ComposePortProtocol::Tcp,
            endpoint: ComposePublishedPortEndpoint {
                host_ip: ComposePublishedPortHostIp::Explicit("0.0.0.0".to_owned()),
                host_port: 502,
            },
            source: ComposePublishedPortReservationSource::RunningContainer,
        }];
        let mut probed_ports = Vec::new();

        let plan = plan_compose_published_ports_with(
            &input,
            true,
            &[],
            &existing_project_published_ports,
            |_, port| {
                probed_ports.push(port);
                Ok(match port {
                    502 => HostPortProbe::Unprobeable,
                    503 => HostPortProbe::Available,
                    unexpected => panic!("unexpected probe for port {unexpected}"),
                })
            },
        )
        .unwrap();

        let entry = &plan.entries[0];
        assert_eq!(probed_ports, vec![503]);
        assert_eq!(entry.requested.host_port, 502);
        assert_eq!(entry.planned.host_port, 503);
        assert!(entry.relocated);
        assert_eq!(
            entry.allocation_reason,
            ComposePublishedPortAllocationReason::Reserved
        );
        assert_eq!(
            entry.planned_endpoint_probe,
            ComposePublishedPortPlannedEndpointProbe::Available
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
    fn planner_skips_unprobeable_relocation_candidates() {
        let input = planning_input(
            json!({
                "services": {
                    "app": {
                        "ports": [{"target": 502, "published": "502"}]
                    }
                }
            }),
            "app",
            &[],
        );
        let forwarding = vec![forward_port("0.0.0.0", 502, 8080)];

        let plan = plan_compose_published_ports_with(&input, true, &forwarding, &[], |_, port| {
            Ok(match port {
                503 | 504 => HostPortProbe::Unprobeable,
                505 => HostPortProbe::Available,
                unexpected => panic!("unexpected probe for port {unexpected}"),
            })
        })
        .unwrap();

        let entry = &plan.entries[0];
        assert_eq!(entry.requested.host_port, 502);
        assert_eq!(entry.planned.host_port, 505);
        assert!(entry.relocated);
        assert_eq!(
            entry.allocation_reason,
            ComposePublishedPortAllocationReason::Reserved
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

        let error = plan_compose_published_ports_with(&input, true, &[], &[], |_, _| {
            Ok(HostPortProbe::Occupied)
        })
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
            | ComposePublishedPortPlanError::InconsistentEntry { .. }
            | ComposePublishedPortPlanError::MappingConflict { .. } => {
                panic!("expected no-candidate error")
            }
        }
    }

    #[test]
    fn planner_keeps_unexpected_probe_errors_as_host_port_availability() {
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
        .expect_err("unexpected probe error should stay fallible");

        match error {
            ComposePublishedPortPlanError::HostPortAvailability {
                host_ip,
                host_port,
                source,
            } => {
                assert_eq!(host_ip, "0.0.0.0");
                assert_eq!(host_port, 3000);
                assert!(source.to_string().contains("socket probe failed"));
            }
            other @ (ComposePublishedPortPlanError::NoRelocationCandidate { .. }
            | ComposePublishedPortPlanError::InconsistentEntry { .. }
            | ComposePublishedPortPlanError::MappingConflict { .. }) => {
                panic!("expected host port availability error, got {other:?}")
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

        let error = plan_compose_published_ports_with(&input, true, &[], &[], |_, _| {
            Ok(HostPortProbe::Available)
        })
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
            | ComposePublishedPortPlanError::HostPortAvailability { .. }
            | ComposePublishedPortPlanError::MappingConflict { .. }) => {
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
            | ComposePublishedPortPlanError::HostPortAvailability { .. }
            | ComposePublishedPortPlanError::MappingConflict { .. }) => {
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
            |_, _| Ok(HostPortProbe::Occupied),
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
            |_, _| Ok(HostPortProbe::Occupied),
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
            |_, _| Ok(HostPortProbe::Occupied),
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
            |_, _| Ok(HostPortProbe::Occupied),
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
            |_, _| Ok(HostPortProbe::Available),
        )
        .unwrap();

        assert_eq!(plan.entries[0].planned.host_port, 3001);
        assert!(plan.entries[0].relocated);
    }

    #[test]
    fn planner_reuses_stopped_existing_binding_when_probe_is_unprobeable() {
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
                host_port: 502,
            },
            source: ComposePublishedPortReservationSource::StoppedContainer,
        }];

        let plan = plan_compose_published_ports_with(
            &input,
            true,
            &[],
            &existing_project_published_ports,
            |_, port| {
                assert_eq!(port, 502);
                Ok(HostPortProbe::Unprobeable)
            },
        )
        .unwrap();

        let entry = &plan.entries[0];
        assert_eq!(entry.planned.host_port, 502);
        assert!(entry.relocated);
        assert_eq!(
            entry.allocation_reason,
            ComposePublishedPortAllocationReason::Unavailable
        );
        assert_eq!(
            entry.planned_endpoint_probe,
            ComposePublishedPortPlannedEndpointProbe::Unprobeable
        );
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
            |_, port| Ok(host_port_probe_from_availability(port != 3000)),
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
            |_, _| Ok(HostPortProbe::Occupied),
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

        let plan = plan_compose_published_ports_with(&input, true, &forwarding, &[], |_, _| {
            Ok(HostPortProbe::Available)
        })
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

        let plan = plan_compose_published_ports_with(&input, true, &forwarding, &[], |_, _| {
            Ok(HostPortProbe::Available)
        })
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

    #[test]
    fn explicit_mapping_applies_when_automatic_relocation_is_disabled() {
        let input = planning_input(
            json!({
                "services": {
                    "app": {
                        "ports": [{"host_ip": "127.0.0.1", "target": 502, "published": "502"}]
                    }
                }
            }),
            "app",
            &[],
        );
        let mappings = vec![mapping(
            ComposePublishedPortHostIp::Explicit("0.0.0.0".to_owned()),
            1502,
        )];

        let plan = plan_compose_published_ports_with_mappings(
            &input,
            false,
            &[],
            &mappings,
            &[],
            &[],
            |host_ip, port| {
                assert_eq!((host_ip, port), ("0.0.0.0", 1502));
                Ok(HostPortProbe::Available)
            },
        )
        .unwrap();

        assert_eq!(plan.entries.len(), 1);
        assert_eq!(plan.entries[0].planned, mappings[0].endpoint);
        assert!(plan.entries[0].relocated);
        assert_eq!(
            plan.entries[0].allocation_reason,
            ComposePublishedPortAllocationReason::Mapping
        );
    }

    #[test]
    fn explicit_mapping_conflicts_with_unmapped_requested_endpoint_without_relocation() {
        let input = planning_input(
            json!({
                "services": {
                    "app": {
                        "ports": [
                            {"host_ip": "127.0.0.1", "target": 502, "published": "502"},
                            {"host_ip": "127.0.0.1", "target": 3000, "published": "1502"}
                        ]
                    }
                }
            }),
            "app",
            &[],
        );
        let mappings = vec![mapping(
            ComposePublishedPortHostIp::Explicit("127.0.0.1".to_owned()),
            1502,
        )];

        let error = plan_compose_published_ports_with_mappings(
            &input,
            false,
            &[],
            &mappings,
            &[],
            &[],
            |host_ip, port| {
                assert_eq!((host_ip, port), ("127.0.0.1", 1502));
                Ok(HostPortProbe::Available)
            },
        )
        .expect_err("mapping endpoint must conflict with the unchanged requested endpoint");

        match error {
            ComposePublishedPortPlanError::MappingConflict { detail } => {
                assert!(detail.contains("target 502/tcp"));
                assert!(detail.contains("127.0.0.1:1502"));
                assert!(detail.contains("target 3000/tcp"));
                assert!(detail.contains("automatic relocation is disabled"));
            }
            other @ (ComposePublishedPortPlanError::NoRelocationCandidate { .. }
            | ComposePublishedPortPlanError::HostPortAvailability { .. }
            | ComposePublishedPortPlanError::InconsistentEntry { .. }) => {
                panic!("unexpected planner error: {other}")
            }
        }
    }

    #[test]
    fn explicit_mapping_takes_precedence_over_current_binding_without_sticky_reuse() {
        let input = planning_input(
            json!({
                "services": {
                    "app": {"ports": [{"target": 502, "published": "502"}]}
                }
            }),
            "app",
            &[],
        );
        let mappings = vec![mapping(
            ComposePublishedPortHostIp::Explicit("127.0.0.1".to_owned()),
            1502,
        )];
        let existing = vec![ComposePublishedPortReservation {
            service: "app".to_owned(),
            target_port: 502,
            protocol: ComposePortProtocol::Tcp,
            endpoint: ComposePublishedPortEndpoint {
                host_ip: ComposePublishedPortHostIp::Explicit("0.0.0.0".to_owned()),
                host_port: 1502,
            },
            source: ComposePublishedPortReservationSource::RunningContainer,
        }];

        let plan = plan_compose_published_ports_inner(
            &input,
            true,
            &[],
            &mappings,
            ExistingProjectBindings::new(&existing, false),
            &[],
            |_, _| panic!("current entry binding should not be probed as an external conflict"),
        )
        .unwrap();

        assert_eq!(plan.entries[0].planned.host_port, 1502);
        assert_eq!(
            plan.entries[0].allocation_reason,
            ComposePublishedPortAllocationReason::Mapping
        );
    }

    #[test]
    fn explicit_mapping_reports_external_reservation_conflict_without_fallback() {
        let input = planning_input(
            json!({
                "services": {
                    "app": {"ports": [{"target": 502, "published": "502"}]}
                }
            }),
            "app",
            &[],
        );
        let mappings = vec![mapping(
            ComposePublishedPortHostIp::Explicit("127.0.0.1".to_owned()),
            1502,
        )];
        let external = vec![HostPortReservation {
            host_ip: "0.0.0.0".to_owned(),
            host: 1502,
        }];

        let error = plan_compose_published_ports_with_mappings(
            &input,
            true,
            &[],
            &mappings,
            &[],
            &external,
            |_, _| panic!("reserved mapping endpoint must not be probed"),
        )
        .expect_err("mapping must not relocate automatically");

        match error {
            ComposePublishedPortPlanError::MappingConflict { detail } => {
                assert!(detail.contains("app"));
                assert!(detail.contains("127.0.0.1:1502"));
                assert!(detail.contains("do not fall back"));
            }
            other @ (ComposePublishedPortPlanError::NoRelocationCandidate { .. }
            | ComposePublishedPortPlanError::HostPortAvailability { .. }
            | ComposePublishedPortPlanError::InconsistentEntry { .. }) => {
                panic!("unexpected planner error: {other}")
            }
        }
    }

    #[test]
    fn external_running_container_reservation_is_used_for_requested_and_candidate_ports() {
        let input = planning_input(
            json!({
                "services": {
                    "app": {"ports": [{"target": 3000, "published": "3000"}]}
                }
            }),
            "app",
            &[],
        );
        let external = vec![
            HostPortReservation {
                host_ip: "0.0.0.0".to_owned(),
                host: 3000,
            },
            HostPortReservation {
                host_ip: "127.0.0.1".to_owned(),
                host: 3001,
            },
        ];
        let mut probes = Vec::new();

        let plan = plan_compose_published_ports_with_mappings(
            &input,
            true,
            &[],
            &[],
            &[],
            &external,
            |host_ip, port| {
                probes.push((host_ip.to_owned(), port));
                Ok(HostPortProbe::Available)
            },
        )
        .unwrap();

        assert_eq!(plan.entries[0].planned.host_port, 3002);
        assert_eq!(probes, vec![("0.0.0.0".to_owned(), 3002)]);
        assert_eq!(
            plan.entries[0].allocation_reason,
            ComposePublishedPortAllocationReason::Reserved
        );
    }

    #[test]
    fn external_ipv6_reservation_does_not_conflict_with_ipv4_binding() {
        let input = planning_input(
            json!({
                "services": {
                    "app": {"ports": [{"host_ip": "127.0.0.1", "target": 3000, "published": "3000"}]}
                }
            }),
            "app",
            &[],
        );
        let external = vec![HostPortReservation {
            host_ip: "::".to_owned(),
            host: 3000,
        }];

        let plan = plan_compose_published_ports_with_mappings(
            &input,
            true,
            &[],
            &[],
            &[],
            &external,
            |_, _| Ok(HostPortProbe::Available),
        )
        .unwrap();

        assert_eq!(plan.entries[0].planned.host_port, 3000);
        assert!(!plan.entries[0].relocated);
    }
}
