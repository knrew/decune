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
fn compose_ps_json_accepts_single_object_output() {
    let containers = parse_compose_ps_json(
            br#"{"ID":"app-id","Name":"project-app-1","Service":"app","State":"running","Publishers":null}"#,
            "decune-project-abc123",
            "app",
        )
        .unwrap();

    assert_eq!(containers.len(), 1);
    assert_eq!(containers[0].id, "app-id");
    assert_eq!(containers[0].service, "app");
    assert!(containers[0].published_ports.is_empty());
}

#[test]
fn compose_ps_json_accepts_array_output() {
    let containers = parse_compose_ps_json(
            br#"[{"ID":"app-id","Name":"project-app-1","Service":"app","State":"running","Publishers":[]}]"#,
            "decune-project-abc123",
            "app",
        )
        .unwrap();

    assert_eq!(containers.len(), 1);
    assert_eq!(containers[0].id, "app-id");
}

#[test]
fn compose_ps_json_accepts_json_lines_output() {
    let containers = parse_compose_ps_json(
            b"{\"ID\":\"app-id\",\"Name\":\"project-app-1\",\"Service\":\"app\",\"State\":\"running\",\"Publishers\":[]}\n{\"ID\":\"sidecar-id\",\"Name\":\"project-sidecar-1\",\"Service\":\"sidecar\",\"State\":\"running\",\"Publishers\":[]}\n",
            "decune-project-abc123",
            "app",
        )
        .unwrap();

    assert_eq!(containers.len(), 2);
    assert_eq!(containers[0].id, "app-id");
    assert_eq!(containers[1].service, "sidecar");
}
#[test]
fn compose_ps_fixture_resolves_single_container_id() {
    let containers = serde_json::from_str(
        r#"
            [
              {
                "ID": "abc123",
                "Name": "project-app-1",
                "Service": "app",
                "State": "running",
                "Publishers": [
                  {"URL": "127.0.0.1", "TargetPort": 3000, "PublishedPort": 3000, "Protocol": "tcp"}
                ]
              }
            ]
            "#,
    )
    .unwrap();

    let container = resolve_compose_container("decune-project-abc123", "app", containers).unwrap();

    assert_eq!(container.id, "abc123");
    assert_eq!(container.service, "app");
    assert_eq!(container.state.as_deref(), Some("running"));
    assert_eq!(container.published_ports.len(), 1);
}

#[test]
fn compose_ps_fixture_treats_null_publishers_as_empty_ports() {
    let containers = serde_json::from_str(
        r#"
            [
              {
                "ID": "abc123",
                "Name": "project-app-1",
                "Service": "app",
                "State": "running",
                "Publishers": null
              }
            ]
            "#,
    )
    .unwrap();

    let container = resolve_compose_container("decune-project-abc123", "app", containers).unwrap();

    assert_eq!(container.id, "abc123");
    assert!(container.published_ports.is_empty());
}

#[test]
fn compose_ps_resolution_rejects_zero_containers() {
    let containers = serde_json::from_str("[]").unwrap();

    let error = resolve_compose_container("decune-project-abc123", "app", containers).unwrap_err();

    assert_eq!(
        error.to_string(),
        "Docker Compose project decune-project-abc123 service `app` has no running container"
    );
}

#[test]
fn compose_ps_resolution_rejects_multiple_containers() {
    let containers = serde_json::from_str(
        r#"
            [
              {"ID": "abc123", "Name": "project-app-1", "Service": "app"},
              {"ID": "def456", "Name": "project-app-2", "Service": "app"}
            ]
            "#,
    )
    .unwrap();

    let error = resolve_compose_container("decune-project-abc123", "app", containers).unwrap_err();

    assert_eq!(
        error.to_string(),
        "Docker Compose project decune-project-abc123 service `app` has 2 containers; expected exactly one"
    );
}
