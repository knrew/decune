use crate::runtime::compose_isolation::{
    ComposeIsolationClassification, ComposeIsolationDaemonSnapshot, ComposeIsolationFinding,
    ComposeIsolationScan, Ipv4Cidr,
};

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
            for existing_config in &network.ipam_configs {
                let Some(existing_subnet_text) = existing_config.subnet.as_deref() else {
                    continue;
                };
                let Some(existing_subnet) = Ipv4Cidr::parse(existing_subnet_text) else {
                    continue;
                };
                if requested_subnet.overlaps(existing_subnet) {
                    findings.push(ComposeIsolationFinding::NetworkSubnetOverlap {
                        classification: ComposeIsolationClassification::DaemonConflict,
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
                classification: ComposeIsolationClassification::DaemonConflict,
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

fn is_self_project(compose_project: Option<&str>, project_name: &str) -> bool {
    compose_project == Some(project_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::compose_isolation::{
        ComposeIsolationDockerIpamConfig, ComposeIsolationDockerNetwork,
        ComposeIsolationDockerResource, ComposeIsolationFixedNameRequest,
        ComposeIsolationNetworkRequest, ComposeIsolationResourceKind,
    };

    #[test]
    fn detects_overlapping_subnet_and_excludes_self_project() {
        let scan = ComposeIsolationScan {
            networks: vec![ComposeIsolationNetworkRequest {
                network: "grpc".to_owned(),
                subnet: "172.28.0.0/16".to_owned(),
                gateway: Some("172.28.0.1".to_owned()),
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
                subnet: "fd00::/64".to_owned(),
                gateway: None,
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

    fn network(
        name: &str,
        compose_project: Option<&str>,
        subnet: &str,
    ) -> ComposeIsolationDockerNetwork {
        ComposeIsolationDockerNetwork {
            name: name.to_owned(),
            compose_project: compose_project.map(str::to_owned),
            ipam_configs: vec![ComposeIsolationDockerIpamConfig {
                subnet: Some(subnet.to_owned()),
                gateway: None,
            }],
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
