use std::{collections::BTreeMap, path::PathBuf};

use crate::runtime::command::RuntimeCommand;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposeCommandPlan {
    pub(crate) project_name: String,
    pub(crate) project_directory: PathBuf,
    pub(crate) files: Vec<PathBuf>,
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) redactions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ComposeBuildOptions {
    pub(crate) with_dependencies: bool,
    pub(crate) no_cache: bool,
    pub(crate) pull: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct ComposePullOptions {
    pub(crate) always: bool,
    pub(crate) ignore_buildable: bool,
    pub(crate) include_deps: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct ComposeUpOptions {
    pub(crate) force_recreate: bool,
    pub(crate) remove_orphans: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct ComposeStopOptions {
    pub(crate) timeout_seconds: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct ComposeDownOptions {
    pub(crate) volumes: bool,
    pub(crate) remove_orphans: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposeLifecyclePlan {
    pub(crate) project: ComposeCommandPlan,
    pub(crate) services: Vec<String>,
    pub(crate) cleanup: ComposeCleanupPlan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposeCleanupPlan {
    pub(crate) remove_project: bool,
    pub(crate) remove_volumes: bool,
    pub(crate) remove_state: bool,
    pub(crate) remove_generated_images: bool,
}

impl ComposeLifecyclePlan {
    pub(crate) fn up(
        project: ComposeCommandPlan,
        primary_service: &str,
        run_services: Option<&[String]>,
    ) -> Self {
        Self {
            project,
            services: compose_target_services(primary_service, run_services),
            cleanup: ComposeCleanupPlan::keep_all(),
        }
    }

    pub(crate) fn down(project: ComposeCommandPlan) -> Self {
        Self {
            project,
            services: Vec::new(),
            cleanup: ComposeCleanupPlan::keep_all(),
        }
    }

    pub(crate) fn remove(project: ComposeCommandPlan, images: bool) -> Self {
        Self {
            project,
            services: Vec::new(),
            cleanup: ComposeCleanupPlan {
                remove_project: true,
                remove_volumes: true,
                remove_state: true,
                remove_generated_images: images,
            },
        }
    }
}

impl ComposeCleanupPlan {
    fn keep_all() -> Self {
        Self {
            remove_project: false,
            remove_volumes: false,
            remove_state: false,
            remove_generated_images: false,
        }
    }
}

impl ComposeCommandPlan {
    pub(crate) fn command<const N: usize>(&self, args: [&str; N]) -> RuntimeCommand {
        let mut command = compose_cmd([])
            .current_dir(self.project_directory.clone())
            .arg("--project-name")
            .arg(&self.project_name)
            .arg("--project-directory")
            .arg(self.project_directory.display().to_string());
        for (key, value) in &self.env {
            command = command.env(key.clone(), value.clone());
        }
        command = command.redact_values(self.redactions.clone());
        for file in &self.files {
            command = command.arg("-f").arg(file.display().to_string());
        }
        command.args(args)
    }

    pub(super) fn file_list(&self) -> String {
        self.files
            .iter()
            .map(|file| file.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

pub(super) fn compose_cmd<const N: usize>(args: [&str; N]) -> RuntimeCommand {
    RuntimeCommand::new("docker").arg("compose").args(args)
}

pub(super) fn compose_config_command(
    project: &ComposeCommandPlan,
    services: &[String],
) -> RuntimeCommand {
    project
        .command(["config", "--format", "json"])
        .args(services)
}

pub(super) fn compose_build_command(
    project: &ComposeCommandPlan,
    options: ComposeBuildOptions,
    services: &[String],
) -> RuntimeCommand {
    let mut command = project.command(["build"]);
    if options.with_dependencies {
        command = command.arg("--with-dependencies");
    }
    if options.no_cache {
        command = command.arg("--no-cache");
    }
    if options.pull {
        command = command.arg("--pull");
    }
    command.args(services)
}

pub(super) fn compose_pull_command(
    project: &ComposeCommandPlan,
    options: ComposePullOptions,
    services: &[String],
) -> RuntimeCommand {
    let mut command = project.command(["pull"]);
    if options.ignore_buildable {
        command = command.arg("--ignore-buildable");
    }
    if options.include_deps {
        command = command.arg("--include-deps");
    }
    if options.always {
        command = command.arg("--policy").arg("always");
    }
    command.args(services)
}

pub(super) fn compose_up_command(
    project: &ComposeCommandPlan,
    options: ComposeUpOptions,
    services: &[String],
) -> RuntimeCommand {
    let mut command = project.command(["up", "-d"]);
    if options.force_recreate {
        command = command.arg("--force-recreate");
    }
    if options.remove_orphans {
        command = command.arg("--remove-orphans");
    }
    command.args(services)
}

pub(super) fn compose_stop_command(
    project: &ComposeCommandPlan,
    options: ComposeStopOptions,
    services: &[String],
) -> RuntimeCommand {
    let mut command = project.command(["stop"]);
    if let Some(timeout_seconds) = options.timeout_seconds {
        command = command.arg("--timeout").arg(timeout_seconds.to_string());
    }
    command.args(services)
}

pub(super) fn compose_down_command(
    project: &ComposeCommandPlan,
    options: ComposeDownOptions,
) -> RuntimeCommand {
    let mut command = project.command(["down"]);
    if options.volumes {
        command = command.arg("--volumes");
    }
    if options.remove_orphans {
        command = command.arg("--remove-orphans");
    }
    command
}

fn compose_target_services(primary_service: &str, run_services: Option<&[String]>) -> Vec<String> {
    let Some(run_services) = run_services else {
        return Vec::new();
    };

    let mut services = Vec::with_capacity(run_services.len() + 1);
    services.push(primary_service.to_owned());
    for service in run_services {
        if !services.iter().any(|existing| existing == service) {
            services.push(service.clone());
        }
    }
    services
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        path::{Path, PathBuf},
    };

    use super::super::test_support::lifecycle_command_plan;
    use super::*;

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
        let command =
            compose_up_command(&plan.project, ComposeUpOptions::default(), &plan.services);

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
        let command =
            compose_up_command(&plan.project, ComposeUpOptions::default(), &plan.services);

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
}
