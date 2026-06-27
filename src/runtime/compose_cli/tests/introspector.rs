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
fn compose_introspector_builds_active_published_port_planning_input() {
    let (_temp, workspace) = fixture_workspace("active-port-planning");
    let devcontainer_dir = workspace.root().join(".devcontainer");
    fs::create_dir(&devcontainer_dir).unwrap();
    write_compose_file(devcontainer_dir.join("compose.yaml"), "services: {}\n");
    let project =
        ComposeProjectPlan::resolve(&workspace, &devcontainer_dir, &["compose.yaml".into()])
            .unwrap();
    let runner = FakeRuntimeCommand::new(vec![Ok(runtime_output(
        br#"{
                "services": {
                    "app": {
                        "image": "alpine:3.20",
                        "ports": [{"target": 3000, "published": "3000"}]
                    },
                    "db": {
                        "image": "alpine:3.20",
                        "ports": [{"target": 5432, "published": "5432"}]
                    }
                }
            }"#,
    ))]);
    let introspector =
        ComposeIntrospector::new(DockerComposeCli::new(std::sync::Arc::new(runner.clone())));
    let run_services = vec!["db".to_owned()];
    let validation = ComposeServiceValidation {
        primary_service: "app",
        run_services: Some(&run_services),
        workspace_folder: "/workspace",
        project_name: project.project_name(),
    };
    let selected_services = vec!["app".to_owned(), "db".to_owned()];
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let input = runtime
        .block_on(introspector.user_published_port_planning_input(
            &project,
            &validation,
            &selected_services,
        ))
        .unwrap();

    assert_eq!(input.port_entries.len(), 2);
    assert_eq!(
        input.services.ordered_services_for_planning(),
        ["app", "db"]
    );
    assert_eq!(
        runner.commands()[0]
            .args_vec()
            .iter()
            .rev()
            .take(4)
            .collect::<Vec<_>>(),
        vec!["db", "app", "json", "--format"]
    );
}

#[test]
fn compose_introspector_includes_dependency_published_ports_from_config_output() {
    let (_temp, workspace) = fixture_workspace("dependency-port-planning");
    let devcontainer_dir = workspace.root().join(".devcontainer");
    fs::create_dir(&devcontainer_dir).unwrap();
    write_compose_file(devcontainer_dir.join("compose.yaml"), "services: {}\n");
    let project =
        ComposeProjectPlan::resolve(&workspace, &devcontainer_dir, &["compose.yaml".into()])
            .unwrap();
    let runner = FakeRuntimeCommand::new(vec![Ok(runtime_output(
        br#"{
                "services": {
                    "app": {
                        "image": "alpine:3.20",
                        "depends_on": {"db": {"condition": "service_started", "required": true}}
                    },
                    "db": {
                        "image": "alpine:3.20",
                        "ports": [{"target": 5432, "published": "5432"}]
                    }
                }
            }"#,
    ))]);
    let introspector =
        ComposeIntrospector::new(DockerComposeCli::new(std::sync::Arc::new(runner.clone())));
    let validation = ComposeServiceValidation {
        primary_service: "app",
        run_services: None,
        workspace_folder: "/workspace",
        project_name: project.project_name(),
    };
    let selected_services = vec!["app".to_owned()];
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let input = runtime
        .block_on(introspector.user_published_port_planning_input(
            &project,
            &validation,
            &selected_services,
        ))
        .unwrap();

    assert_eq!(input.port_entries.len(), 1);
    assert_eq!(input.port_entries[0].service, "db");
    assert_eq!(
        input.services.ordered_services_for_planning(),
        ["app", "db"]
    );
    assert_eq!(
        runner.commands()[0]
            .args_vec()
            .iter()
            .rev()
            .take(3)
            .collect::<Vec<_>>(),
        vec!["app", "json", "--format"]
    );
}
#[test]
fn compose_introspection_reads_user_and_generated_config_paths() {
    let (_temp, workspace) = fixture_workspace("introspection-paths");
    let devcontainer_dir = workspace.root().join(".devcontainer");
    fs::create_dir(&devcontainer_dir).unwrap();
    write_compose_file(devcontainer_dir.join("compose.yaml"), "services: {}\n");
    let project =
        ComposeProjectPlan::resolve(&workspace, &devcontainer_dir, &["compose.yaml".into()])
            .unwrap();
    let runner = FakeRuntimeCommand::new(vec![
        Ok(RuntimeOutput {
            stdout: br#"{"services":{"app":{"image":"generated:latest"}}}"#.to_vec(),
            stderr: Vec::new(),
            exit_code: 0,
        }),
        Ok(RuntimeOutput {
            stdout: br#"{"services":{"app":{"image":"alpine:3.20"}}}"#.to_vec(),
            stderr: Vec::new(),
            exit_code: 0,
        }),
    ]);
    let introspector =
        ComposeIntrospector::new(DockerComposeCli::new(std::sync::Arc::new(runner.clone())));
    let validation = ComposeServiceValidation {
        primary_service: "app",
        run_services: None,
        workspace_folder: "/workspace",
        project_name: project.project_name(),
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let user_model = runtime
        .block_on(introspector.user_config_model(&project, &validation))
        .unwrap();
    let generated_model = runtime
        .block_on(introspector.config_model_with_generated_override(&project, &validation))
        .unwrap();
    let commands = runner.commands();

    assert_eq!(
        user_model
            .service("app")
            .and_then(|service| service.image.as_deref()),
        Some("alpine:3.20")
    );
    assert_eq!(
        generated_model
            .service("app")
            .and_then(|service| service.image.as_deref()),
        Some("generated:latest")
    );
    assert!(
        !commands[0]
            .args_vec()
            .contains(&project.generated_override_path().display().to_string())
    );
    assert!(
        commands[1]
            .args_vec()
            .contains(&project.generated_override_path().display().to_string())
    );
}
