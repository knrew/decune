use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, anyhow, bail};

use crate::runtime::{
    compose_cli::ComposeConfigModel,
    compose_isolation::{
        COMPOSE_CLONE_ISOLATION_INVALID, ComposeIsolationEndpointDeclaration,
        ComposeIsolationEndpointPlan, ComposeIsolationFinding, ComposeIsolationScan,
        ComposeIsolationSubnetAllocation, ComposeIsolationSubnetPlan, Ipv4Cidr,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EndpointValueKind {
    Gateway,
    Subnet,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EndpointPlaceholder {
    text: String,
    network: String,
    kind: EndpointValueKind,
}

/// Plans endpoint environment overrides and may extend `subnet_plan` as part of that plan.
///
/// When a `.gateway` placeholder references an allocation whose source IPAM omitted a gateway,
/// this derives the first host address from the planned subnet and records it as the allocation's
/// explicit planned gateway.
pub(crate) fn plan_compose_isolation_endpoints(
    model: &ComposeConfigModel,
    scan: &ComposeIsolationScan,
    declarations: &[ComposeIsolationEndpointDeclaration],
    network_relocation_enabled: bool,
    subnet_plan: &mut ComposeIsolationSubnetPlan,
) -> Result<(ComposeIsolationEndpointPlan, Vec<ComposeIsolationFinding>)> {
    let mut endpoint_plan = ComposeIsolationEndpointPlan::default();

    for declaration in declarations {
        if !model.has_service(&declaration.service) {
            bail!(
                "{COMPOSE_CLONE_ISOLATION_INVALID}: endpoint declaration references missing Compose service `{}`; environment variable `{}`",
                declaration.service,
                declaration.env
            );
        }
        let placeholders = endpoint_placeholders(&declaration.value)?;
        let mut rendered = declaration.value.clone();
        for placeholder in placeholders {
            if !model.has_network(&placeholder.network) {
                bail!(
                    "{COMPOSE_CLONE_ISOLATION_INVALID}: endpoint declaration references missing Compose network `{}`; service `{}`; environment variable `{}`",
                    placeholder.network,
                    declaration.service,
                    declaration.env
                );
            }
            let allocation = endpoint_allocation(subnet_plan, &placeholder.network).ok_or_else(|| {
                if network_relocation_enabled {
                    anyhow!(
                        "{COMPOSE_CLONE_ISOLATION_INVALID}: endpoint declaration references Compose network `{}` that is not a network relocation target; service `{}`; environment variable `{}`",
                        placeholder.network,
                        declaration.service,
                        declaration.env
                    )
                } else {
                    anyhow!(
                        "{COMPOSE_CLONE_ISOLATION_INVALID}: endpoint declaration cannot render Compose network `{}` because network relocation is disabled; set compose.clone_isolation.networks.relocation = true; service `{}`; environment variable `{}`",
                        placeholder.network,
                        declaration.service,
                        declaration.env
                    )
                }
            })?;
            let replacement = match placeholder.kind {
                EndpointValueKind::Subnet => allocation.planned_subnet.clone(),
                EndpointValueKind::Gateway => planned_gateway(allocation)?,
            };
            rendered = rendered.replace(&placeholder.text, &replacement);
        }
        endpoint_plan
            .services
            .entry(declaration.service.clone())
            .or_default()
            .insert(declaration.env.clone(), rendered);
    }

    let findings = stale_endpoint_findings(model, scan, subnet_plan, &endpoint_plan);
    Ok((endpoint_plan, findings))
}

fn endpoint_allocation<'a>(
    subnet_plan: &'a mut ComposeIsolationSubnetPlan,
    network: &str,
) -> Option<&'a mut ComposeIsolationSubnetAllocation> {
    subnet_plan
        .allocations
        .iter_mut()
        .find(|allocation| allocation.network == network)
}

fn planned_gateway(allocation: &mut ComposeIsolationSubnetAllocation) -> Result<String> {
    if let Some(gateway) = &allocation.planned_gateway {
        return Ok(gateway.clone());
    }
    let subnet = Ipv4Cidr::parse(&allocation.planned_subnet).ok_or_else(|| {
        anyhow!(
            "{COMPOSE_CLONE_ISOLATION_INVALID}: planned subnet for Compose network `{}` is not a valid IPv4 CIDR: {}",
            allocation.network,
            allocation.planned_subnet
        )
    })?;
    let gateway = subnet.address_at_offset(1).ok_or_else(|| {
        anyhow!(
            "{COMPOSE_CLONE_ISOLATION_INVALID}: planned subnet {} for Compose network `{}` has no first host address for endpoint gateway derivation",
            allocation.planned_subnet,
            allocation.network
        )
    })?;
    let gateway = gateway.to_string();
    allocation.planned_gateway = Some(gateway.clone());
    Ok(gateway)
}

fn endpoint_placeholders(value: &str) -> Result<Vec<EndpointPlaceholder>> {
    const START: &str = "${decune";
    let mut remaining = value;
    let mut placeholders = Vec::new();
    while let Some((_, after_start)) = remaining.split_once(START) {
        let Some((body_suffix, rest)) = after_start.split_once('}') else {
            bail!(
                "{COMPOSE_CLONE_ISOLATION_INVALID}: endpoint value contains an unterminated decune placeholder"
            );
        };
        let body = format!("decune{body_suffix}");
        let text = format!("${{{body}}}");
        let Some(network_and_kind) = body.strip_prefix("decune.network.") else {
            bail!(
                "{COMPOSE_CLONE_ISOLATION_INVALID}: endpoint value contains unknown placeholder `{text}`"
            );
        };
        let (network, kind) = if let Some(network) = network_and_kind.strip_suffix(".gateway") {
            (network, EndpointValueKind::Gateway)
        } else if let Some(network) = network_and_kind.strip_suffix(".subnet") {
            (network, EndpointValueKind::Subnet)
        } else {
            bail!(
                "{COMPOSE_CLONE_ISOLATION_INVALID}: endpoint value contains unknown placeholder `{text}`"
            );
        };
        if network.is_empty() {
            bail!(
                "{COMPOSE_CLONE_ISOLATION_INVALID}: endpoint value contains unknown placeholder `{text}`"
            );
        }
        placeholders.push(EndpointPlaceholder {
            text,
            network: network.to_owned(),
            kind,
        });
        remaining = rest;
    }
    Ok(placeholders)
}

fn stale_endpoint_findings(
    model: &ComposeConfigModel,
    scan: &ComposeIsolationScan,
    subnet_plan: &ComposeIsolationSubnetPlan,
    endpoint_plan: &ComposeIsolationEndpointPlan,
) -> Vec<ComposeIsolationFinding> {
    let mut findings = BTreeSet::new();
    for allocation in subnet_plan
        .allocations
        .iter()
        .filter(|allocation| allocation.relocated)
    {
        let Some(requested) = scan.networks.iter().find(|requested| {
            if requested.network != allocation.network {
                return false;
            }
            requested.subnet == allocation.requested_subnet
        }) else {
            continue;
        };
        let mut addresses = BTreeSet::new();
        if let Some(cidr) = Ipv4Cidr::parse(&requested.subnet) {
            addresses.insert(std::net::Ipv4Addr::from(cidr.network()).to_string());
        }
        if let Some(gateway) = &requested.gateway {
            addresses.insert(gateway.clone());
        }
        for (service_name, service) in model.services() {
            if !service_uses_network(model, service_name, &allocation.network) {
                continue;
            }
            let mut environment = service
                .environment_values()
                .map(|(env, value)| (env.as_str(), value))
                .collect::<BTreeMap<_, _>>();
            if let Some(overrides) = endpoint_plan.services.get(service_name) {
                environment.extend(
                    overrides
                        .iter()
                        .map(|(env, value)| (env.as_str(), value.as_str())),
                );
            }
            for (env, value) in environment {
                for address in &addresses {
                    if contains_address_token(value, address) {
                        findings.insert((
                            service_name.clone(),
                            env.to_owned(),
                            allocation.network.clone(),
                            address.clone(),
                        ));
                    }
                }
            }
        }
    }
    findings
        .into_iter()
        .map(
            |(service, env, network, address)| ComposeIsolationFinding::EndpointUnsafe {
                service,
                env,
                network,
                address,
            },
        )
        .collect()
}

fn service_uses_network(model: &ComposeConfigModel, service: &str, network: &str) -> bool {
    service_uses_network_inner(model, service, network, &mut BTreeSet::new())
}

fn service_uses_network_inner(
    model: &ComposeConfigModel,
    service_name: &str,
    network: &str,
    visited: &mut BTreeSet<String>,
) -> bool {
    if !visited.insert(service_name.to_owned()) {
        return false;
    }
    let Some(service) = model.service(service_name) else {
        return false;
    };
    if service.network_names().any(|name| name == network) {
        return true;
    }
    let Some(shared_service) = service
        .network_mode
        .as_deref()
        .and_then(|mode| mode.strip_prefix("service:"))
    else {
        return false;
    };
    service_uses_network_inner(model, shared_service, network, visited)
}

fn contains_address_token(value: &str, address: &str) -> bool {
    value.match_indices(address).any(|(start, _)| {
        let Some(end) = start.checked_add(address.len()) else {
            return false;
        };
        let (Some(before_text), Some(after_text)) = (value.get(..start), value.get(end..)) else {
            return false;
        };
        let before = before_text.chars().next_back();
        let after = after_text.chars().next();
        before.is_none_or(|ch| !ch.is_ascii_digit() && ch != '.')
            && after.is_none_or(|ch| !ch.is_ascii_digit() && ch != '.')
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::compose_isolation::ComposeIsolationNetworkRequest;

    fn model(environment: &serde_json::Value) -> ComposeConfigModel {
        serde_json::from_value(serde_json::json!({
            "services": {
                "app": {
                    "image": "alpine:3.20",
                    "environment": environment,
                    "networks": ["fixed_net"]
                }
            },
            "networks": {
                "fixed_net": {
                    "ipam": {
                        "config": [{"subnet": "10.99.0.0/24", "gateway": "10.99.0.1"}]
                    }
                }
            }
        }))
        .unwrap()
    }

    fn scan(gateway: Option<&str>) -> ComposeIsolationScan {
        ComposeIsolationScan {
            networks: vec![ComposeIsolationNetworkRequest {
                network: "fixed_net".to_owned(),
                driver: None,
                ipam_driver: None,
                subnet: "10.99.0.0/24".to_owned(),
                gateway: gateway.map(str::to_owned),
                ip_range: None,
                aux_addresses: BTreeMap::new(),
                has_unrepresented_ipam_configs: false,
                unsupported_ipam_fields: BTreeSet::new(),
            }],
            fixed_names: Vec::new(),
        }
    }

    fn subnet_plan(relocated: bool, gateway: Option<&str>) -> ComposeIsolationSubnetPlan {
        ComposeIsolationSubnetPlan {
            allocations: vec![ComposeIsolationSubnetAllocation {
                network: "fixed_net".to_owned(),
                requested_subnet: "10.99.0.0/24".to_owned(),
                planned_subnet: "10.200.42.0/24".to_owned(),
                planned_gateway: gateway.map(str::to_owned),
                planned_ip_range: None,
                planned_aux_addresses: BTreeMap::new(),
                relocated,
            }],
            networks_to_remove: Vec::new(),
        }
    }

    fn declaration(value: &str) -> ComposeIsolationEndpointDeclaration {
        ComposeIsolationEndpointDeclaration {
            service: "app".to_owned(),
            env: "HOST_AGENT_ENDPOINT".to_owned(),
            value: value.to_owned(),
        }
    }

    #[test]
    fn renders_declared_gateway_and_subnet() {
        let model = model(&serde_json::json!({}));
        let mut subnet_plan = subnet_plan(true, Some("10.200.42.7"));

        let (endpoint_plan, findings) = plan_compose_isolation_endpoints(
            &model,
            &scan(Some("10.99.0.7")),
            &[declaration(
                "grpc://${decune.network.fixed_net.gateway}:50051/${decune.network.fixed_net.subnet}",
            )],
            true,
            &mut subnet_plan,
        )
        .unwrap();

        assert_eq!(
            endpoint_plan.services["app"]["HOST_AGENT_ENDPOINT"],
            "grpc://10.200.42.7:50051/10.200.42.0/24"
        );
        assert!(findings.is_empty());
    }

    #[test]
    fn gateway_placeholder_adds_first_host_gateway_when_source_omits_it() {
        let model = model(&serde_json::json!({}));
        let mut subnet_plan = subnet_plan(true, None);

        let (endpoint_plan, _) = plan_compose_isolation_endpoints(
            &model,
            &scan(None),
            &[declaration("${decune.network.fixed_net.gateway}")],
            true,
            &mut subnet_plan,
        )
        .unwrap();

        assert_eq!(
            endpoint_plan.services["app"]["HOST_AGENT_ENDPOINT"],
            "10.200.42.1"
        );
        assert_eq!(
            subnet_plan.allocations[0].planned_gateway.as_deref(),
            Some("10.200.42.1")
        );
    }

    #[test]
    fn rejects_unknown_placeholder_and_missing_or_unrelocated_targets() {
        let model = model(&serde_json::json!({}));
        for (value, expected) in [
            ("${decune.network.fixed_net.address}", "unknown placeholder"),
            (
                "${decune.network.missing.gateway}",
                "missing Compose network",
            ),
        ] {
            let mut subnet_plan = subnet_plan(true, None);
            let error = plan_compose_isolation_endpoints(
                &model,
                &scan(None),
                &[declaration(value)],
                true,
                &mut subnet_plan,
            )
            .err()
            .unwrap();
            assert!(error.to_string().contains(expected), "{error}");
        }

        let mut no_allocations = ComposeIsolationSubnetPlan::default();
        let error = plan_compose_isolation_endpoints(
            &model,
            &scan(None),
            &[declaration("${decune.network.fixed_net.gateway}")],
            true,
            &mut no_allocations,
        )
        .err()
        .unwrap();
        assert!(
            error
                .to_string()
                .contains("not a network relocation target")
        );

        let error = plan_compose_isolation_endpoints(
            &model,
            &scan(None),
            &[declaration("${decune.network.fixed_net.gateway}")],
            false,
            &mut ComposeIsolationSubnetPlan::default(),
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("network relocation is disabled"));
        assert!(
            error
                .to_string()
                .contains("compose.clone_isolation.networks.relocation = true")
        );

        let mut declaration = declaration("${decune.network.fixed_net.gateway}");
        declaration.service = "missing".to_owned();
        let error = plan_compose_isolation_endpoints(
            &model,
            &scan(None),
            &[declaration],
            true,
            &mut subnet_plan(true, None),
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("missing Compose service"));
    }

    #[test]
    fn stale_detection_uses_ip_token_boundaries() {
        let model = model(&serde_json::json!({
            "STALE": "grpc://10.99.0.1:50051",
            "LONGER_HOST": "grpc://10.99.0.100:50051",
            "LONGER_PREFIX": "grpc://110.99.0.1:50051"
        }));
        let mut subnet_plan = subnet_plan(true, Some("10.200.42.1"));

        let (_, findings) = plan_compose_isolation_endpoints(
            &model,
            &scan(Some("10.99.0.1")),
            &[],
            true,
            &mut subnet_plan,
        )
        .unwrap();

        assert_eq!(findings.len(), 1);
        assert!(matches!(
            &findings[0],
            ComposeIsolationFinding::EndpointUnsafe { service, env, network, address }
                if service == "app"
                    && env == "STALE"
                    && network == "fixed_net"
                    && address == "10.99.0.1"
        ));
    }

    #[test]
    fn stale_detection_checks_original_subnet_base_address() {
        let model = model(&serde_json::json!({
            "ROUTE": "10.99.0.0/24"
        }));
        let mut subnet_plan = subnet_plan(true, Some("10.200.42.1"));

        let (_, findings) =
            plan_compose_isolation_endpoints(&model, &scan(None), &[], true, &mut subnet_plan)
                .unwrap();

        assert!(matches!(
            findings.as_slice(),
            [ComposeIsolationFinding::EndpointUnsafe { env, address, .. }]
                if env == "ROUTE" && address == "10.99.0.0"
        ));
    }

    #[test]
    fn stale_detection_uses_rendered_environment_and_skips_unrelocated_network() {
        let model = model(&serde_json::json!({
            "HOST_AGENT_ENDPOINT": "grpc://10.99.0.1:50051"
        }));
        let endpoint = declaration("grpc://${decune.network.fixed_net.gateway}:50051");

        let mut relocated = subnet_plan(true, Some("10.200.42.1"));
        let (_, covered_findings) = plan_compose_isolation_endpoints(
            &model,
            &scan(Some("10.99.0.1")),
            std::slice::from_ref(&endpoint),
            true,
            &mut relocated,
        )
        .unwrap();
        assert!(covered_findings.is_empty());

        let mut unchanged = subnet_plan(false, Some("10.99.0.1"));
        let (_, unchanged_findings) = plan_compose_isolation_endpoints(
            &model,
            &scan(Some("10.99.0.1")),
            &[],
            true,
            &mut unchanged,
        )
        .unwrap();
        assert!(unchanged_findings.is_empty());
    }

    #[test]
    fn stale_detection_checks_all_connected_networks_after_endpoint_rendering() {
        let model: ComposeConfigModel = serde_json::from_value(serde_json::json!({
            "services": {
                "app": {
                    "image": "alpine:3.20",
                    "networks": ["fixed_net", "other_net"],
                    "environment": {}
                }
            },
            "networks": {
                "fixed_net": {},
                "other_net": {}
            }
        }))
        .unwrap();
        let scan = ComposeIsolationScan {
            networks: vec![
                ComposeIsolationNetworkRequest {
                    network: "fixed_net".to_owned(),
                    driver: None,
                    ipam_driver: None,
                    subnet: "10.99.0.0/24".to_owned(),
                    gateway: Some("10.99.0.1".to_owned()),
                    ip_range: None,
                    aux_addresses: BTreeMap::new(),
                    has_unrepresented_ipam_configs: false,
                    unsupported_ipam_fields: BTreeSet::new(),
                },
                ComposeIsolationNetworkRequest {
                    network: "other_net".to_owned(),
                    driver: None,
                    ipam_driver: None,
                    subnet: "10.100.0.0/24".to_owned(),
                    gateway: Some("10.100.0.1".to_owned()),
                    ip_range: None,
                    aux_addresses: BTreeMap::new(),
                    has_unrepresented_ipam_configs: false,
                    unsupported_ipam_fields: BTreeSet::new(),
                },
            ],
            fixed_names: Vec::new(),
        };
        let mut subnet_plan = ComposeIsolationSubnetPlan {
            allocations: vec![
                ComposeIsolationSubnetAllocation {
                    network: "fixed_net".to_owned(),
                    requested_subnet: "10.99.0.0/24".to_owned(),
                    planned_subnet: "10.200.42.0/24".to_owned(),
                    planned_gateway: Some("10.200.42.1".to_owned()),
                    planned_ip_range: None,
                    planned_aux_addresses: BTreeMap::new(),
                    relocated: true,
                },
                ComposeIsolationSubnetAllocation {
                    network: "other_net".to_owned(),
                    requested_subnet: "10.100.0.0/24".to_owned(),
                    planned_subnet: "10.200.43.0/24".to_owned(),
                    planned_gateway: Some("10.200.43.1".to_owned()),
                    planned_ip_range: None,
                    planned_aux_addresses: BTreeMap::new(),
                    relocated: true,
                },
            ],
            networks_to_remove: Vec::new(),
        };
        let endpoint =
            declaration("grpc://${decune.network.fixed_net.gateway}:50051/failover/10.100.0.1");

        let (endpoint_plan, findings) =
            plan_compose_isolation_endpoints(&model, &scan, &[endpoint], true, &mut subnet_plan)
                .unwrap();

        assert_eq!(
            endpoint_plan.services["app"]["HOST_AGENT_ENDPOINT"],
            "grpc://10.200.42.1:50051/failover/10.100.0.1"
        );
        assert!(matches!(
            findings.as_slice(),
            [ComposeIsolationFinding::EndpointUnsafe { network, address, .. }]
                if network == "other_net" && address == "10.100.0.1"
        ));
    }

    #[test]
    fn stale_detection_uses_endpoint_override_instead_of_original_environment() {
        let model = model(&serde_json::json!({
            "HOST_AGENT_ENDPOINT": "grpc://10.99.0.1:50051"
        }));
        let mut subnet_plan = subnet_plan(true, Some("10.200.42.1"));

        let (_, findings) = plan_compose_isolation_endpoints(
            &model,
            &scan(Some("10.99.0.1")),
            &[declaration("grpc://replacement.invalid:50051")],
            true,
            &mut subnet_plan,
        )
        .unwrap();

        assert!(findings.is_empty());
    }

    #[test]
    fn stale_detection_only_scans_services_using_the_relocated_network() {
        let model: ComposeConfigModel = serde_json::from_value(serde_json::json!({
            "services": {
                "connected": {
                    "image": "alpine:3.20",
                    "networks": ["fixed_net"],
                    "environment": {"CONNECTED": "grpc://10.99.0.1:50051"}
                },
                "shared": {
                    "image": "alpine:3.20",
                    "network_mode": "service:connected",
                    "environment": {"SHARED": "grpc://10.99.0.1:50051"}
                },
                "unrelated": {
                    "image": "alpine:3.20",
                    "networks": ["other_net"],
                    "environment": {"UNRELATED": "grpc://10.99.0.1:50051"}
                }
            },
            "networks": {
                "fixed_net": {},
                "other_net": {}
            }
        }))
        .unwrap();
        let mut subnet_plan = subnet_plan(true, Some("10.200.42.1"));

        let (_, findings) = plan_compose_isolation_endpoints(
            &model,
            &scan(Some("10.99.0.1")),
            &[],
            true,
            &mut subnet_plan,
        )
        .unwrap();

        let services = findings
            .iter()
            .filter_map(|finding| match finding {
                ComposeIsolationFinding::EndpointUnsafe { service, .. } => Some(service.as_str()),
                ComposeIsolationFinding::NetworkSubnetOverlap { .. }
                | ComposeIsolationFinding::FixedNameConflict { .. } => None,
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(services, BTreeSet::from(["connected", "shared"]));
    }
}
