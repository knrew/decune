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
fn compose_config_output_includes_published_port_classification() {
    let runner = FakeRuntimeCommand::new(vec![Ok(runtime_output(
        br#"{
                "services": {
                    "app": {
                        "image": "alpine:3.20",
                        "ports": [
                            {
                                "target": 3000,
                                "published": "3000",
                                "protocol": "tcp"
                            }
                        ]
                    }
                }
            }"#,
    ))]);
    let cli = DockerComposeCli::new(std::sync::Arc::new(runner));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let output = runtime
        .block_on(cli.config_output(&lifecycle_command_plan()))
        .unwrap();

    assert_eq!(output.published_port_entries.len(), 1);
    assert_eq!(output.published_port_entries[0].service, "app");
    assert_eq!(
        output.published_port_entries[0].eligibility,
        ComposePortEligibility::EligibleFixedTcp
    );
}

#[test]
fn compose_config_output_classifies_invalid_port_syntax_errors() {
    let runner = FakeRuntimeCommand::new(vec![Ok(crate::runtime::command::RuntimeOutput {
        stdout: Vec::new(),
        stderr: b"invalid IP address: 999.999.999.999\n".to_vec(),
        exit_code: 1,
    })]);
    let cli = DockerComposeCli::new(std::sync::Arc::new(runner));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let error = runtime
        .block_on(cli.config_output(&lifecycle_command_plan()))
        .unwrap_err()
        .to_string();

    assert!(error.contains("compose_published_port_invalid"));
    assert!(error.contains("invalid IP address"));
}

#[test]
fn compose_up_classifies_published_port_startup_failures() {
    let runner = FakeRuntimeCommand::new(vec![Ok(runtime_error_output(
        "Error response from daemon: Bind for 0.0.0.0:3000 failed: port is already allocated",
    ))]);
    let cli = DockerComposeCli::new(std::sync::Arc::new(runner));
    let model: ComposeConfigModel = serde_json::from_value(serde_json::json!({
        "services": {
            "app": {
                "image": "alpine:3.20",
                "ports": [{"target": 3000, "published": "3000"}]
            }
        }
    }))
    .unwrap();
    let entries = classify_compose_published_ports(&model);
    let input = compose_published_port_planning_input(&model, &entries, "app", &[]);
    let plan = ComposePublishedPortPlan::default();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let error = runtime
        .block_on(cli.up(
            &lifecycle_command_plan(),
            ComposeUpOptions::default(),
            &[],
            Some(ComposePublishedPortStartupDiagnostics {
                input: &input,
                plan: &plan,
                relocation_enabled: false,
            }),
        ))
        .unwrap_err()
        .to_string();

    assert!(error.contains(COMPOSE_PUBLISHED_PORT_COLLISION));
    assert!(error.contains("service: `app`"));
    assert!(!error.contains("Failed to start Docker Compose project"));
}

#[test]
fn compose_config_output_for_services_passes_service_args() {
    let runner = FakeRuntimeCommand::new(vec![Ok(runtime_output(
        br#"{
                "services": {
                    "app": {
                        "image": "alpine:3.20"
                    },
                    "db": {
                        "image": "alpine:3.20"
                    }
                }
            }"#,
    ))]);
    let cli = DockerComposeCli::new(std::sync::Arc::new(runner.clone()));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let services = vec!["app".to_owned(), "db".to_owned()];

    runtime
        .block_on(cli.config_output_for_services(&lifecycle_command_plan(), &services))
        .unwrap();

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
fn compose_capability_probe_runs_version_and_help_commands() {
    let runner = FakeRuntimeCommand::new(vec![
        Ok(runtime_output("--force-recreate --remove-orphans")),
        Ok(runtime_output(
            "--policy string --ignore-buildable --include-deps",
        )),
        Ok(runtime_output("--with-dependencies")),
        Ok(runtime_output("--format string")),
        Ok(runtime_output("--format string")),
        Ok(runtime_output("2.40.0\n")),
    ]);
    let cli = DockerComposeCli::new(std::sync::Arc::new(runner.clone()));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let capabilities = runtime
        .block_on(cli.ensure_required_capabilities())
        .unwrap();
    let commands = runner.commands();

    assert!(capabilities.build_with_dependencies);
    assert_eq!(commands[0].args_vec(), &["compose", "version", "--short"]);
    assert_eq!(commands[1].args_vec(), &["compose", "config", "--help"]);
    assert_eq!(commands[2].args_vec(), &["compose", "ps", "--help"]);
    assert_eq!(commands[3].args_vec(), &["compose", "build", "--help"]);
    assert_eq!(commands[4].args_vec(), &["compose", "pull", "--help"]);
    assert_eq!(commands[5].args_vec(), &["compose", "up", "--help"]);
    assert!(
        commands
            .iter()
            .all(|command| command.current_dir_path().is_none())
    );
}

#[test]
fn compose_capability_probe_does_not_require_version_short() {
    let runner = FakeRuntimeCommand::new(vec![
        Ok(runtime_output("--force-recreate --remove-orphans")),
        Ok(runtime_output(
            "--policy string --ignore-buildable --include-deps",
        )),
        Ok(runtime_output("--with-dependencies")),
        Ok(runtime_output("--format string")),
        Ok(runtime_output("--format string")),
        Ok(runtime_output("Docker Compose version v2.40.0\n")),
        Ok(runtime_error_output("unknown flag: --short")),
    ]);
    let cli = DockerComposeCli::new(std::sync::Arc::new(runner.clone()));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let capabilities = runtime
        .block_on(cli.ensure_required_capabilities())
        .unwrap();
    let commands = runner.commands();

    assert_eq!(capabilities.version_short, None);
    assert_eq!(commands[0].args_vec(), &["compose", "version", "--short"]);
    assert_eq!(commands[1].args_vec(), &["compose", "version"]);
    assert_eq!(commands[2].args_vec(), &["compose", "config", "--help"]);
}
#[test]
fn docker_compose_cli_reads_typed_config_and_ps_json() {
    let runner = FakeRuntimeCommand::new(vec![
        Ok(RuntimeOutput {
            stdout: br#"[{"ID":"abc123","Name":"project-app-1","Service":"app"}]"#.to_vec(),
            stderr: Vec::new(),
            exit_code: 0,
        }),
        Ok(RuntimeOutput {
            stdout: br#"{"services":{"app":{"image":"alpine:3.20"}}}"#.to_vec(),
            stderr: Vec::new(),
            exit_code: 0,
        }),
    ]);
    let cli = DockerComposeCli::new(std::sync::Arc::new(runner.clone()));
    let command_plan = ComposeCommandPlan {
        project_name: "decune-project-abc123def456".to_owned(),
        project_directory: PathBuf::from("/workspace"),
        files: vec![PathBuf::from("/workspace/compose.yaml")],
        env: BTreeMap::new(),
        redactions: Vec::new(),
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let config = runtime
        .block_on(cli.config_output(&command_plan))
        .unwrap()
        .model;
    let ps = runtime.block_on(cli.ps_json(&command_plan, "app")).unwrap();
    let commands = runner.commands();

    assert!(config.has_service("app"));
    assert_eq!(ps.len(), 1);
    assert_eq!(
        commands[0].args_vec().last().map(String::as_str),
        Some("json")
    );
    assert_eq!(
        commands[1].args_vec(),
        &[
            "compose",
            "--project-name",
            "decune-project-abc123def456",
            "--project-directory",
            "/workspace",
            "-f",
            "/workspace/compose.yaml",
            "ps",
            "--format",
            "json",
            "app",
        ]
    );
}
