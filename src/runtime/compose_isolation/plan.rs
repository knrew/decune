use std::{
    collections::{BTreeMap, BTreeSet},
    net::{IpAddr, Ipv4Addr},
};

use anyhow::{Result, anyhow, bail};

use crate::runtime::{
    compose_cli::ComposeConfigModel,
    compose_isolation::{
        COMPOSE_CLONE_ISOLATION_INVALID, COMPOSE_CLONE_ISOLATION_POOL_EXHAUSTED,
        COMPOSE_CLONE_ISOLATION_UNSUPPORTED, ComposeIsolationDaemonSnapshot,
        ComposeIsolationDockerIpamConfig, ComposeIsolationDockerNetwork, ComposeIsolationFinding,
        ComposeIsolationNameRewritePlan, ComposeIsolationNetworkRequest,
        ComposeIsolationPersistedSubnet, ComposeIsolationResourceKind,
        ComposeIsolationResourceNameRewrite, ComposeIsolationScan,
        ComposeIsolationServiceNameRewrite, ComposeIsolationSubnetAllocation,
        ComposeIsolationSubnetPlan, Ipv4Cidr, allocate_ipv4_subnet_slot,
    },
};

const COMPOSE_CLONE_ISOLATION_RECREATE_HINT: &str = "Run decune down, then decune rebuild.";

pub(crate) struct ComposeIsolationNameRewritePlanInput<'a> {
    pub(crate) model: &'a ComposeConfigModel,
    pub(crate) scan: &'a ComposeIsolationScan,
    pub(crate) workspace_id: &'a str,
    pub(crate) enabled: bool,
    pub(crate) rewrite_container_names: bool,
    pub(crate) rewrite_resource_names: bool,
}

pub(crate) fn plan_compose_isolation_name_rewrites(
    input: &ComposeIsolationNameRewritePlanInput<'_>,
) -> ComposeIsolationNameRewritePlan {
    if !input.enabled {
        return ComposeIsolationNameRewritePlan::default();
    }

    let mut plan = ComposeIsolationNameRewritePlan::default();
    for fixed in &input.scan.fixed_names {
        let rewritten_name = format!("{}-{}", fixed.name, input.workspace_id);
        match fixed.kind {
            ComposeIsolationResourceKind::ServiceContainer if input.rewrite_container_names => {
                let networks = input
                    .model
                    .service(&fixed.resource)
                    .map(|service| service.network_names().cloned().collect())
                    .unwrap_or_default();
                plan.services.push(ComposeIsolationServiceNameRewrite {
                    service: fixed.resource.clone(),
                    original_name: fixed.name.clone(),
                    rewritten_name,
                    networks,
                });
            }
            ComposeIsolationResourceKind::Network
            | ComposeIsolationResourceKind::Volume
            | ComposeIsolationResourceKind::Config
            | ComposeIsolationResourceKind::Secret
                if input.rewrite_resource_names =>
            {
                plan.resources.push(ComposeIsolationResourceNameRewrite {
                    kind: fixed.kind,
                    resource: fixed.resource.clone(),
                    original_name: fixed.name.clone(),
                    rewritten_name,
                });
            }
            ComposeIsolationResourceKind::ServiceContainer
            | ComposeIsolationResourceKind::Network
            | ComposeIsolationResourceKind::Volume
            | ComposeIsolationResourceKind::Config
            | ComposeIsolationResourceKind::Secret => {}
        }
    }
    plan
}

pub(crate) fn apply_compose_isolation_name_rewrites(
    scan: &ComposeIsolationScan,
    plan: &ComposeIsolationNameRewritePlan,
) -> ComposeIsolationScan {
    let mut effective = scan.clone();
    for fixed in &mut effective.fixed_names {
        let rewritten = if fixed.kind == ComposeIsolationResourceKind::ServiceContainer {
            plan.services
                .iter()
                .find(|rewrite| rewrite.service == fixed.resource)
                .map(|rewrite| &rewrite.rewritten_name)
        } else {
            plan.resources
                .iter()
                .find(|rewrite| rewrite.kind == fixed.kind && rewrite.resource == fixed.resource)
                .map(|rewrite| &rewrite.rewritten_name)
        };
        if let Some(rewritten) = rewritten {
            fixed.name.clone_from(rewritten);
        }
    }
    effective
}

pub(crate) struct ComposeIsolationSubnetPlanInput<'a> {
    pub(crate) model: &'a ComposeConfigModel,
    pub(crate) project_name: &'a str,
    pub(crate) workspace_id: &'a str,
    pub(crate) scan: &'a ComposeIsolationScan,
    pub(crate) daemon: &'a ComposeIsolationDaemonSnapshot,
    pub(crate) state: &'a [ComposeIsolationPersistedSubnet],
    pub(crate) enabled: bool,
    pub(crate) relocation: bool,
    pub(crate) subnet_pool: Option<&'a str>,
    pub(crate) subnet_prefix: Option<u8>,
    pub(crate) rebuild: bool,
}

struct ValidatedRelocationRequest<'a> {
    requested: &'a ComposeIsolationNetworkRequest,
    cidr: Ipv4Cidr,
}

pub(crate) fn plan_compose_isolation_subnets(
    input: &ComposeIsolationSubnetPlanInput<'_>,
) -> Result<ComposeIsolationSubnetPlan> {
    if !input.enabled || !input.relocation || input.scan.networks.is_empty() {
        return Ok(ComposeIsolationSubnetPlan::default());
    }
    let validated_requests = validate_relocation_requests(input)?;
    let pool_text = input.subnet_pool.ok_or_else(|| {
        anyhow!(
            "{COMPOSE_CLONE_ISOLATION_INVALID}: compose.clone_isolation.networks.subnet_pool is required when network relocation is enabled"
        )
    })?;
    let pool = Ipv4Cidr::parse(pool_text).ok_or_else(|| {
        anyhow!(
            "{COMPOSE_CLONE_ISOLATION_INVALID}: clone isolation subnet pool is not a valid IPv4 CIDR: {pool_text}"
        )
    })?;
    let mut plan = ComposeIsolationSubnetPlan::default();
    let mut assigned = Vec::new();

    for validated in validated_requests {
        let requested = validated.requested;
        let requested_cidr = validated.cidr;
        let prefix = input
            .subnet_prefix
            .unwrap_or_else(|| requested_cidr.prefix());
        if prefix < pool.prefix() {
            bail!(
                "{COMPOSE_CLONE_ISOLATION_INVALID}: subnet prefix {prefix} for Compose network `{}` does not fit subnet pool {pool}",
                requested.network
            );
        }
        let unavailable = unavailable_subnets(input, requested, &assigned);
        let stable = if input.rebuild {
            None
        } else {
            preferred_existing_subnet(input, requested, pool, prefix, &unavailable)
                .or_else(|| preferred_state_subnet(input, requested, pool, prefix, &unavailable))
        };
        let requested_on_rebuild = input
            .rebuild
            .then_some(requested_cidr)
            .filter(|candidate| candidate.prefix() == prefix)
            .filter(|candidate| subnet_is_available(*candidate, pool, &unavailable));
        let planned = stable
            .or(requested_on_rebuild)
            .or_else(|| {
                allocate_ipv4_subnet_slot(
                    pool,
                    prefix,
                    input.workspace_id,
                    &requested.network,
                    &unavailable,
                )
            })
            .ok_or_else(|| {
                anyhow!(
                    "{COMPOSE_CLONE_ISOLATION_POOL_EXHAUSTED}: no available /{prefix} subnet remains in pool {pool} for Compose network `{}`",
                    requested.network
                )
            })?;
        let planned_gateway = relocate_gateway(requested, requested_cidr, planned)?;
        let planned_ip_range = relocate_ip_range(requested, requested_cidr, planned)?;
        let planned_aux_addresses = relocate_aux_addresses(requested, requested_cidr, planned)?;
        assigned.push(planned);
        plan.allocations.push(ComposeIsolationSubnetAllocation {
            network: requested.network.clone(),
            requested_subnet: requested.subnet.clone(),
            planned_subnet: planned.to_string(),
            planned_gateway,
            planned_ip_range,
            planned_aux_addresses,
            relocated: planned != requested_cidr,
        });
    }

    plan.networks_to_remove = networks_to_recreate(input, &plan.allocations)?;
    Ok(plan)
}

pub(crate) fn apply_compose_isolation_subnet_plan(
    scan: &ComposeIsolationScan,
    plan: &ComposeIsolationSubnetPlan,
) -> ComposeIsolationScan {
    let mut effective = scan.clone();
    for requested in &mut effective.networks {
        if let Some(allocation) = plan.allocations.iter().find(|allocation| {
            (&allocation.network, &allocation.requested_subnet)
                == (&requested.network, &requested.subnet)
        }) {
            requested.subnet.clone_from(&allocation.planned_subnet);
            requested.gateway.clone_from(&allocation.planned_gateway);
            requested.ip_range.clone_from(&allocation.planned_ip_range);
            requested
                .aux_addresses
                .clone_from(&allocation.planned_aux_addresses);
        }
    }
    effective
}

fn validate_relocation_requests<'a>(
    input: &'a ComposeIsolationSubnetPlanInput<'_>,
) -> Result<Vec<ValidatedRelocationRequest<'a>>> {
    let mut networks = BTreeSet::new();
    let mut validated = Vec::with_capacity(input.scan.networks.len());
    for requested in &input.scan.networks {
        if requested.has_unrepresented_ipam_configs {
            bail!(
                "{COMPOSE_CLONE_ISOLATION_UNSUPPORTED}: Compose network `{}` IPAM config entries without subnet cannot be relocated safely",
                requested.network
            );
        }
        if let Some(field) = requested.unsupported_ipam_fields.iter().next() {
            bail!(
                "{COMPOSE_CLONE_ISOLATION_UNSUPPORTED}: Compose network `{}` IPAM field `{field}` cannot be relocated safely",
                requested.network
            );
        }
        let cidr = Ipv4Cidr::parse(&requested.subnet).ok_or_else(|| {
            anyhow!(
                "{COMPOSE_CLONE_ISOLATION_UNSUPPORTED}: fixed IPv6 Compose network subnets cannot be relocated; network `{}`; subnet {}",
                requested.network,
                requested.subnet
            )
        })?;
        if !networks.insert(&requested.network) {
            bail!(
                "{COMPOSE_CLONE_ISOLATION_UNSUPPORTED}: multiple fixed IPAM subnets on Compose network `{}` cannot be relocated",
                requested.network
            );
        }
        for (service_name, service) in input.model.services() {
            let Some(network_config) = service.network_config(&requested.network) else {
                continue;
            };
            let Some(network_config) = network_config.as_object() else {
                continue;
            };
            for key in ["ipv4_address", "ipv6_address", "link_local_ips"] {
                if network_config.contains_key(key) {
                    bail!(
                        "{COMPOSE_CLONE_ISOLATION_UNSUPPORTED}: static service network addressing cannot be relocated; service `{service_name}`; network `{}`; field `{key}`",
                        requested.network
                    );
                }
            }
        }
        validated.push(ValidatedRelocationRequest { requested, cidr });
    }
    Ok(validated)
}

fn unavailable_subnets(
    input: &ComposeIsolationSubnetPlanInput<'_>,
    requested: &ComposeIsolationNetworkRequest,
    assigned: &[Ipv4Cidr],
) -> Vec<Ipv4Cidr> {
    let mut unavailable = assigned.to_vec();
    for network in &input.daemon.networks {
        let is_requested_self_network =
            is_self_project(network.compose_project.as_deref(), input.project_name)
                && network.compose_network.as_deref() == Some(requested.network.as_str());
        if is_requested_self_network || !uses_same_ipam_address_space(requested, network) {
            continue;
        }
        unavailable.extend(
            network
                .ipam_configs
                .iter()
                .filter_map(|config| config.subnet.as_deref().and_then(Ipv4Cidr::parse)),
        );
    }
    unavailable
}

fn preferred_existing_subnet(
    input: &ComposeIsolationSubnetPlanInput<'_>,
    requested: &ComposeIsolationNetworkRequest,
    pool: Ipv4Cidr,
    prefix: u8,
    unavailable: &[Ipv4Cidr],
) -> Option<Ipv4Cidr> {
    input
        .daemon
        .networks
        .iter()
        .filter(|network| {
            is_self_project(network.compose_project.as_deref(), input.project_name)
                && network.compose_network.as_deref() == Some(requested.network.as_str())
        })
        .flat_map(|network| &network.ipam_configs)
        .filter_map(|config| config.subnet.as_deref().and_then(Ipv4Cidr::parse))
        .find(|candidate| {
            candidate.prefix() == prefix && subnet_is_available(*candidate, pool, unavailable)
        })
}

fn preferred_state_subnet(
    input: &ComposeIsolationSubnetPlanInput<'_>,
    requested: &ComposeIsolationNetworkRequest,
    pool: Ipv4Cidr,
    prefix: u8,
    unavailable: &[Ipv4Cidr],
) -> Option<Ipv4Cidr> {
    input
        .state
        .iter()
        .find(|state| {
            (&state.network, &state.requested_subnet) == (&requested.network, &requested.subnet)
        })
        .and_then(|state| Ipv4Cidr::parse(&state.planned_subnet))
        .filter(|candidate| candidate.prefix() == prefix)
        .filter(|candidate| subnet_is_available(*candidate, pool, unavailable))
}

fn subnet_is_available(candidate: Ipv4Cidr, pool: Ipv4Cidr, unavailable: &[Ipv4Cidr]) -> bool {
    pool.contains(candidate)
        && unavailable
            .iter()
            .all(|existing| !candidate.overlaps(*existing))
}

fn relocate_gateway(
    requested: &ComposeIsolationNetworkRequest,
    requested_cidr: Ipv4Cidr,
    planned: Ipv4Cidr,
) -> Result<Option<String>> {
    let Some(gateway) = requested.gateway.as_deref() else {
        return Ok(None);
    };
    let gateway = gateway.parse::<Ipv4Addr>().map_err(|error| {
        anyhow!(
            "{COMPOSE_CLONE_ISOLATION_INVALID}: gateway for Compose network `{}` is not a valid IPv4 address: {gateway}: {error}",
            requested.network,
        )
    })?;
    if !requested_cidr.contains_address(gateway) {
        bail!(
            "{COMPOSE_CLONE_ISOLATION_INVALID}: gateway {gateway} is outside requested subnet {} for Compose network `{}`",
            requested.subnet,
            requested.network
        );
    }
    let offset = u32::from(gateway) - requested_cidr.network();
    let planned_gateway = planned.address_at_offset(offset).ok_or_else(|| {
        anyhow!(
            "{COMPOSE_CLONE_ISOLATION_INVALID}: gateway host offset from {gateway} does not fit planned subnet {planned} for Compose network `{}`",
            requested.network
        )
    })?;
    Ok(Some(planned_gateway.to_string()))
}

fn relocate_ip_range(
    requested: &ComposeIsolationNetworkRequest,
    requested_cidr: Ipv4Cidr,
    planned: Ipv4Cidr,
) -> Result<Option<String>> {
    let Some(ip_range) = requested.ip_range.as_deref() else {
        return Ok(None);
    };
    let requested_range = Ipv4Cidr::parse(ip_range).ok_or_else(|| {
        let is_ipv6 = ip_range
            .split_once('/')
            .and_then(|(address, _)| address.parse::<IpAddr>().ok())
            .is_some_and(|address| address.is_ipv6());
        let code = if is_ipv6 {
            COMPOSE_CLONE_ISOLATION_UNSUPPORTED
        } else {
            COMPOSE_CLONE_ISOLATION_INVALID
        };
        anyhow!(
            "{code}: Compose network `{}` IPAM field `ip_range` is not a relocatable IPv4 CIDR",
            requested.network
        )
    })?;
    if !requested_cidr.contains(requested_range) {
        bail!(
            "{COMPOSE_CLONE_ISOLATION_INVALID}: Compose network `{}` IPAM field `ip_range` is outside its requested subnet",
            requested.network
        );
    }
    let offset = requested_range.network() - requested_cidr.network();
    let Some(planned_address) = planned.address_at_offset(offset) else {
        bail!(
            "{COMPOSE_CLONE_ISOLATION_INVALID}: Compose network `{}` IPAM field `ip_range` does not fit the planned subnet; choose a subnet_prefix that preserves the requested range offset",
            requested.network
        );
    };
    let planned_range = Ipv4Cidr::parse(&format!(
        "{planned_address}/{}",
        requested_range.prefix()
    ))
    .filter(|range| range.network() == u32::from(planned_address) && planned.contains(*range))
    .ok_or_else(|| {
        anyhow!(
            "{COMPOSE_CLONE_ISOLATION_INVALID}: Compose network `{}` IPAM field `ip_range` does not fit the planned subnet; choose a subnet_prefix that preserves the requested range offset",
            requested.network
        )
    })?;
    Ok(Some(planned_range.to_string()))
}

fn relocate_aux_addresses(
    requested: &ComposeIsolationNetworkRequest,
    requested_cidr: Ipv4Cidr,
    planned: Ipv4Cidr,
) -> Result<BTreeMap<String, String>> {
    requested
        .aux_addresses
        .iter()
        .map(|(name, address)| {
            let address = match address.parse::<IpAddr>() {
                Ok(IpAddr::V4(address)) => address,
                Ok(IpAddr::V6(_)) => {
                    bail!(
                        "{COMPOSE_CLONE_ISOLATION_UNSUPPORTED}: Compose network `{}` IPAM field `aux_addresses` contains a non-IPv4 address that cannot be relocated",
                        requested.network
                    );
                }
                Err(_) => {
                    bail!(
                        "{COMPOSE_CLONE_ISOLATION_INVALID}: Compose network `{}` IPAM field `aux_addresses` contains an invalid IP address",
                        requested.network
                    );
                }
            };
            if !requested_cidr.contains_address(address) {
                bail!(
                    "{COMPOSE_CLONE_ISOLATION_INVALID}: Compose network `{}` IPAM field `aux_addresses` contains an address outside its requested subnet",
                    requested.network
                );
            }
            let offset = u32::from(address) - requested_cidr.network();
            let planned_address = planned.address_at_offset(offset).ok_or_else(|| {
                anyhow!(
                    "{COMPOSE_CLONE_ISOLATION_INVALID}: Compose network `{}` IPAM field `aux_addresses` does not fit the planned subnet; choose a subnet_prefix that preserves every requested address offset",
                    requested.network
                )
            })?;
            Ok((name.clone(), planned_address.to_string()))
        })
        .collect()
}

fn networks_to_recreate(
    input: &ComposeIsolationSubnetPlanInput<'_>,
    allocations: &[ComposeIsolationSubnetAllocation],
) -> Result<Vec<String>> {
    let mut removals = BTreeSet::new();
    for allocation in allocations {
        for network in input.daemon.networks.iter().filter(|network| {
            is_self_project(network.compose_project.as_deref(), input.project_name)
                && network.compose_network.as_deref() == Some(allocation.network.as_str())
        }) {
            let matches_plan = network
                .ipam_configs
                .iter()
                .any(|config| docker_ipam_matches_allocation(config, allocation));
            if matches_plan {
                continue;
            }
            if network.has_attached_containers {
                bail!(
                    "{COMPOSE_CLONE_ISOLATION_INVALID}: Docker network `{}` for Compose network `{}` must be recreated with subnet {}, but containers are still attached. {COMPOSE_CLONE_ISOLATION_RECREATE_HINT}",
                    network.name,
                    allocation.network,
                    allocation.planned_subnet
                );
            }
            removals.insert(network.name.clone());
        }
    }
    Ok(removals.into_iter().collect())
}

fn docker_ipam_matches_allocation(
    config: &ComposeIsolationDockerIpamConfig,
    allocation: &ComposeIsolationSubnetAllocation,
) -> bool {
    let subnet_matches = config.subnet.as_deref().and_then(Ipv4Cidr::parse)
        == Ipv4Cidr::parse(&allocation.planned_subnet);
    let gateway_matches = allocation
        .planned_gateway
        .as_deref()
        .is_none_or(|planned| config.gateway.as_deref().map(str::trim) == Some(planned));
    let ip_range_matches = allocation.planned_ip_range.as_deref().map_or_else(
        || {
            config
                .ip_range
                .as_deref()
                .is_none_or(|value| value.trim().is_empty())
        },
        |planned| config.ip_range.as_deref().and_then(Ipv4Cidr::parse) == Ipv4Cidr::parse(planned),
    );
    subnet_matches
        && gateway_matches
        && ip_range_matches
        && config.auxiliary_addresses == allocation.planned_aux_addresses
}

pub(crate) struct ComposeIsolationPlanInput<'a> {
    pub(crate) project_name: &'a str,
    pub(crate) scan: &'a ComposeIsolationScan,
    pub(crate) daemon: &'a ComposeIsolationDaemonSnapshot,
}

pub(crate) fn plan_compose_isolation(
    input: &ComposeIsolationPlanInput<'_>,
) -> Vec<ComposeIsolationFinding> {
    let mut findings = Vec::new();
    findings.extend(plan_subnet_overlaps(input));
    findings.extend(plan_fixed_name_conflicts(input));
    findings
}

fn plan_subnet_overlaps(input: &ComposeIsolationPlanInput<'_>) -> Vec<ComposeIsolationFinding> {
    let mut findings = Vec::new();
    for requested in &input.scan.networks {
        let Some(requested_subnet) = Ipv4Cidr::parse(&requested.subnet) else {
            continue;
        };
        for network in &input.daemon.networks {
            if is_self_project(network.compose_project.as_deref(), input.project_name) {
                continue;
            }
            if !uses_same_ipam_address_space(requested, network) {
                continue;
            }
            for existing_config in &network.ipam_configs {
                let Some(existing_subnet_text) = existing_config.subnet.as_deref() else {
                    continue;
                };
                let Some(existing_subnet) = Ipv4Cidr::parse(existing_subnet_text) else {
                    continue;
                };
                if requested_subnet.overlaps(existing_subnet) {
                    findings.push(ComposeIsolationFinding::NetworkSubnetOverlap {
                        compose_network: requested.network.clone(),
                        requested_subnet: requested.subnet.clone(),
                        requested_gateway: requested.gateway.clone(),
                        docker_network: network.name.clone(),
                        docker_project: network.compose_project.clone(),
                        docker_subnet: existing_subnet_text.to_owned(),
                        docker_gateway: existing_config.gateway.clone(),
                    });
                }
            }
        }
    }
    findings
}

fn plan_fixed_name_conflicts(
    input: &ComposeIsolationPlanInput<'_>,
) -> Vec<ComposeIsolationFinding> {
    let mut findings = Vec::new();
    for fixed in &input.scan.fixed_names {
        for existing in &input.daemon.resources {
            if fixed.kind != existing.kind || fixed.name != existing.name {
                continue;
            }
            if is_self_project(existing.compose_project.as_deref(), input.project_name) {
                continue;
            }
            findings.push(ComposeIsolationFinding::FixedNameConflict {
                kind: fixed.kind,
                compose_resource: fixed.resource.clone(),
                requested_name: fixed.name.clone(),
                docker_resource_name: existing.name.clone(),
                docker_project: existing.compose_project.clone(),
            });
        }
    }
    findings
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum IpamAddressSpace {
    Local,
    Global,
}

fn uses_same_ipam_address_space(
    requested: &ComposeIsolationNetworkRequest,
    existing: &ComposeIsolationDockerNetwork,
) -> bool {
    if effective_ipam_driver(requested.ipam_driver.as_deref())
        != effective_ipam_driver(existing.ipam_driver.as_deref())
    {
        return false;
    }

    match (
        requested_address_space(requested.driver.as_deref()),
        existing_address_space(existing.scope.as_deref()),
    ) {
        (Some(requested), Some(existing)) => requested == existing,
        _ => true,
    }
}

fn effective_ipam_driver(driver: Option<&str>) -> &str {
    driver
        .map(str::trim)
        .filter(|driver| !driver.is_empty())
        .unwrap_or("default")
}

fn requested_address_space(driver: Option<&str>) -> Option<IpamAddressSpace> {
    match driver.map(str::trim).filter(|driver| !driver.is_empty()) {
        None | Some("bridge" | "macvlan" | "ipvlan") => Some(IpamAddressSpace::Local),
        Some("overlay") => Some(IpamAddressSpace::Global),
        Some(_) => None,
    }
}

fn existing_address_space(scope: Option<&str>) -> Option<IpamAddressSpace> {
    match scope.map(str::trim).filter(|scope| !scope.is_empty()) {
        Some("local") => Some(IpamAddressSpace::Local),
        Some("swarm" | "global") => Some(IpamAddressSpace::Global),
        None | Some(_) => None,
    }
}

fn is_self_project(compose_project: Option<&str>, project_name: &str) -> bool {
    compose_project == Some(project_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::compose_isolation::{
        ComposeIsolationDockerNetwork, ComposeIsolationDockerResource,
        ComposeIsolationFixedNameRequest, ComposeIsolationNetworkRequest,
        ComposeIsolationPersistedSubnet, ComposeIsolationResourceKind, allocate_ipv4_subnet_slot,
        scan_compose_isolation,
    };

    #[test]
    fn subnet_planner_skips_occupied_initial_slot_and_preserves_gateway_offset() {
        let model: ComposeConfigModel = serde_json::from_value(serde_json::json!({
            "services": {"app": {"image": "alpine:3.20", "networks": {"grpc": null}}},
            "networks": {"grpc": {"ipam": {"config": [{
                "subnet": "10.99.0.0/24",
                "gateway": "10.99.0.1",
                "ip_range": "10.99.0.128/25",
                "aux_addresses": {
                    "reserved-a": "10.99.0.10",
                    "reserved-b": "10.99.0.11"
                }
            }]}}}
        }))
        .unwrap();
        let scan = scan_compose_isolation(&model, "self-project");
        let pool = Ipv4Cidr::parse("10.200.0.0/16").unwrap();
        let initial = allocate_ipv4_subnet_slot(pool, 24, "workspace-a", "grpc", &[]).unwrap();
        let daemon = ComposeIsolationDaemonSnapshot {
            networks: vec![network(
                "occupied",
                Some("other-project"),
                &initial.to_string(),
            )],
            resources: Vec::new(),
        };

        let plan = plan_compose_isolation_subnets(&ComposeIsolationSubnetPlanInput {
            model: &model,
            project_name: "self-project",
            workspace_id: "workspace-a",
            scan: &scan,
            daemon: &daemon,
            state: &[],
            enabled: true,
            relocation: true,
            subnet_pool: Some("10.200.0.0/16"),
            subnet_prefix: Some(24),
            rebuild: false,
        })
        .unwrap();

        assert_eq!(plan.allocations.len(), 1);
        assert_ne!(plan.allocations[0].planned_subnet, initial.to_string());
        let planned = Ipv4Cidr::parse(&plan.allocations[0].planned_subnet).unwrap();
        assert_eq!(
            plan.allocations[0].planned_gateway,
            planned
                .address_at_offset(1)
                .map(|address| address.to_string())
        );
        assert_eq!(
            plan.allocations[0].planned_ip_range,
            planned
                .address_at_offset(128)
                .map(|address| format!("{address}/25"))
        );
        assert_eq!(
            plan.allocations[0].planned_aux_addresses,
            BTreeMap::from([
                (
                    "reserved-a".to_owned(),
                    planned.address_at_offset(10).unwrap().to_string(),
                ),
                (
                    "reserved-b".to_owned(),
                    planned.address_at_offset(11).unwrap().to_string(),
                ),
            ])
        );
    }

    #[test]
    fn ipam_field_relocation_rejects_ranges_and_addresses_that_do_not_fit() {
        let requested = ComposeIsolationNetworkRequest {
            network: "grpc".to_owned(),
            driver: None,
            ipam_driver: None,
            subnet: "10.99.0.0/24".to_owned(),
            gateway: None,
            ip_range: Some("10.99.0.128/25".to_owned()),
            aux_addresses: BTreeMap::from([("reserved".to_owned(), "10.99.0.200".to_owned())]),
            has_unrepresented_ipam_configs: false,
            unsupported_ipam_fields: BTreeSet::new(),
        };
        let requested_cidr = Ipv4Cidr::parse(&requested.subnet).unwrap();
        let planned = Ipv4Cidr::parse("10.200.0.0/25").unwrap();

        let range_error = relocate_ip_range(&requested, requested_cidr, planned).unwrap_err();
        assert!(
            range_error
                .to_string()
                .contains(COMPOSE_CLONE_ISOLATION_INVALID)
        );
        assert!(range_error.to_string().contains("ip_range"));
        assert!(!range_error.to_string().contains("10.99.0.128"));

        let aux_error = relocate_aux_addresses(&requested, requested_cidr, planned).unwrap_err();
        assert!(
            aux_error
                .to_string()
                .contains(COMPOSE_CLONE_ISOLATION_INVALID)
        );
        assert!(aux_error.to_string().contains("aux_addresses"));
        assert!(!aux_error.to_string().contains("10.99.0.200"));

        let mut outside = requested;
        outside.ip_range = Some("10.99.1.0/24".to_owned());
        outside.aux_addresses.clear();
        let outside_error = relocate_ip_range(&outside, requested_cidr, planned).unwrap_err();
        assert!(
            outside_error
                .to_string()
                .contains("outside its requested subnet")
        );
        assert!(!outside_error.to_string().contains("10.99.1.0"));
    }

    #[test]
    fn ipam_field_relocation_distinguishes_ipv6_from_invalid_values() {
        let requested_cidr = Ipv4Cidr::parse("10.99.0.0/24").unwrap();
        let planned = Ipv4Cidr::parse("10.200.0.0/24").unwrap();
        for (value, expected_code) in [
            ("fd00::/64", COMPOSE_CLONE_ISOLATION_UNSUPPORTED),
            ("not-a-cidr", COMPOSE_CLONE_ISOLATION_INVALID),
        ] {
            let mut requested = ComposeIsolationNetworkRequest {
                network: "grpc".to_owned(),
                driver: None,
                ipam_driver: None,
                subnet: requested_cidr.to_string(),
                gateway: None,
                ip_range: Some(value.to_owned()),
                aux_addresses: BTreeMap::new(),
                has_unrepresented_ipam_configs: false,
                unsupported_ipam_fields: BTreeSet::new(),
            };
            let error = relocate_ip_range(&requested, requested_cidr, planned).unwrap_err();
            assert!(error.to_string().contains(expected_code));
            assert!(!error.to_string().contains(value));

            requested.ip_range = None;
            requested.aux_addresses.insert(
                "reserved".to_owned(),
                value.trim_end_matches("/64").to_owned(),
            );
            let error = relocate_aux_addresses(&requested, requested_cidr, planned).unwrap_err();
            assert!(error.to_string().contains(expected_code));
            assert!(!error.to_string().contains(value.trim_end_matches("/64")));
        }
    }

    #[test]
    fn subnet_planner_rejects_unknown_ipam_fields_without_printing_values() {
        let model: ComposeConfigModel = serde_json::from_value(serde_json::json!({
            "services": {"app": {"image": "alpine:3.20", "networks": {"grpc": null}}},
            "networks": {"grpc": {"ipam": {"config": [{
                "subnet": "10.99.0.0/24",
                "future_field": "sensitive-field-value"
            }]}}}
        }))
        .unwrap();
        let scan = scan_compose_isolation(&model, "self-project");

        let error = plan_compose_isolation_subnets(&ComposeIsolationSubnetPlanInput {
            model: &model,
            project_name: "self-project",
            workspace_id: "workspace-a",
            scan: &scan,
            daemon: &ComposeIsolationDaemonSnapshot::default(),
            state: &[],
            enabled: true,
            relocation: true,
            subnet_pool: Some("10.200.0.0/16"),
            subnet_prefix: Some(24),
            rebuild: false,
        })
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains(COMPOSE_CLONE_ISOLATION_UNSUPPORTED)
        );
        assert!(error.to_string().contains("future_field"));
        assert!(!error.to_string().contains("sensitive-field-value"));
    }

    #[test]
    fn subnet_planner_rejects_unrepresented_ipam_configs_without_printing_values() {
        let model: ComposeConfigModel = serde_json::from_value(serde_json::json!({
            "services": {"app": {"image": "alpine:3.20", "networks": {"grpc": null}}},
            "networks": {"grpc": {"ipam": {"config": [
                {"subnet": "10.99.0.0/24"},
                {
                    "ip_range": "sensitive-range-value",
                    "aux_addresses": {"reserved": "sensitive-address-value"}
                }
            ]}}}
        }))
        .unwrap();
        let scan = scan_compose_isolation(&model, "self-project");

        let error = plan_compose_isolation_subnets(&ComposeIsolationSubnetPlanInput {
            model: &model,
            project_name: "self-project",
            workspace_id: "workspace-a",
            scan: &scan,
            daemon: &ComposeIsolationDaemonSnapshot::default(),
            state: &[],
            enabled: true,
            relocation: true,
            subnet_pool: Some("10.200.0.0/16"),
            subnet_prefix: Some(24),
            rebuild: false,
        })
        .unwrap_err();

        let error = error.to_string();
        assert!(error.contains(COMPOSE_CLONE_ISOLATION_UNSUPPORTED));
        assert!(error.contains("network `grpc`"));
        assert!(error.contains("IPAM config entries without subnet"));
        assert!(!error.contains("sensitive-range-value"));
        assert!(!error.contains("sensitive-address-value"));
    }

    #[test]
    fn existing_network_ipam_must_match_planned_range_and_aux_addresses() {
        let allocation = ComposeIsolationSubnetAllocation {
            network: "grpc".to_owned(),
            requested_subnet: "10.99.0.0/24".to_owned(),
            planned_subnet: "10.200.42.0/24".to_owned(),
            planned_gateway: Some("10.200.42.1".to_owned()),
            planned_ip_range: Some("10.200.42.128/25".to_owned()),
            planned_aux_addresses: BTreeMap::from([(
                "reserved".to_owned(),
                "10.200.42.10".to_owned(),
            )]),
            relocated: true,
        };
        let mut existing = ComposeIsolationDockerIpamConfig {
            subnet: Some("10.200.42.0/24".to_owned()),
            gateway: Some("10.200.42.1".to_owned()),
            ip_range: Some("10.200.42.128/25".to_owned()),
            auxiliary_addresses: allocation.planned_aux_addresses.clone(),
        };

        assert!(docker_ipam_matches_allocation(&existing, &allocation));
        existing.ip_range = None;
        assert!(!docker_ipam_matches_allocation(&existing, &allocation));
        existing.ip_range = allocation.planned_ip_range.clone();
        existing.auxiliary_addresses.clear();
        assert!(!docker_ipam_matches_allocation(&existing, &allocation));
    }

    #[test]
    fn gateway_relocation_rejects_gateway_outside_requested_subnet() {
        let requested = ComposeIsolationNetworkRequest {
            network: "grpc".to_owned(),
            driver: None,
            ipam_driver: None,
            subnet: "10.99.0.0/24".to_owned(),
            gateway: Some("10.99.1.1".to_owned()),
            ip_range: None,
            aux_addresses: BTreeMap::new(),
            has_unrepresented_ipam_configs: false,
            unsupported_ipam_fields: BTreeSet::new(),
        };

        let error = relocate_gateway(
            &requested,
            Ipv4Cidr::parse(&requested.subnet).unwrap(),
            Ipv4Cidr::parse("10.200.0.0/24").unwrap(),
        )
        .unwrap_err();

        assert!(error.to_string().contains(COMPOSE_CLONE_ISOLATION_INVALID));
        assert!(error.to_string().contains("outside requested subnet"));
    }

    #[test]
    fn gateway_relocation_rejects_offset_that_does_not_fit_narrower_subnet() {
        let requested = ComposeIsolationNetworkRequest {
            network: "grpc".to_owned(),
            driver: None,
            ipam_driver: None,
            subnet: "10.99.0.0/24".to_owned(),
            gateway: Some("10.99.0.200".to_owned()),
            ip_range: None,
            aux_addresses: BTreeMap::new(),
            has_unrepresented_ipam_configs: false,
            unsupported_ipam_fields: BTreeSet::new(),
        };

        let error = relocate_gateway(
            &requested,
            Ipv4Cidr::parse(&requested.subnet).unwrap(),
            Ipv4Cidr::parse("10.200.0.0/25").unwrap(),
        )
        .unwrap_err();

        assert!(error.to_string().contains(COMPOSE_CLONE_ISOLATION_INVALID));
        assert!(error.to_string().contains("does not fit planned subnet"));
    }

    #[test]
    fn subnet_planner_assigns_distinct_slots_within_one_plan() {
        let model: ComposeConfigModel = serde_json::from_value(serde_json::json!({
            "services": {"app": {"image": "alpine:3.20", "networks": {"a": null, "b": null}}},
            "networks": {
                "a": {"ipam": {"config": [{"subnet": "10.90.0.0/24"}]}},
                "b": {"ipam": {"config": [{"subnet": "10.91.0.0/24"}]}}
            }
        }))
        .unwrap();
        let scan = scan_compose_isolation(&model, "self-project");
        let plan = plan_compose_isolation_subnets(&ComposeIsolationSubnetPlanInput {
            model: &model,
            project_name: "self-project",
            workspace_id: "workspace-a",
            scan: &scan,
            daemon: &ComposeIsolationDaemonSnapshot::default(),
            state: &[],
            enabled: true,
            relocation: true,
            subnet_pool: Some("10.200.0.0/16"),
            subnet_prefix: Some(24),
            rebuild: false,
        })
        .unwrap();

        assert_eq!(plan.allocations.len(), 2);
        assert_ne!(
            plan.allocations[0].planned_subnet,
            plan.allocations[1].planned_subnet
        );
    }

    #[test]
    fn subnet_planner_reports_pool_exhaustion_code() {
        let model: ComposeConfigModel = serde_json::from_value(serde_json::json!({
            "services": {"app": {"image": "alpine:3.20", "networks": {"grpc": null}}},
            "networks": {"grpc": {"ipam": {"config": [{"subnet": "10.99.0.0/30"}]}}}
        }))
        .unwrap();
        let scan = scan_compose_isolation(&model, "self-project");
        let daemon = ComposeIsolationDaemonSnapshot {
            networks: vec![
                network("occupied-a", Some("other-a"), "10.200.0.0/30"),
                network("occupied-b", Some("other-b"), "10.200.0.4/30"),
            ],
            resources: Vec::new(),
        };
        let error = plan_compose_isolation_subnets(&ComposeIsolationSubnetPlanInput {
            model: &model,
            project_name: "self-project",
            workspace_id: "workspace-a",
            scan: &scan,
            daemon: &daemon,
            state: &[],
            enabled: true,
            relocation: true,
            subnet_pool: Some("10.200.0.0/29"),
            subnet_prefix: Some(30),
            rebuild: false,
        })
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains(COMPOSE_CLONE_ISOLATION_POOL_EXHAUSTED)
        );
    }

    #[test]
    fn subnet_planner_prefers_self_project_then_persisted_assignment() {
        let model: ComposeConfigModel = serde_json::from_value(serde_json::json!({
            "services": {"app": {"image": "alpine:3.20", "networks": {"grpc": null}}},
            "networks": {"grpc": {"ipam": {"config": [{"subnet": "10.99.0.0/24"}]}}}
        }))
        .unwrap();
        let scan = scan_compose_isolation(&model, "self-project");
        let mut existing = network("self_grpc", Some("self-project"), "10.200.42.0/24");
        existing.compose_network = Some("grpc".to_owned());
        let state = [ComposeIsolationPersistedSubnet {
            network: "grpc".to_owned(),
            requested_subnet: "10.99.0.0/24".to_owned(),
            planned_subnet: "10.200.43.0/24".to_owned(),
        }];
        let daemon = ComposeIsolationDaemonSnapshot {
            networks: vec![existing],
            resources: Vec::new(),
        };
        let plan = plan_compose_isolation_subnets(&ComposeIsolationSubnetPlanInput {
            model: &model,
            project_name: "self-project",
            workspace_id: "workspace-a",
            scan: &scan,
            daemon: &daemon,
            state: &state,
            enabled: true,
            relocation: true,
            subnet_pool: Some("10.200.0.0/16"),
            subnet_prefix: Some(24),
            rebuild: false,
        })
        .unwrap();
        assert_eq!(plan.allocations[0].planned_subnet, "10.200.42.0/24");

        let state_plan = plan_compose_isolation_subnets(&ComposeIsolationSubnetPlanInput {
            model: &model,
            project_name: "self-project",
            workspace_id: "workspace-a",
            scan: &scan,
            daemon: &ComposeIsolationDaemonSnapshot::default(),
            state: &state,
            enabled: true,
            relocation: true,
            subnet_pool: Some("10.200.0.0/16"),
            subnet_prefix: Some(24),
            rebuild: false,
        })
        .unwrap();
        assert_eq!(state_plan.allocations[0].planned_subnet, "10.200.43.0/24");
    }

    #[test]
    fn subnet_planner_returns_to_requested_slot_only_on_rebuild() {
        let model: ComposeConfigModel = serde_json::from_value(serde_json::json!({
            "services": {"app": {"image": "alpine:3.20", "networks": {"grpc": null}}},
            "networks": {"grpc": {"ipam": {"config": [{"subnet": "10.200.1.0/24"}]}}}
        }))
        .unwrap();
        let scan = scan_compose_isolation(&model, "self-project");
        let state = [ComposeIsolationPersistedSubnet {
            network: "grpc".to_owned(),
            requested_subnet: "10.200.1.0/24".to_owned(),
            planned_subnet: "10.200.2.0/24".to_owned(),
        }];
        let plan = |rebuild| {
            plan_compose_isolation_subnets(&ComposeIsolationSubnetPlanInput {
                model: &model,
                project_name: "self-project",
                workspace_id: "workspace-a",
                scan: &scan,
                daemon: &ComposeIsolationDaemonSnapshot::default(),
                state: &state,
                enabled: true,
                relocation: true,
                subnet_pool: Some("10.200.0.0/16"),
                subnet_prefix: Some(24),
                rebuild,
            })
            .unwrap()
        };

        assert_eq!(plan(false).allocations[0].planned_subnet, "10.200.2.0/24");
        assert_eq!(plan(true).allocations[0].planned_subnet, "10.200.1.0/24");
    }

    #[test]
    fn subnet_planner_keeps_noop_allocation_for_override_generation() {
        let model: ComposeConfigModel = serde_json::from_value(serde_json::json!({
            "services": {"app": {"image": "alpine:3.20", "networks": {"grpc": null}}},
            "networks": {"grpc": {"ipam": {"config": [{"subnet": "10.200.1.0/24"}]}}}
        }))
        .unwrap();
        let scan = scan_compose_isolation(&model, "self-project");

        let plan = plan_compose_isolation_subnets(&ComposeIsolationSubnetPlanInput {
            model: &model,
            project_name: "self-project",
            workspace_id: "workspace-a",
            scan: &scan,
            daemon: &ComposeIsolationDaemonSnapshot::default(),
            state: &[],
            enabled: true,
            relocation: true,
            subnet_pool: Some("10.200.1.0/24"),
            subnet_prefix: Some(24),
            rebuild: false,
        })
        .unwrap();

        assert_eq!(plan.allocations.len(), 1);
        assert_eq!(plan.allocations[0].planned_subnet, "10.200.1.0/24");
        assert!(!plan.allocations[0].relocated);
    }

    #[test]
    fn subnet_planner_rejects_static_addresses_and_ipv6() {
        let static_model: ComposeConfigModel = serde_json::from_value(serde_json::json!({
            "services": {"app": {"image": "alpine:3.20", "networks": {
                "grpc": {"ipv4_address": "10.99.0.2"}
            }}},
            "networks": {"grpc": {"ipam": {"config": [{"subnet": "10.99.0.0/24"}]}}}
        }))
        .unwrap();
        let scan = scan_compose_isolation(&static_model, "self-project");
        let error = plan_compose_isolation_subnets(&ComposeIsolationSubnetPlanInput {
            model: &static_model,
            project_name: "self-project",
            workspace_id: "workspace-a",
            scan: &scan,
            daemon: &ComposeIsolationDaemonSnapshot::default(),
            state: &[],
            enabled: true,
            relocation: true,
            subnet_pool: Some("10.200.0.0/16"),
            subnet_prefix: Some(24),
            rebuild: false,
        })
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains(COMPOSE_CLONE_ISOLATION_UNSUPPORTED)
        );
        assert!(error.to_string().contains("ipv4_address"));

        let ipv6_model: ComposeConfigModel = serde_json::from_value(serde_json::json!({
            "services": {"app": {"image": "alpine:3.20", "networks": {"grpc": null}}},
            "networks": {"grpc": {"ipam": {"config": [{"subnet": "fd00::/64"}]}}}
        }))
        .unwrap();
        let scan = scan_compose_isolation(&ipv6_model, "self-project");
        let error = plan_compose_isolation_subnets(&ComposeIsolationSubnetPlanInput {
            model: &ipv6_model,
            project_name: "self-project",
            workspace_id: "workspace-a",
            scan: &scan,
            daemon: &ComposeIsolationDaemonSnapshot::default(),
            state: &[],
            enabled: true,
            relocation: true,
            subnet_pool: Some("10.200.0.0/16"),
            subnet_prefix: Some(24),
            rebuild: false,
        })
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains(COMPOSE_CLONE_ISOLATION_UNSUPPORTED)
        );
    }

    #[test]
    fn subnet_planner_rejects_multiple_fixed_subnets_on_one_network() {
        let model: ComposeConfigModel = serde_json::from_value(serde_json::json!({
            "services": {"app": {"image": "alpine:3.20", "networks": {"grpc": null}}},
            "networks": {"grpc": {"ipam": {"config": [
                {"subnet": "10.99.0.0/24"},
                {"subnet": "10.99.1.0/24"}
            ]}}}
        }))
        .unwrap();
        let scan = scan_compose_isolation(&model, "self-project");

        let error = plan_compose_isolation_subnets(&ComposeIsolationSubnetPlanInput {
            model: &model,
            project_name: "self-project",
            workspace_id: "workspace-a",
            scan: &scan,
            daemon: &ComposeIsolationDaemonSnapshot::default(),
            state: &[],
            enabled: true,
            relocation: true,
            subnet_pool: Some("10.200.0.0/16"),
            subnet_prefix: Some(24),
            rebuild: false,
        })
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains(COMPOSE_CLONE_ISOLATION_UNSUPPORTED)
        );
        assert!(error.to_string().contains("multiple fixed IPAM subnets"));
    }

    #[test]
    fn subnet_planner_reports_ipv6_for_dual_stack_network_in_either_order() {
        for configs in [
            serde_json::json!([
                {"subnet": "10.99.0.0/24"},
                {"subnet": "fd00::/64"}
            ]),
            serde_json::json!([
                {"subnet": "fd00::/64"},
                {"subnet": "10.99.0.0/24"}
            ]),
        ] {
            let model: ComposeConfigModel = serde_json::from_value(serde_json::json!({
                "services": {"app": {"image": "alpine:3.20", "networks": {"grpc": null}}},
                "networks": {"grpc": {"ipam": {"config": configs}}}
            }))
            .unwrap();
            let scan = scan_compose_isolation(&model, "self-project");

            let error = plan_compose_isolation_subnets(&ComposeIsolationSubnetPlanInput {
                model: &model,
                project_name: "self-project",
                workspace_id: "workspace-a",
                scan: &scan,
                daemon: &ComposeIsolationDaemonSnapshot::default(),
                state: &[],
                enabled: true,
                relocation: true,
                subnet_pool: Some("10.200.0.0/16"),
                subnet_prefix: Some(24),
                rebuild: false,
            })
            .unwrap_err();

            assert!(
                error
                    .to_string()
                    .contains(COMPOSE_CLONE_ISOLATION_UNSUPPORTED)
            );
            assert!(
                error
                    .to_string()
                    .contains("fixed IPv6 Compose network subnets")
            );
            assert!(!error.to_string().contains("multiple fixed IPAM subnets"));
        }
    }

    #[test]
    fn subnet_planner_removes_unused_stale_self_network_and_rejects_attached_one() {
        let model: ComposeConfigModel = serde_json::from_value(serde_json::json!({
            "services": {"app": {"image": "alpine:3.20", "networks": {"grpc": null}}},
            "networks": {"grpc": {"ipam": {"config": [{"subnet": "10.99.0.0/24"}]}}}
        }))
        .unwrap();
        let scan = scan_compose_isolation(&model, "self-project");
        let mut existing = network("self_grpc", Some("self-project"), "10.50.0.0/24");
        existing.compose_network = Some("grpc".to_owned());
        let daemon = ComposeIsolationDaemonSnapshot {
            networks: vec![existing.clone()],
            resources: Vec::new(),
        };
        let input = |daemon| ComposeIsolationSubnetPlanInput {
            model: &model,
            project_name: "self-project",
            workspace_id: "workspace-a",
            scan: &scan,
            daemon,
            state: &[],
            enabled: true,
            relocation: true,
            subnet_pool: Some("10.200.0.0/16"),
            subnet_prefix: Some(24),
            rebuild: false,
        };
        let plan = plan_compose_isolation_subnets(&input(&daemon)).unwrap();
        assert_eq!(plan.networks_to_remove, ["self_grpc"]);

        existing.has_attached_containers = true;
        let attached = ComposeIsolationDaemonSnapshot {
            networks: vec![existing],
            resources: Vec::new(),
        };
        let error = plan_compose_isolation_subnets(&input(&attached)).unwrap_err();
        assert!(error.to_string().contains(COMPOSE_CLONE_ISOLATION_INVALID));
        assert!(
            error
                .to_string()
                .contains(COMPOSE_CLONE_ISOLATION_RECREATE_HINT)
        );
    }

    #[test]
    fn subnet_planner_recreates_self_network_missing_planned_ipam_fields() {
        let model: ComposeConfigModel = serde_json::from_value(serde_json::json!({
            "services": {"app": {"image": "alpine:3.20", "networks": {"grpc": null}}},
            "networks": {"grpc": {"ipam": {"config": [{
                "subnet": "10.99.0.0/24",
                "ip_range": "10.99.0.128/25",
                "aux_addresses": {"reserved": "10.99.0.10"}
            }]}}}
        }))
        .unwrap();
        let scan = scan_compose_isolation(&model, "self-project");
        let mut existing = network("self_grpc", Some("self-project"), "10.200.42.0/24");
        existing.compose_network = Some("grpc".to_owned());
        let daemon = ComposeIsolationDaemonSnapshot {
            networks: vec![existing],
            resources: Vec::new(),
        };

        let plan = plan_compose_isolation_subnets(&ComposeIsolationSubnetPlanInput {
            model: &model,
            project_name: "self-project",
            workspace_id: "workspace-a",
            scan: &scan,
            daemon: &daemon,
            state: &[],
            enabled: true,
            relocation: true,
            subnet_pool: Some("10.200.0.0/16"),
            subnet_prefix: Some(24),
            rebuild: false,
        })
        .unwrap();

        assert_eq!(plan.allocations[0].planned_subnet, "10.200.42.0/24");
        assert_eq!(plan.networks_to_remove, ["self_grpc"]);
    }

    #[test]
    fn subnet_planner_requires_down_before_rebuild_returns_to_requested_subnet() {
        let model: ComposeConfigModel = serde_json::from_value(serde_json::json!({
            "services": {"app": {"image": "alpine:3.20", "networks": {"grpc": null}}},
            "networks": {"grpc": {"ipam": {"config": [{"subnet": "10.200.1.0/24"}]}}}
        }))
        .unwrap();
        let scan = scan_compose_isolation(&model, "self-project");
        let mut existing = network("self_grpc", Some("self-project"), "10.200.2.0/24");
        existing.compose_network = Some("grpc".to_owned());
        existing.has_attached_containers = true;
        let daemon = ComposeIsolationDaemonSnapshot {
            networks: vec![existing],
            resources: Vec::new(),
        };

        let error = plan_compose_isolation_subnets(&ComposeIsolationSubnetPlanInput {
            model: &model,
            project_name: "self-project",
            workspace_id: "workspace-a",
            scan: &scan,
            daemon: &daemon,
            state: &[],
            enabled: true,
            relocation: true,
            subnet_pool: Some("10.200.0.0/16"),
            subnet_prefix: Some(24),
            rebuild: true,
        })
        .unwrap_err();

        assert!(error.to_string().contains(COMPOSE_CLONE_ISOLATION_INVALID));
        assert!(error.to_string().contains("10.200.1.0/24"));
        assert!(
            error
                .to_string()
                .contains(COMPOSE_CLONE_ISOLATION_RECREATE_HINT)
        );
    }

    #[test]
    fn plans_workspace_scoped_name_rewrites_and_aliases() {
        let model: ComposeConfigModel = serde_json::from_value(serde_json::json!({
            "services": {
                "app": {
                    "image": "alpine:3.20",
                    "container_name": "fixed-app",
                    "networks": {"backend": null, "frontend": null}
                }
            },
            "networks": {
                "backend": {"name": "fixed-backend"},
                "frontend": {"name": "shared-frontend", "external": true}
            },
            "volumes": {
                "cache": {"name": "fixed-cache"},
                "shared": {"name": "shared-cache", "external": true}
            },
            "configs": {"app": {"name": "fixed-config"}},
            "secrets": {"app": {"name": "fixed-secret"}}
        }))
        .unwrap();
        let scan = scan_compose_isolation(&model, "self-project");

        let plan = plan_compose_isolation_name_rewrites(&ComposeIsolationNameRewritePlanInput {
            model: &model,
            scan: &scan,
            workspace_id: "abc123def456",
            enabled: true,
            rewrite_container_names: true,
            rewrite_resource_names: true,
        });

        assert_eq!(plan.services.len(), 1);
        assert_eq!(plan.services[0].service, "app");
        assert_eq!(plan.services[0].original_name, "fixed-app");
        assert_eq!(plan.services[0].rewritten_name, "fixed-app-abc123def456");
        assert_eq!(plan.services[0].networks, ["backend", "frontend"]);
        assert_eq!(plan.resources.len(), 4);
        assert!(plan.resources.iter().any(|rewrite| {
            rewrite.kind == ComposeIsolationResourceKind::Volume
                && rewrite.resource == "cache"
                && rewrite.rewritten_name == "fixed-cache-abc123def456"
        }));
        assert!(
            !plan
                .resources
                .iter()
                .any(|rewrite| rewrite.resource == "shared" || rewrite.resource == "frontend")
        );

        let effective = apply_compose_isolation_name_rewrites(&scan, &plan);
        assert!(effective.fixed_names.iter().any(|fixed| {
            fixed.kind == ComposeIsolationResourceKind::ServiceContainer
                && fixed.name == "fixed-app-abc123def456"
        }));
        assert!(effective.fixed_names.iter().any(|fixed| {
            fixed.kind == ComposeIsolationResourceKind::Volume
                && fixed.name == "fixed-cache-abc123def456"
        }));
    }

    #[test]
    fn name_rewrite_plan_is_empty_without_clone_isolation_opt_in() {
        let model: ComposeConfigModel = serde_json::from_value(serde_json::json!({
            "services": {
                "app": {"image": "alpine:3.20", "container_name": "fixed-app"}
            },
            "volumes": {"cache": {"name": "fixed-cache"}}
        }))
        .unwrap();
        let scan = scan_compose_isolation(&model, "self-project");

        let plan = plan_compose_isolation_name_rewrites(&ComposeIsolationNameRewritePlanInput {
            model: &model,
            scan: &scan,
            workspace_id: "abc123def456",
            enabled: false,
            rewrite_container_names: true,
            rewrite_resource_names: true,
        });

        assert!(plan.services.is_empty());
        assert!(plan.resources.is_empty());
        assert_eq!(apply_compose_isolation_name_rewrites(&scan, &plan), scan);
    }

    #[test]
    fn name_rewrite_policies_control_container_and_resource_names_independently() {
        let model: ComposeConfigModel = serde_json::from_value(serde_json::json!({
            "services": {
                "app": {"image": "alpine:3.20", "container_name": "fixed-app"}
            },
            "volumes": {"cache": {"name": "fixed-cache"}}
        }))
        .unwrap();
        let scan = scan_compose_isolation(&model, "self-project");
        let input = |rewrite_container_names, rewrite_resource_names| {
            plan_compose_isolation_name_rewrites(&ComposeIsolationNameRewritePlanInput {
                model: &model,
                scan: &scan,
                workspace_id: "abc123def456",
                enabled: true,
                rewrite_container_names,
                rewrite_resource_names,
            })
        };

        let resources_only = input(false, true);
        assert!(resources_only.services.is_empty());
        assert_eq!(resources_only.resources.len(), 1);
        assert_eq!(resources_only.resources[0].original_name, "fixed-cache");
        assert_eq!(
            resources_only.resources[0].rewritten_name,
            "fixed-cache-abc123def456"
        );

        let containers_only = input(true, false);
        assert_eq!(containers_only.services.len(), 1);
        assert!(containers_only.resources.is_empty());
    }

    #[test]
    fn detects_overlapping_subnet_and_excludes_self_project() {
        let scan = ComposeIsolationScan {
            networks: vec![ComposeIsolationNetworkRequest {
                network: "grpc".to_owned(),
                driver: None,
                ipam_driver: None,
                subnet: "172.28.0.0/16".to_owned(),
                gateway: Some("172.28.0.1".to_owned()),
                ip_range: None,
                aux_addresses: BTreeMap::new(),
                has_unrepresented_ipam_configs: false,
                unsupported_ipam_fields: BTreeSet::new(),
            }],
            fixed_names: Vec::new(),
        };
        let daemon = ComposeIsolationDaemonSnapshot {
            networks: vec![
                network("self_net", Some("self-project"), "172.28.0.0/16"),
                network("other_net", Some("other-project"), "172.28.10.0/24"),
                network("adjacent_net", None, "172.29.0.0/16"),
            ],
            resources: Vec::new(),
        };

        let findings = plan_compose_isolation(&ComposeIsolationPlanInput {
            project_name: "self-project",
            scan: &scan,
            daemon: &daemon,
        });

        assert_eq!(findings.len(), 1);
        assert!(matches!(
            &findings[0],
            ComposeIsolationFinding::NetworkSubnetOverlap {
                compose_network,
                docker_network,
                docker_project,
                ..
            } if compose_network == "grpc"
                && docker_network == "other_net"
                && docker_project.as_deref() == Some("other-project")
        ));
    }

    #[test]
    fn skips_ipv6_subnet_overlap_detection() {
        let scan = ComposeIsolationScan {
            networks: vec![ComposeIsolationNetworkRequest {
                network: "v6".to_owned(),
                driver: None,
                ipam_driver: None,
                subnet: "fd00::/64".to_owned(),
                gateway: None,
                ip_range: None,
                aux_addresses: BTreeMap::new(),
                has_unrepresented_ipam_configs: false,
                unsupported_ipam_fields: BTreeSet::new(),
            }],
            fixed_names: Vec::new(),
        };
        let daemon = ComposeIsolationDaemonSnapshot {
            networks: vec![network("other", None, "fd00::/64")],
            resources: Vec::new(),
        };

        let findings = plan_compose_isolation(&ComposeIsolationPlanInput {
            project_name: "self-project",
            scan: &scan,
            daemon: &daemon,
        });

        assert!(findings.is_empty());
    }

    #[test]
    fn detects_fixed_name_conflict_and_excludes_self_project() {
        let scan = ComposeIsolationScan {
            networks: Vec::new(),
            fixed_names: vec![
                ComposeIsolationFixedNameRequest {
                    kind: ComposeIsolationResourceKind::ServiceContainer,
                    resource: "app".to_owned(),
                    name: "fixed-app".to_owned(),
                },
                ComposeIsolationFixedNameRequest {
                    kind: ComposeIsolationResourceKind::Volume,
                    resource: "cache".to_owned(),
                    name: "fixed-cache".to_owned(),
                },
            ],
        };
        let daemon = ComposeIsolationDaemonSnapshot {
            networks: Vec::new(),
            resources: vec![
                resource(
                    ComposeIsolationResourceKind::ServiceContainer,
                    "fixed-app",
                    Some("other-project"),
                ),
                resource(
                    ComposeIsolationResourceKind::Volume,
                    "fixed-cache",
                    Some("self-project"),
                ),
            ],
        };

        let findings = plan_compose_isolation(&ComposeIsolationPlanInput {
            project_name: "self-project",
            scan: &scan,
            daemon: &daemon,
        });

        assert_eq!(findings.len(), 1);
        assert!(matches!(
            &findings[0],
            ComposeIsolationFinding::FixedNameConflict {
                kind: ComposeIsolationResourceKind::ServiceContainer,
                compose_resource,
                requested_name,
                docker_project,
                ..
            } if compose_resource == "app"
                && requested_name == "fixed-app"
                && docker_project.as_deref() == Some("other-project")
        ));
    }

    #[test]
    fn compares_only_compatible_known_ipam_address_spaces() {
        let mut requested = ComposeIsolationNetworkRequest {
            network: "app".to_owned(),
            driver: None,
            ipam_driver: None,
            subnet: "172.28.0.0/16".to_owned(),
            gateway: None,
            ip_range: None,
            aux_addresses: BTreeMap::new(),
            has_unrepresented_ipam_configs: false,
            unsupported_ipam_fields: BTreeSet::new(),
        };
        let mut existing = network("existing", None, "172.28.0.0/16");

        assert!(uses_same_ipam_address_space(&requested, &existing));

        existing.scope = Some("swarm".to_owned());
        assert!(!uses_same_ipam_address_space(&requested, &existing));

        requested.driver = Some("overlay".to_owned());
        assert!(uses_same_ipam_address_space(&requested, &existing));

        existing.ipam_driver = Some("custom".to_owned());
        assert!(!uses_same_ipam_address_space(&requested, &existing));
    }

    #[test]
    fn conservatively_compares_unknown_network_scope_metadata() {
        let requested = ComposeIsolationNetworkRequest {
            network: "app".to_owned(),
            driver: Some("custom-network-driver".to_owned()),
            ipam_driver: Some("custom-ipam".to_owned()),
            subnet: "172.28.0.0/16".to_owned(),
            gateway: None,
            ip_range: None,
            aux_addresses: BTreeMap::new(),
            has_unrepresented_ipam_configs: false,
            unsupported_ipam_fields: BTreeSet::new(),
        };
        let mut existing = network("existing", None, "172.28.0.0/16");
        existing.scope = None;
        existing.ipam_driver = Some("custom-ipam".to_owned());

        assert!(uses_same_ipam_address_space(&requested, &existing));
    }

    fn network(
        name: &str,
        compose_project: Option<&str>,
        subnet: &str,
    ) -> ComposeIsolationDockerNetwork {
        ComposeIsolationDockerNetwork {
            name: name.to_owned(),
            compose_project: compose_project.map(str::to_owned),
            compose_network: None,
            scope: Some("local".to_owned()),
            ipam_driver: Some("default".to_owned()),
            ipam_configs: vec![ComposeIsolationDockerIpamConfig {
                subnet: Some(subnet.to_owned()),
                gateway: None,
                ip_range: None,
                auxiliary_addresses: BTreeMap::new(),
            }],
            has_attached_containers: false,
        }
    }

    fn resource(
        kind: ComposeIsolationResourceKind,
        name: &str,
        compose_project: Option<&str>,
    ) -> ComposeIsolationDockerResource {
        ComposeIsolationDockerResource {
            kind,
            name: name.to_owned(),
            compose_project: compose_project.map(str::to_owned),
        }
    }
}
