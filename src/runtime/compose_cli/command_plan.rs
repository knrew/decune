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
