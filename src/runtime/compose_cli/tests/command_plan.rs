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
fn compose_project_command_uses_docker_compose_plugin_argv() {
    let project = ComposeCommandPlan {
        project_name: "decune-project-abc123".to_owned(),
        project_directory: PathBuf::from("/workspace"),
        files: vec![PathBuf::from("/workspace/compose.yml")],
        env: BTreeMap::new(),
        redactions: Vec::new(),
    };

    let command = project.command(["config", "--format", "json"]);

    assert_eq!(command.program(), "docker");
    assert_eq!(command.args_vec()[0], "compose");
    assert_eq!(command.current_dir_path(), Some(Path::new("/workspace")));
    assert!(command.args_vec().contains(&"--project-name".to_owned()));
    assert!(command.args_vec().contains(&"config".to_owned()));
}

#[test]
fn compose_plan_includes_explicit_project_name_flag() {
    let command_plan = ComposeCommandPlan {
        project_name: "decune-project-abc123def456".to_owned(),
        project_directory: PathBuf::from("/workspace"),
        files: vec![PathBuf::from("/workspace/compose.yaml")],
        env: BTreeMap::new(),
        redactions: Vec::new(),
    };

    let command = command_plan.command(["config", "--format", "json"]);

    assert_eq!(
        command.args_vec(),
        &[
            "compose",
            "--project-name",
            "decune-project-abc123def456",
            "--project-directory",
            "/workspace",
            "-f",
            "/workspace/compose.yaml",
            "config",
            "--format",
            "json",
        ]
    );
    assert_eq!(command.env_value("COMPOSE_PROJECT_NAME"), None);
}

#[test]
fn compose_plan_passes_generated_override_env_as_child_env() {
    let command_plan = ComposeCommandPlan {
        project_name: "decune-project-abc123def456".to_owned(),
        project_directory: PathBuf::from("/workspace"),
        files: vec![PathBuf::from("/workspace/compose.yaml")],
        env: BTreeMap::from([(
            "DECUNE_CONTAINER_ENV_NPM_TOKEN".to_owned(),
            "secret-token".to_owned(),
        )]),
        redactions: vec!["secret-token".to_owned()],
    };

    let command = command_plan.command(["up", "-d"]);

    assert_eq!(
        command
            .env_value("DECUNE_CONTAINER_ENV_NPM_TOKEN")
            .map(String::as_str),
        Some("secret-token")
    );
    assert!(
        !command
            .args_vec()
            .iter()
            .any(|arg| arg.contains("secret-token"))
    );
    assert!(!command.sanitized_display().contains("secret-token"));
}
#[test]
fn compose_lifecycle_up_without_run_services_targets_whole_project() {
    let plan = ComposeLifecyclePlan::up(lifecycle_command_plan(), "app", None);
    let command = compose_up_command(&plan.project, ComposeUpOptions::default(), &plan.services);

    assert!(plan.services.is_empty());
    assert_eq!(
        command.args_vec(),
        &[
            "compose",
            "--project-name",
            "decune-project-abc123def456",
            "--project-directory",
            "/workspace",
            "-f",
            "/workspace/compose.yaml",
            "up",
            "-d",
        ]
    );
}

#[test]
fn compose_lifecycle_up_with_run_services_includes_primary_service_first() {
    let run_services = vec!["db".to_owned()];
    let plan = ComposeLifecyclePlan::up(lifecycle_command_plan(), "app", Some(&run_services));
    let command = compose_up_command(&plan.project, ComposeUpOptions::default(), &plan.services);

    assert_eq!(plan.services, ["app", "db"]);
    assert_eq!(
        command.args_vec().iter().rev().take(4).collect::<Vec<_>>(),
        vec!["db", "app", "-d", "up"]
    );
}

#[test]
fn compose_build_command_with_dependencies_combines_no_cache_and_pull() {
    let services = vec!["app".to_owned()];
    let command = compose_build_command(
        &lifecycle_command_plan(),
        ComposeBuildOptions {
            with_dependencies: true,
            no_cache: true,
            pull: true,
        },
        &services,
    );

    assert_eq!(
        command.args_vec().iter().rev().take(5).collect::<Vec<_>>(),
        vec![
            "app",
            "--pull",
            "--no-cache",
            "--with-dependencies",
            "build"
        ]
    );
}

#[test]
fn compose_lifecycle_rebuild_maps_no_cache_pull_and_force_recreate() {
    let run_services = vec!["db".to_owned()];
    let plan = ComposeLifecyclePlan::up(lifecycle_command_plan(), "app", Some(&run_services));
    let build = compose_build_command(
        &plan.project,
        ComposeBuildOptions {
            with_dependencies: true,
            no_cache: true,
            pull: true,
        },
        &plan.services,
    );
    let up = compose_up_command(
        &plan.project,
        ComposeUpOptions {
            force_recreate: true,
            remove_orphans: false,
        },
        &plan.services,
    );

    assert_eq!(
        build.args_vec().iter().rev().take(6).collect::<Vec<_>>(),
        vec![
            "db",
            "app",
            "--pull",
            "--no-cache",
            "--with-dependencies",
            "build"
        ]
    );
    assert_eq!(
        up.args_vec().iter().rev().take(5).collect::<Vec<_>>(),
        vec!["db", "app", "--force-recreate", "-d", "up"]
    );
}

#[test]
fn compose_up_command_can_remove_orphans() {
    let plan = ComposeLifecyclePlan::up(lifecycle_command_plan(), "app", None);
    let up = compose_up_command(
        &plan.project,
        ComposeUpOptions {
            force_recreate: true,
            remove_orphans: true,
        },
        &plan.services,
    );

    assert!(up.args_vec().contains(&"--force-recreate".to_owned()));
    assert!(up.args_vec().contains(&"--remove-orphans".to_owned()));
}

#[test]
fn compose_pull_command_updates_image_only_services() {
    let run_services = vec!["db".to_owned()];
    let plan = ComposeLifecyclePlan::up(lifecycle_command_plan(), "app", Some(&run_services));
    let pull = compose_pull_command(
        &plan.project,
        ComposePullOptions {
            always: true,
            ignore_buildable: true,
            include_deps: true,
        },
        &plan.services,
    );

    assert_eq!(
        pull.args_vec().iter().rev().take(7).collect::<Vec<_>>(),
        vec![
            "db",
            "app",
            "always",
            "--policy",
            "--include-deps",
            "--ignore-buildable",
            "pull"
        ]
    );
}

#[test]
fn compose_lifecycle_down_stops_whole_project_and_keeps_state_volumes_and_images() {
    let plan = ComposeLifecyclePlan::down(lifecycle_command_plan());
    let command = plan.project.command(["stop"]).args(&plan.services);

    assert!(plan.services.is_empty());
    assert_eq!(
        command.args_vec(),
        &[
            "compose",
            "--project-name",
            "decune-project-abc123def456",
            "--project-directory",
            "/workspace",
            "-f",
            "/workspace/compose.yaml",
            "stop",
        ]
    );
    assert!(!plan.cleanup.remove_project);
    assert!(!plan.cleanup.remove_volumes);
    assert!(!plan.cleanup.remove_state);
    assert!(!plan.cleanup.remove_generated_images);
}

#[test]
fn compose_stop_command_includes_timeout_when_requested() {
    let plan = ComposeLifecyclePlan::down(lifecycle_command_plan());
    let command = compose_stop_command(
        &plan.project,
        ComposeStopOptions {
            timeout_seconds: Some(37),
        },
        &plan.services,
    );

    assert_eq!(
        command.args_vec(),
        &[
            "compose",
            "--project-name",
            "decune-project-abc123def456",
            "--project-directory",
            "/workspace",
            "-f",
            "/workspace/compose.yaml",
            "stop",
            "--timeout",
            "37",
        ]
    );
}

#[test]
fn compose_remove_down_removes_project_volumes_orphans_without_rmi() {
    let plan = ComposeLifecyclePlan::remove(lifecycle_command_plan(), false);
    let command = compose_down_command(
        &plan.project,
        ComposeDownOptions {
            volumes: plan.cleanup.remove_volumes,
            remove_orphans: true,
        },
    );

    assert!(plan.cleanup.remove_project);
    assert!(plan.cleanup.remove_state);
    assert!(!plan.cleanup.remove_generated_images);
    assert!(command.args_vec().contains(&"--volumes".to_owned()));
    assert!(command.args_vec().contains(&"--remove-orphans".to_owned()));
    assert!(!command.args_vec().contains(&"--rmi".to_owned()));
}

#[test]
fn compose_remove_images_targets_only_decune_generated_image_policy() {
    let plan = ComposeLifecyclePlan::remove(lifecycle_command_plan(), true);

    assert!(plan.cleanup.remove_generated_images);
    assert!(plan.services.is_empty());
}
