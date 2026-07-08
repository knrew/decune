use crate::{
    docker::resource::non_empty_trimmed,
    runtime::{
        compose_cli::{ComposeConfigModel, ComposeConfigResource},
        compose_isolation::{
            ComposeIsolationFixedNameRequest, ComposeIsolationNetworkRequest,
            ComposeIsolationResourceKind, ComposeIsolationScan,
        },
    },
};

pub(crate) fn scan_compose_isolation(
    model: &ComposeConfigModel,
    project_name: &str,
) -> ComposeIsolationScan {
    let mut scan = ComposeIsolationScan::default();

    for (service_name, service) in model.services() {
        if let Some(name) = non_empty_trimmed(service.container_name.as_deref()) {
            scan.fixed_names.push(ComposeIsolationFixedNameRequest {
                kind: ComposeIsolationResourceKind::ServiceContainer,
                resource: service_name.clone(),
                name: name.to_owned(),
            });
        }
    }

    scan_networks(model, project_name, &mut scan);
    scan_named_resources(
        model.volumes(),
        ComposeIsolationResourceKind::Volume,
        project_name,
        &mut scan,
    );
    scan_named_resources(
        model.configs(),
        ComposeIsolationResourceKind::Config,
        project_name,
        &mut scan,
    );
    scan_named_resources(
        model.secrets(),
        ComposeIsolationResourceKind::Secret,
        project_name,
        &mut scan,
    );

    scan
}

fn scan_networks(model: &ComposeConfigModel, project_name: &str, scan: &mut ComposeIsolationScan) {
    for (network_name, network) in model.networks() {
        if network.external.is_external() {
            continue;
        }
        if let Some(ipam) = &network.ipam {
            for config in &ipam.config {
                if let Some(subnet) = non_empty_trimmed(config.subnet.as_deref()) {
                    scan.networks.push(ComposeIsolationNetworkRequest {
                        network: network_name.clone(),
                        subnet: subnet.to_owned(),
                        gateway: non_empty_trimmed(config.gateway.as_deref()).map(str::to_owned),
                    });
                }
            }
        }
        scan_named_resource(
            network_name,
            network,
            ComposeIsolationResourceKind::Network,
            project_name,
            scan,
        );
    }
}

fn scan_named_resources<'a>(
    resources: impl Iterator<Item = (&'a String, &'a ComposeConfigResource)>,
    kind: ComposeIsolationResourceKind,
    project_name: &str,
    scan: &mut ComposeIsolationScan,
) {
    for (resource_name, resource) in resources {
        scan_named_resource(resource_name, resource, kind, project_name, scan);
    }
}

fn scan_named_resource(
    resource_name: &str,
    resource: &ComposeConfigResource,
    kind: ComposeIsolationResourceKind,
    project_name: &str,
    scan: &mut ComposeIsolationScan,
) {
    if resource.external.is_external() {
        return;
    }
    let Some(name) = non_empty_trimmed(resource.name.as_deref()) else {
        return;
    };
    if name == scoped_resource_name(project_name, resource_name) {
        return;
    }
    scan.fixed_names.push(ComposeIsolationFixedNameRequest {
        kind,
        resource: resource_name.to_owned(),
        name: name.to_owned(),
    });
}

fn scoped_resource_name(project_name: &str, resource_name: &str) -> String {
    format!("{project_name}_{resource_name}")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn model(value: serde_json::Value) -> ComposeConfigModel {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn extracts_fixed_subnets_and_fixed_names() {
        let model = model(json!({
            "services": {
                "app": {
                    "image": "alpine:3.20",
                    "container_name": "fixed-app"
                }
            },
            "networks": {
                "grpc": {
                    "name": "fixed-grpc",
                    "ipam": {
                        "config": [
                            {"subnet": "172.28.0.0/16", "gateway": "172.28.0.1"}
                        ]
                    }
                }
            },
            "volumes": {
                "cache": {"name": "fixed-cache"}
            }
        }));

        let scan = scan_compose_isolation(&model, "decune-project-abc123def456");

        assert_eq!(scan.networks.len(), 1);
        assert_eq!(scan.networks[0].network, "grpc");
        assert_eq!(scan.networks[0].subnet, "172.28.0.0/16");
        assert_eq!(scan.networks[0].gateway.as_deref(), Some("172.28.0.1"));
        assert_eq!(scan.fixed_names.len(), 3);
        assert!(scan.fixed_names.iter().any(|name| name.kind
            == ComposeIsolationResourceKind::ServiceContainer
            && name.resource == "app"
            && name.name == "fixed-app"));
        assert!(
            scan.fixed_names
                .iter()
                .any(|name| name.kind == ComposeIsolationResourceKind::Network
                    && name.resource == "grpc"
                    && name.name == "fixed-grpc")
        );
        assert!(
            scan.fixed_names
                .iter()
                .any(|name| name.kind == ComposeIsolationResourceKind::Volume
                    && name.resource == "cache"
                    && name.name == "fixed-cache")
        );
    }

    #[test]
    fn skips_external_resources_and_compose_scoped_default_names() {
        let model = model(json!({
            "services": {
                "app": {"image": "alpine:3.20"}
            },
            "networks": {
                "default": {
                    "name": "decune-project-abc123def456_default",
                    "ipam": {"config": [{"subnet": "172.28.0.0/16"}]}
                },
                "shared": {
                    "name": "shared-network",
                    "external": true,
                    "ipam": {"config": [{"subnet": "172.29.0.0/16"}]}
                }
            },
            "volumes": {
                "cache": {"name": "decune-project-abc123def456_cache"},
                "external-cache": {"name": "shared-cache", "external": true}
            }
        }));

        let scan = scan_compose_isolation(&model, "decune-project-abc123def456");

        assert_eq!(scan.networks.len(), 1);
        assert_eq!(scan.networks[0].network, "default");
        assert!(scan.fixed_names.is_empty());
    }
}
