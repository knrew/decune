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
    pub(crate) resources: Vec<ComposeIsolationResourceNameRewrite>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposeIsolationServiceNameRewrite {
    pub(crate) service: String,
    pub(crate) original_name: String,
    pub(crate) rewritten_name: String,
    pub(crate) networks: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposeIsolationResourceNameRewrite {
    pub(crate) kind: ComposeIsolationResourceKind,
    pub(crate) resource: String,
    pub(crate) original_name: String,
    pub(crate) rewritten_name: String,
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
    pub(crate) scope: Option<String>,
    pub(crate) ipam_driver: Option<String>,
    pub(crate) ipam_configs: Vec<ComposeIsolationDockerIpamConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposeIsolationDockerIpamConfig {
    pub(crate) subnet: Option<String>,
    pub(crate) gateway: Option<String>,
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
}
