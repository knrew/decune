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
fn compose_project_name_is_stable_for_same_workspace_path() {
    let (_temp, workspace) = fixture_workspace("Project Name");
    let devcontainer_dir = workspace.root().join(".devcontainer");
    fs::create_dir(&devcontainer_dir).unwrap();
    write_compose_file(devcontainer_dir.join("compose.yaml"), "services: {}\n");

    let first =
        ComposeProjectPlan::resolve(&workspace, &devcontainer_dir, &["compose.yaml".into()])
            .unwrap();
    let second =
        ComposeProjectPlan::resolve(&workspace, &devcontainer_dir, &["compose.yaml".into()])
            .unwrap();

    assert_eq!(first.project_name(), second.project_name());
    assert_eq!(
        first.project_name(),
        format!("decune-project-name-{}", workspace.id())
    );
}

#[test]
fn compose_project_name_includes_workspace_id_for_distinct_workspaces() {
    let (_first_temp, first_workspace) = fixture_workspace("Project Name");
    let (_second_temp, second_workspace) = fixture_workspace("Project Name");
    for workspace in [&first_workspace, &second_workspace] {
        let devcontainer_dir = workspace.root().join(".devcontainer");
        fs::create_dir(&devcontainer_dir).unwrap();
        write_compose_file(devcontainer_dir.join("compose.yaml"), "services: {}\n");
    }

    let first = ComposeProjectPlan::resolve(
        &first_workspace,
        &first_workspace.root().join(".devcontainer"),
        &["compose.yaml".into()],
    )
    .unwrap();
    let second = ComposeProjectPlan::resolve(
        &second_workspace,
        &second_workspace.root().join(".devcontainer"),
        &["compose.yaml".into()],
    )
    .unwrap();

    assert_ne!(first_workspace.id(), second_workspace.id());
    assert_ne!(first.project_name(), second.project_name());
}

#[test]
fn compose_plan_preserves_multi_file_order() {
    let (_temp, workspace) = fixture_workspace("multi-file");
    let devcontainer_dir = workspace.root().join(".devcontainer");
    fs::create_dir(&devcontainer_dir).unwrap();
    write_compose_file(devcontainer_dir.join("compose.yaml"), "services: {}\n");
    write_compose_file(
        devcontainer_dir.join("compose.override.yaml"),
        "services: {}\n",
    );

    let plan = ComposeProjectPlan::resolve(
        &workspace,
        &devcontainer_dir,
        &["compose.yaml".into(), "compose.override.yaml".into()],
    )
    .unwrap();
    let command = plan
        .command_plan_without_generated_override()
        .command(["config"]);

    let file_args = command
        .args_vec()
        .windows(2)
        .filter(|args| args[0] == "-f")
        .map(|args| args[1].clone())
        .collect::<Vec<_>>();

    assert_eq!(
        file_args,
        vec![
            devcontainer_dir
                .join("compose.yaml")
                .canonicalize()
                .unwrap()
                .display()
                .to_string(),
            devcontainer_dir
                .join("compose.override.yaml")
                .canonicalize()
                .unwrap()
                .display()
                .to_string(),
        ]
    );
}

#[test]
fn compose_project_directory_is_first_compose_file_parent() {
    let (_temp, workspace) = fixture_workspace("project-directory");
    let devcontainer_dir = workspace.root().join(".devcontainer");
    let compose_dir = devcontainer_dir.join("compose");
    fs::create_dir_all(&compose_dir).unwrap();
    write_compose_file(compose_dir.join("compose.yaml"), "services: {}\n");
    write_compose_file(
        devcontainer_dir.join("compose.override.yaml"),
        "services: {}\n",
    );

    let plan = ComposeProjectPlan::resolve(
        &workspace,
        &devcontainer_dir,
        &[
            "compose/compose.yaml".into(),
            "compose.override.yaml".into(),
        ],
    )
    .unwrap();

    assert_eq!(
        plan.project_directory(),
        compose_dir.canonicalize().unwrap().as_path()
    );
}

#[cfg(unix)]
#[test]
fn compose_project_directory_uses_declared_first_compose_file_parent_for_symlink() {
    let (_temp, workspace) = fixture_workspace("symlink-project-directory");
    let devcontainer_dir = workspace.root().join(".devcontainer");
    let target_dir = workspace.root().join("shared-compose");
    fs::create_dir_all(&devcontainer_dir).unwrap();
    fs::create_dir(&target_dir).unwrap();
    write_compose_file(target_dir.join("compose.yaml"), "services: {}\n");
    std::os::unix::fs::symlink(
        target_dir.join("compose.yaml"),
        devcontainer_dir.join("compose.yaml"),
    )
    .unwrap();

    let plan = ComposeProjectPlan::resolve(&workspace, &devcontainer_dir, &["compose.yaml".into()])
        .unwrap();

    assert_eq!(
        plan.project_directory(),
        devcontainer_dir.canonicalize().unwrap().as_path()
    );
    assert_eq!(
        plan.config_hash_files()[0].canonical_path,
        target_dir
            .join("compose.yaml")
            .canonicalize()
            .unwrap()
            .display()
            .to_string()
    );
}

#[test]
fn compose_generated_override_path_is_under_state_directory() {
    let (_temp, workspace) = fixture_workspace("generated-override");
    let devcontainer_dir = workspace.root().join(".devcontainer");
    fs::create_dir(&devcontainer_dir).unwrap();
    write_compose_file(devcontainer_dir.join("compose.yaml"), "services: {}\n");

    let plan = ComposeProjectPlan::resolve(&workspace, &devcontainer_dir, &["compose.yaml".into()])
        .unwrap();

    assert_eq!(
        plan.generated_override_path(),
        workspace.paths().state_dir().join("compose.override.yaml")
    );
}

#[test]
fn compose_project_plan_collects_canonical_file_hash_inputs() {
    let (_temp, workspace) = fixture_workspace("config-hash-input");
    let devcontainer_dir = workspace.root().join(".devcontainer");
    fs::create_dir(&devcontainer_dir).unwrap();
    write_compose_file(devcontainer_dir.join("compose.yaml"), "services: {}\n");

    let plan = ComposeProjectPlan::resolve(&workspace, &devcontainer_dir, &["compose.yaml".into()])
        .unwrap();
    let inputs = plan.config_hash_files();

    assert_eq!(inputs.len(), 1);
    assert_eq!(
        inputs[0].canonical_path,
        devcontainer_dir
            .join("compose.yaml")
            .canonicalize()
            .unwrap()
            .display()
            .to_string()
    );
    assert_eq!(inputs[0].digest.len(), 64);
}
#[test]
fn generated_override_file_is_passed_after_user_compose_files() {
    let (_temp, workspace) = fixture_workspace("generated-override-order");
    let devcontainer_dir = workspace.root().join(".devcontainer");
    fs::create_dir(&devcontainer_dir).unwrap();
    write_compose_file(devcontainer_dir.join("compose.yaml"), "services: {}\n");
    write_compose_file(devcontainer_dir.join("dev.yaml"), "services: {}\n");
    let project = ComposeProjectPlan::resolve(
        &workspace,
        &devcontainer_dir,
        &["compose.yaml".into(), "dev.yaml".into()],
    )
    .unwrap();

    let command = project
        .command_plan_with_generated_override()
        .command(["config", "--format", "json"]);
    let file_args = command
        .args_vec()
        .windows(2)
        .filter(|args| args[0] == "-f")
        .map(|args| args[1].clone())
        .collect::<Vec<_>>();

    assert_eq!(
        file_args,
        vec![
            devcontainer_dir
                .join("compose.yaml")
                .canonicalize()
                .unwrap()
                .display()
                .to_string(),
            devcontainer_dir
                .join("dev.yaml")
                .canonicalize()
                .unwrap()
                .display()
                .to_string(),
            workspace
                .paths()
                .state_dir()
                .join("compose.override.yaml")
                .display()
                .to_string(),
        ]
    );
}

#[test]
fn compose_project_plan_rejects_missing_compose_file() {
    let (_temp, workspace) = fixture_workspace("missing-compose-file");
    let devcontainer_dir = workspace.root().join(".devcontainer");
    fs::create_dir(&devcontainer_dir).unwrap();

    let error =
        ComposeProjectPlan::resolve(&workspace, &devcontainer_dir, &["missing.yaml".into()])
            .unwrap_err();
    let message = format!("{error:#}");

    assert!(message.contains("Failed to canonicalize Docker Compose file"));
    assert!(message.contains("missing.yaml"));
}
