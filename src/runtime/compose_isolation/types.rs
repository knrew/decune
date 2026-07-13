use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ComposeIsolationResourceKind {
    ServiceContainer,
    Network,
    Volume,
    Config,
    Secret,
}

impl ComposeIsolationResourceKind {
    pub(crate) const fn compose_label(self) -> &'static str {
        match self {
            Self::ServiceContainer => "service container",
            Self::Network => "network",
            Self::Volume => "volume",
            Self::Config => "config",
            Self::Secret => "secret",
        }
    }

    pub(crate) const fn docker_label(self) -> &'static str {
        match self {
            Self::ServiceContainer => "container",
            Self::Network => "network",
            Self::Volume => "volume",
            Self::Config => "config",
            Self::Secret => "secret",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ComposeIsolationScan {
    pub(crate) networks: Vec<ComposeIsolationNetworkRequest>,
    pub(crate) fixed_names: Vec<ComposeIsolationFixedNameRequest>,
}

impl ComposeIsolationScan {
    pub(crate) const fn is_empty(&self) -> bool {
        self.networks.is_empty() && self.fixed_names.is_empty()
    }

    pub(crate) fn has_fixed_names_of_kind(&self, kind: ComposeIsolationResourceKind) -> bool {
        self.fixed_names.iter().any(|fixed| fixed.kind == kind)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposeIsolationNetworkRequest {
    pub(crate) network: String,
    pub(crate) driver: Option<String>,
    pub(crate) ipam_driver: Option<String>,
    pub(crate) subnet: String,
    pub(crate) gateway: Option<String>,
    pub(crate) ip_range: Option<String>,
    pub(crate) aux_addresses: BTreeMap<String, String>,
    pub(crate) has_unrepresented_ipam_configs: bool,
    pub(crate) unsupported_ipam_fields: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposeIsolationFixedNameRequest {
    pub(crate) kind: ComposeIsolationResourceKind,
    pub(crate) resource: String,
    pub(crate) name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ComposeIsolationNameRewritePlan {
    pub(crate) services: Vec<ComposeIsolationServiceNameRewrite>,
    pub(crate) service_references: Vec<ComposeIsolationServiceReferenceRewrite>,
    pub(crate) resources: Vec<ComposeIsolationResourceNameRewrite>,
}

impl ComposeIsolationNameRewritePlan {
    /// Whether any `volumes_from` / `external_links` list must be replaced via
    /// the Compose `!override` tag. Scalar reference rewrites
    /// (`network_mode` / `ipc` / `pid`) never need the tag.
    pub(crate) fn requires_reference_list_override_tag(&self) -> bool {
        self.service_references
            .iter()
            .any(|rewrite| rewrite.volumes_from.is_some() || rewrite.external_links.is_some())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposeIsolationServiceNameRewrite {
    pub(crate) service: String,
    pub(crate) original_name: String,
    pub(crate) rewritten_name: String,
    pub(crate) networks: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposeIsolationServiceReferenceRewrite {
    pub(crate) service: String,
    pub(crate) network_mode: Option<String>,
    pub(crate) ipc: Option<String>,
    pub(crate) pid: Option<String>,
    pub(crate) volumes_from: Option<Vec<String>>,
    pub(crate) external_links: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposeIsolationResourceNameRewrite {
    pub(crate) kind: ComposeIsolationResourceKind,
    pub(crate) resource: String,
    pub(crate) original_name: String,
    pub(crate) rewritten_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ComposeIsolationSubnetPlan {
    pub(crate) allocations: Vec<ComposeIsolationSubnetAllocation>,
    pub(crate) networks_to_remove: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposeIsolationSubnetAllocation {
    pub(crate) network: String,
    pub(crate) requested_subnet: String,
    pub(crate) planned_subnet: String,
    pub(crate) planned_gateway: Option<String>,
    pub(crate) planned_ip_range: Option<String>,
    pub(crate) planned_aux_addresses: BTreeMap<String, String>,
    pub(crate) relocated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposeIsolationEndpointDeclaration {
    pub(crate) service: String,
    pub(crate) env: String,
    pub(crate) value: String,
}

#[derive(Clone, PartialEq, Eq, Default)]
pub(crate) struct ComposeIsolationEndpointPlan {
    pub(crate) services: BTreeMap<String, BTreeMap<String, String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposeIsolationPersistedSubnet {
    pub(crate) network: String,
    pub(crate) requested_subnet: String,
    pub(crate) planned_subnet: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ComposeIsolationDaemonSnapshot {
    pub(crate) networks: Vec<ComposeIsolationDockerNetwork>,
    pub(crate) resources: Vec<ComposeIsolationDockerResource>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposeIsolationDockerNetwork {
    pub(crate) name: String,
    pub(crate) compose_project: Option<String>,
    pub(crate) compose_network: Option<String>,
    pub(crate) scope: Option<String>,
    pub(crate) ipam_driver: Option<String>,
    pub(crate) ipam_configs: Vec<ComposeIsolationDockerIpamConfig>,
    pub(crate) has_attached_containers: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposeIsolationDockerIpamConfig {
    pub(crate) subnet: Option<String>,
    pub(crate) gateway: Option<String>,
    pub(crate) ip_range: Option<String>,
    pub(crate) auxiliary_addresses: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposeIsolationDockerResource {
    pub(crate) kind: ComposeIsolationResourceKind,
    pub(crate) name: String,
    pub(crate) compose_project: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ComposeIsolationFinding {
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
