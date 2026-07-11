use crate::runtime::compose_isolation::{ComposeIsolationFinding, ComposeIsolationResourceKind};

pub(crate) const COMPOSE_NETWORK_SUBNET_OVERLAP: &str = "compose_network_subnet_overlap";
pub(crate) const COMPOSE_FIXED_NAME_CONFLICT: &str = "compose_fixed_name_conflict";
pub(crate) const COMPOSE_CLONE_ISOLATION_INVALID: &str = "compose_clone_isolation_invalid";
pub(crate) const COMPOSE_CLONE_ISOLATION_UNSUPPORTED: &str = "compose_clone_isolation_unsupported";
pub(crate) const COMPOSE_CLONE_ISOLATION_POOL_EXHAUSTED: &str =
    "compose_clone_isolation_pool_exhausted";
pub(crate) const COMPOSE_CLONE_ISOLATION_ENDPOINT_UNSAFE: &str =
    "compose_clone_isolation_endpoint_unsafe";

#[derive(Debug)]
pub(crate) enum ComposeIsolationDiagnostic {
    NetworkSubnetOverlap {
        compose_network: String,
        requested_subnet: String,
        requested_gateway: Option<String>,
        docker_network: String,
        docker_project: Option<String>,
        docker_subnet: String,
        docker_gateway: Option<String>,
    },
    FixedNameConflict {
        kind: ComposeIsolationResourceKind,
        compose_resource: String,
        requested_name: String,
        docker_resource_name: String,
        docker_project: Option<String>,
    },
    EndpointUnsafe {
        service: String,
        env: String,
        network: String,
        address: String,
    },
}

impl ComposeIsolationDiagnostic {
    pub(crate) fn from_finding(finding: &ComposeIsolationFinding) -> Self {
        match finding {
            ComposeIsolationFinding::NetworkSubnetOverlap {
                compose_network,
                requested_subnet,
                requested_gateway,
                docker_network,
                docker_project,
                docker_subnet,
                docker_gateway,
                ..
            } => Self::NetworkSubnetOverlap {
                compose_network: compose_network.clone(),
                requested_subnet: requested_subnet.clone(),
                requested_gateway: requested_gateway.clone(),
                docker_network: docker_network.clone(),
                docker_project: docker_project.clone(),
                docker_subnet: docker_subnet.clone(),
                docker_gateway: docker_gateway.clone(),
            },
            ComposeIsolationFinding::FixedNameConflict {
                kind,
                compose_resource,
                requested_name,
                docker_resource_name,
                docker_project,
                ..
            } => Self::FixedNameConflict {
                kind: *kind,
                compose_resource: compose_resource.clone(),
                requested_name: requested_name.clone(),
                docker_resource_name: docker_resource_name.clone(),
                docker_project: docker_project.clone(),
            },
            ComposeIsolationFinding::EndpointUnsafe {
                service,
                env,
                network,
                address,
            } => Self::EndpointUnsafe {
                service: service.clone(),
                env: env.clone(),
                network: network.clone(),
                address: address.clone(),
            },
        }
    }
}

impl std::fmt::Display for ComposeIsolationDiagnostic {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NetworkSubnetOverlap {
                compose_network,
                requested_subnet,
                requested_gateway,
                docker_network,
                docker_project,
                docker_subnet,
                docker_gateway,
            } => write!(
                formatter,
                "{COMPOSE_NETWORK_SUBNET_OVERLAP}: Docker Compose network subnet overlaps an existing Docker network. network: `{compose_network}`; requested subnet: {requested_subnet}; requested gateway: {}; existing network: `{docker_network}`; existing subnet: {docker_subnet}; existing gateway: {}; existing compose project: {}. decune does not rewrite Compose network IPAM in this mode. Suggested actions: stop or remove the conflicting Docker network/project, or change the Compose network subnet when that is compatible with external contracts.",
                optional_value(requested_gateway.as_deref()),
                optional_value(docker_gateway.as_deref()),
                optional_value(docker_project.as_deref()),
            ),
            Self::FixedNameConflict {
                kind,
                compose_resource,
                requested_name,
                docker_resource_name,
                docker_project,
            } => write!(
                formatter,
                "{COMPOSE_FIXED_NAME_CONFLICT}: Docker Compose fixed resource name conflicts with an existing Docker resource. resource: {} `{compose_resource}`; requested name: `{requested_name}`; existing resource: {} `{docker_resource_name}`; existing compose project: {}. decune does not rewrite fixed Compose resource names in this mode. Suggested actions: stop or remove the conflicting Docker resource/project, or change the fixed name in the Compose file.",
                kind.compose_label(),
                kind.docker_label(),
                optional_value(docker_project.as_deref()),
            ),
            Self::EndpointUnsafe {
                service,
                env,
                network,
                address,
            } => write!(
                formatter,
                "{COMPOSE_CLONE_ISOLATION_ENDPOINT_UNSAFE}: Docker Compose service environment contains an address from a relocated network without a matching endpoint declaration. service: `{service}`; environment variable: `{env}`; network: `{network}`; original address: {address}. Declare [[compose.clone_isolation.endpoints]] for this service and environment variable."
            ),
        }
    }
}

impl std::error::Error for ComposeIsolationDiagnostic {}

#[derive(Debug)]
pub(crate) struct ComposeIsolationDiagnostics {
    diagnostics: Vec<ComposeIsolationDiagnostic>,
}

impl std::fmt::Display for ComposeIsolationDiagnostics {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let [diagnostic] = self.diagnostics.as_slice() {
            return diagnostic.fmt(formatter);
        }

        writeln!(
            formatter,
            "Docker Compose clone isolation preflight detected {} conflicts:",
            self.diagnostics.len()
        )?;
        for (index, diagnostic) in self.diagnostics.iter().enumerate() {
            write!(formatter, "- {diagnostic}")?;
            if index + 1 < self.diagnostics.len() {
                writeln!(formatter)?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for ComposeIsolationDiagnostics {}

pub(crate) fn validate_compose_isolation_diagnostics(
    findings: &[ComposeIsolationFinding],
) -> std::result::Result<(), Box<ComposeIsolationDiagnostics>> {
    if findings.is_empty() {
        return Ok(());
    }

    let mut diagnostics = findings
        .iter()
        .map(ComposeIsolationDiagnostic::from_finding)
        .collect::<Vec<_>>();
    diagnostics.sort_by_cached_key(ToString::to_string);
    Err(Box::new(ComposeIsolationDiagnostics { diagnostics }))
}

fn optional_value(value: Option<&str>) -> &str {
    value
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("<none>")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::compose_isolation::ComposeIsolationResourceKind;

    #[test]
    fn displays_network_subnet_overlap_code_and_conflict_context() {
        let finding = ComposeIsolationFinding::NetworkSubnetOverlap {
            compose_network: "grpc".to_owned(),
            requested_subnet: "172.28.0.0/16".to_owned(),
            requested_gateway: Some("172.28.0.1".to_owned()),
            docker_network: "other_grpc".to_owned(),
            docker_project: Some("other-project".to_owned()),
            docker_subnet: "172.28.10.0/24".to_owned(),
            docker_gateway: None,
        };

        let message = ComposeIsolationDiagnostic::from_finding(&finding).to_string();

        assert!(message.contains(COMPOSE_NETWORK_SUBNET_OVERLAP));
        assert!(message.contains("network: `grpc`"));
        assert!(message.contains("requested subnet: 172.28.0.0/16"));
        assert!(message.contains("existing network: `other_grpc`"));
        assert!(message.contains("existing compose project: other-project"));
    }

    #[test]
    fn displays_fixed_name_conflict_code_and_conflict_context() {
        let finding = ComposeIsolationFinding::FixedNameConflict {
            kind: ComposeIsolationResourceKind::ServiceContainer,
            compose_resource: "app".to_owned(),
            requested_name: "fixed-app".to_owned(),
            docker_resource_name: "fixed-app".to_owned(),
            docker_project: None,
        };

        let message = ComposeIsolationDiagnostic::from_finding(&finding).to_string();

        assert!(message.contains(COMPOSE_FIXED_NAME_CONFLICT));
        assert!(message.contains("service container `app`"));
        assert!(message.contains("requested name: `fixed-app`"));
        assert!(message.contains("existing resource: container `fixed-app`"));
        assert!(message.contains("existing compose project: <none>"));
    }

    #[test]
    fn validation_reports_all_findings_in_stable_order() {
        let findings = vec![
            ComposeIsolationFinding::FixedNameConflict {
                kind: ComposeIsolationResourceKind::Volume,
                compose_resource: "cache".to_owned(),
                requested_name: "fixed-cache".to_owned(),
                docker_resource_name: "fixed-cache".to_owned(),
                docker_project: None,
            },
            ComposeIsolationFinding::NetworkSubnetOverlap {
                compose_network: "grpc".to_owned(),
                requested_subnet: "172.28.0.0/16".to_owned(),
                requested_gateway: None,
                docker_network: "other-grpc".to_owned(),
                docker_project: None,
                docker_subnet: "172.28.0.0/24".to_owned(),
                docker_gateway: None,
            },
        ];

        let message = validate_compose_isolation_diagnostics(&findings)
            .unwrap_err()
            .to_string();

        assert!(
            message.starts_with("Docker Compose clone isolation preflight detected 2 conflicts:\n")
        );
        assert_eq!(message.matches(COMPOSE_FIXED_NAME_CONFLICT).count(), 1);
        assert_eq!(message.matches(COMPOSE_NETWORK_SUBNET_OVERLAP).count(), 1);
        assert!(
            message.find(COMPOSE_FIXED_NAME_CONFLICT)
                < message.find(COMPOSE_NETWORK_SUBNET_OVERLAP)
        );
    }

    #[test]
    fn validation_preserves_single_diagnostic_message() {
        let finding = ComposeIsolationFinding::FixedNameConflict {
            kind: ComposeIsolationResourceKind::Volume,
            compose_resource: "cache".to_owned(),
            requested_name: "fixed-cache".to_owned(),
            docker_resource_name: "fixed-cache".to_owned(),
            docker_project: None,
        };
        let expected = ComposeIsolationDiagnostic::from_finding(&finding).to_string();

        let actual = validate_compose_isolation_diagnostics(&[finding])
            .unwrap_err()
            .to_string();

        assert_eq!(actual, expected);
    }
}
