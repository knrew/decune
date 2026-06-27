#![allow(unused_imports)]

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use crate::runtime::command::{FakeRuntimeCommand, RuntimeOutput};
use crate::runtime::compose_cli::{
    ComposeBuildOptions, ComposeCliCapabilities, ComposeCommandPlan, ComposeConfigModel,
    ComposeConfigService, ComposeDownOptions, ComposeIntrospector, ComposeLifecyclePlan,
    ComposeOverrideMount, ComposeOverridePatch, ComposeOverrideServicePatch,
    ComposePrimaryImageResolver, ComposeProjectPlan, ComposePullOptions, ComposeServiceValidation,
    ComposeStopOptions, ComposeUpOptions, DockerComposeCli, resolve_compose_container,
    write_compose_override,
};
use crate::runtime::compose_ports::{
    COMPOSE_PUBLISHED_PORT_COLLISION, ComposePortEligibility, ComposePublishedPortPlan,
    ComposePublishedPortStartupDiagnostics, classify_compose_published_ports,
    compose_published_port_planning_input,
};
use crate::workspace::Workspace;

use super::super::{
    compose_build_command, compose_down_command, compose_pull_command, compose_stop_command,
    compose_up_command, parse_compose_ps_json,
};
use super::test_support::{
    fixture_workspace, lifecycle_command_plan, runtime_error_output, runtime_output,
    valid_compose_capabilities, write_compose_file,
};

#[test]
fn compose_config_model_preserves_service_user() {
    let model: ComposeConfigModel = serde_json::from_value(serde_json::json!({
        "services": {
            "app": {
                "image": "alpine:3.20",
                "user": "1001:1002"
            }
        }
    }))
    .unwrap();

    assert_eq!(
        model
            .service("app")
            .and_then(|service| service.user.as_deref()),
        Some("1001:1002")
    );
}

#[test]
fn compose_config_model_preserves_port_policy_service_context() {
    let model: ComposeConfigModel = serde_json::from_value(serde_json::json!({
        "services": {
            "scaled": {
                "image": "alpine:3.20",
                "scale": 2
            },
            "deployed": {
                "image": "alpine:3.20",
                "deploy": {"replicas": 3}
            },
            "hostnet": {
                "image": "alpine:3.20",
                "network_mode": "host"
            }
        }
    }))
    .unwrap();

    assert_eq!(
        model.service("scaled").unwrap().effective_replica_count(),
        2
    );
    assert_eq!(
        model.service("deployed").unwrap().effective_replica_count(),
        3
    );
    assert!(model.service("hostnet").unwrap().uses_host_network());
}

#[test]
fn compose_primary_image_resolver_uses_service_image_without_build() {
    let model: ComposeConfigModel = serde_json::from_value(serde_json::json!({
        "services": {
            "app": {
                "image": "example/app:dev"
            }
        }
    }))
    .unwrap();

    let image = ComposePrimaryImageResolver {
        project_name: "decune-project-abc123def456",
        service: "app",
    }
    .resolve(&model)
    .unwrap();

    assert_eq!(image.base_image, "example/app:dev");
    assert!(!image.has_build);
}

#[test]
fn compose_primary_image_resolver_uses_compose_build_default_tag_without_image() {
    let model: ComposeConfigModel = serde_json::from_value(serde_json::json!({
        "services": {
            "app": {
                "build": {"context": ".", "dockerfile": "Dockerfile"}
            }
        }
    }))
    .unwrap();

    let image = ComposePrimaryImageResolver {
        project_name: "decune-project-abc123def456",
        service: "app",
    }
    .resolve(&model)
    .unwrap();

    assert_eq!(image.base_image, "decune-project-abc123def456-app");
    assert!(image.has_build);
}

#[test]
fn compose_primary_image_resolver_uses_canonical_image_when_build_is_tagged() {
    let model: ComposeConfigModel = serde_json::from_value(serde_json::json!({
        "services": {
            "app": {
                "image": "example/app:dev",
                "build": {"context": ".", "dockerfile": "Dockerfile"}
            }
        }
    }))
    .unwrap();

    let image = ComposePrimaryImageResolver {
        project_name: "decune-project-abc123def456",
        service: "app",
    }
    .resolve(&model)
    .unwrap();

    assert_eq!(image.base_image, "example/app:dev");
    assert!(image.has_build);
}

#[test]
fn compose_primary_image_resolver_rejects_service_without_image_or_build() {
    let model: ComposeConfigModel = serde_json::from_value(serde_json::json!({
        "services": {
            "app": {}
        }
    }))
    .unwrap();

    let error = ComposePrimaryImageResolver {
        project_name: "decune-project-abc123def456",
        service: "app",
    }
    .resolve(&model)
    .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("did not resolve an image or build")
    );
}

#[test]
fn compose_config_fixture_parses_services_without_rejecting_unknown_fields() {
    let model: ComposeConfigModel = serde_json::from_str(
        r#"
            {
              "name": "ignored",
              "services": {
                "app": {
                  "image": "alpine:3.20",
                  "working_dir": "/workspace",
                  "x-compose-version-dependent": true
                },
                "db": {
                  "build": {"context": ".", "dockerfile": "Dockerfile"}
                }
              },
              "networks": {"default": {"name": "example_default"}}
            }
            "#,
    )
    .unwrap();

    assert!(model.has_service("app"));
    assert!(model.has_service("db"));
    assert_eq!(
        model
            .service("app")
            .and_then(|service| service.image.as_deref()),
        Some("alpine:3.20")
    );
}

#[test]
fn compose_introspection_validation_rejects_missing_primary_service() {
    let model: ComposeConfigModel =
        serde_json::from_str(r#"{"services":{"db":{"image":"postgres:16"}}}"#).unwrap();
    let validation = ComposeServiceValidation {
        primary_service: "app",
        run_services: None,
        workspace_folder: "/workspace",
        project_name: "decune-project-abc123",
    };

    let error = model.validate_services(&validation).unwrap_err();

    assert_eq!(
        error.to_string(),
        "Docker Compose project decune-project-abc123 does not contain primary service `app`. The service may be disabled by Compose profiles"
    );
}

#[test]
fn compose_introspection_validation_rejects_missing_run_service() {
    let model: ComposeConfigModel =
        serde_json::from_str(r#"{"services":{"app":{"image":"alpine:3.20"}}}"#).unwrap();
    let run_services = vec!["app".to_owned(), "db".to_owned()];
    let validation = ComposeServiceValidation {
        primary_service: "app",
        run_services: Some(&run_services),
        workspace_folder: "/workspace",
        project_name: "decune-project-abc123",
    };

    let error = model.validate_services(&validation).unwrap_err();

    assert_eq!(
        error.to_string(),
        "Docker Compose project decune-project-abc123 does not contain runServices service `db`. The service may be disabled by Compose profiles"
    );
}

#[test]
fn compose_introspection_validation_rejects_profile_disabled_primary_service() {
    let model: ComposeConfigModel =
        serde_json::from_str(r#"{"services":{"db":{"image":"postgres:16"}}}"#).unwrap();
    let validation = ComposeServiceValidation {
        primary_service: "app",
        run_services: None,
        workspace_folder: "/workspace",
        project_name: "decune-project-abc123",
    };

    let error = model.validate_services(&validation).unwrap_err();

    assert!(error.to_string().contains("disabled by Compose profiles"));
}

#[test]
fn compose_introspection_validation_rejects_relative_workspace_folder() {
    let model: ComposeConfigModel =
        serde_json::from_str(r#"{"services":{"app":{"image":"alpine:3.20"}}}"#).unwrap();
    let validation = ComposeServiceValidation {
        primary_service: "app",
        run_services: None,
        workspace_folder: "workspace",
        project_name: "decune-project-abc123",
    };

    let error = model.validate_services(&validation).unwrap_err();

    assert_eq!(
        error.to_string(),
        "workspaceFolder must be an absolute container path: workspace"
    );
}
#[test]
fn compose_config_service_deserializes_startup_values() {
    let service: ComposeConfigService = serde_json::from_value(serde_json::json!({
        "image": "alpine:3.20",
        "entrypoint": ["/entrypoint.sh", "--flag"],
        "command": "server --port 3000"
    }))
    .unwrap();

    assert_eq!(
        service.entrypoint,
        Some(vec!["/entrypoint.sh".to_owned(), "--flag".to_owned()])
    );
    assert_eq!(service.command, Some(vec!["server --port 3000".to_owned()]));
}

#[test]
fn compose_config_service_treats_null_startup_as_image_default() {
    let service: ComposeConfigService = serde_json::from_value(serde_json::json!({
        "image": "alpine:3.20",
        "entrypoint": null,
        "command": null
    }))
    .unwrap();

    assert_eq!(service.entrypoint, None);
    assert_eq!(service.command, None);
}

#[test]
fn compose_config_service_preserves_empty_startup_override() {
    let service: ComposeConfigService = serde_json::from_value(serde_json::json!({
        "image": "alpine:3.20",
        "entrypoint": [],
        "command": ""
    }))
    .unwrap();

    assert_eq!(service.entrypoint, Some(Vec::new()));
    assert_eq!(service.command, Some(Vec::new()));
}
