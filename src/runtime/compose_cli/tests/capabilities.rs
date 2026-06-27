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
fn compose_capability_valid_help_output_detects_required_options() {
    let capabilities = valid_compose_capabilities();

    assert_eq!(capabilities.version_short.as_deref(), Some("2.40.0"));
    assert!(capabilities.config_format_json);
    assert!(capabilities.ps_format_json);
    assert!(capabilities.build_with_dependencies);
    assert!(capabilities.pull_policy_always);
    assert!(capabilities.pull_ignore_buildable);
    assert!(capabilities.pull_include_deps);
    assert!(capabilities.up_force_recreate);
    assert!(capabilities.up_remove_orphans);
    capabilities.ensure_required().unwrap();
    capabilities.ensure_compose_override_tag().unwrap();
}

#[test]
fn compose_capability_accepts_override_tag_minimum_version() {
    let capabilities = ComposeCliCapabilities {
        version_short: Some("v2.24.4".to_owned()),
        ..valid_compose_capabilities()
    };

    capabilities.ensure_compose_override_tag().unwrap();
}

#[test]
fn compose_capability_rejects_old_override_tag_version() {
    let capabilities = ComposeCliCapabilities {
        version_short: Some("2.24.3".to_owned()),
        ..valid_compose_capabilities()
    };

    let error = capabilities
        .ensure_compose_override_tag()
        .unwrap_err()
        .to_string();

    assert!(
        error
            .contains("Compose published port relocation requires Docker Compose v2.24.4 or newer")
    );
    assert!(error.contains("detected Docker Compose v2.24.3"));
}

#[test]
fn compose_capability_rejects_unknown_override_tag_version() {
    let capabilities = ComposeCliCapabilities {
        version_short: None,
        ..valid_compose_capabilities()
    };

    let error = capabilities
        .ensure_compose_override_tag()
        .unwrap_err()
        .to_string();

    assert!(
        error
            .contains("Compose published port relocation requires Docker Compose v2.24.4 or newer")
    );
    assert!(error.contains("failed to determine Docker Compose version"));
}

#[test]
fn compose_capability_missing_build_with_dependencies_errors_clearly() {
    let capabilities = ComposeCliCapabilities::from_help_outputs(
        Some("2.3.0".to_owned()),
        "--format string",
        "--format string",
        "--no-cache --pull",
        "--policy string --ignore-buildable --include-deps",
        "--force-recreate --remove-orphans",
    );

    let error = capabilities.ensure_required().unwrap_err().to_string();

    assert!(error.contains("docker compose build --with-dependencies"));
    assert!(error.contains("build --help does not list --with-dependencies"));
    assert!(error.contains("Update Docker Compose v2 plugin"));
}

#[test]
fn compose_capability_missing_pull_include_deps_errors_clearly() {
    let capabilities = ComposeCliCapabilities::from_help_outputs(
        Some("2.3.0".to_owned()),
        "--format string",
        "--format string",
        "--with-dependencies",
        "--policy string --ignore-buildable",
        "--force-recreate --remove-orphans",
    );

    let error = capabilities.ensure_required().unwrap_err().to_string();

    assert!(error.contains("docker compose pull --include-deps"));
    assert!(error.contains("pull --help does not list --include-deps"));
    assert!(error.contains("Update Docker Compose v2 plugin"));
}

#[test]
fn compose_capability_missing_config_format_mentions_config_format_json() {
    let capabilities = ComposeCliCapabilities::from_help_outputs(
        Some("2.3.0".to_owned()),
        "--services",
        "--format string",
        "--with-dependencies",
        "--policy string --ignore-buildable --include-deps",
        "--force-recreate --remove-orphans",
    );

    let error = capabilities.ensure_required().unwrap_err().to_string();

    assert!(error.contains("docker compose config --format json"));
    assert!(error.contains("config --help does not list --format"));
}

#[test]
fn compose_capability_missing_up_options_prompts_compose_plugin_update() {
    let capabilities = ComposeCliCapabilities::from_help_outputs(
        Some("2.3.0".to_owned()),
        "--format string",
        "--format string",
        "--with-dependencies",
        "--policy string --ignore-buildable --include-deps",
        "--detach",
    );

    let error = capabilities.ensure_required().unwrap_err().to_string();

    assert!(error.contains("docker compose up --force-recreate"));
    assert!(error.contains("docker compose up --remove-orphans"));
    assert!(error.contains("Update Docker Compose v2 plugin"));
}
