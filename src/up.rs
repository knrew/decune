use std::{
    future::Future,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use bollard::models::ContainerSummary;

use crate::{
    config::{
        ConfigHashInput, ConfigLayer, ConfigMergeInput, config_hash, load::load_config_file,
        resolve_config, resolved::ResolvedConfig, resolved::ResolvedDevcontainerSource,
        types::MountType,
    },
    devcontainer::{
        json::DevcontainerJson,
        lifecycle::{
            LifecycleRunContext, LifecycleRunPath, run_container_lifecycle,
            run_host_initialize_lifecycle,
        },
        metadata::parse_metadata,
    },
    docker::{
        build::{
            DockerBuildInput, DockerBuildOptions, ResolvedBuildContext, build_hash_input,
            build_image, resolve_build_context,
        },
        client::DockerClient,
        container::{
            ContainerCreateInput, ContainerCreateSpec, create_container,
            devcontainer_keepalive_command, remove_container, start_container, stop_container,
            workspace_container_list_options,
        },
        exec::{ExecCommandSpec, exec_attach, resolve_exec_env, run_attached_exec_stdio},
        image::{PullPolicy, ensure_image},
        mounts::DockerMountSpec,
        resource::DockerResources,
        user::{RemoteUserResolveInput, resolve_remote_user},
    },
    ui,
    workspace::Workspace,
};

const CONFIG_HASH_LABEL: &str = "decune.config_hash";
const REBUILD_STOP_TIMEOUT_SECONDS: i32 = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UpContainerSummary {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) config_hash: Option<String>,
    pub(crate) running: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExistingContainerDecision {
    Create,
    Recreate { containers: Vec<UpContainerSummary> },
    ReuseRunning { id: String, name: String },
    StartStopped { id: String, name: String },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct UpPlan {
    pub(crate) image: String,
    pub(crate) build_context: Option<ResolvedBuildContext>,
    pub(crate) build_options: DockerBuildOptions,
    pub(crate) resources: DockerResources,
    pub(crate) config: ResolvedConfig,
    pub(crate) workspace_folder: String,
    pub(crate) mounts: Vec<DockerMountSpec>,
}

#[derive(Debug, Clone)]
pub(crate) struct UpOptions {
    pub(crate) workspace: PathBuf,
    pub(crate) config_path: Option<PathBuf>,
    pub(crate) cli_layer: ConfigLayer,
    pub(crate) pull: bool,
    pub(crate) rebuild: bool,
    pub(crate) no_cache: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UpOutcome {
    pub(crate) container_id: String,
    pub(crate) container_name: String,
    pub(crate) reused: bool,
}

pub(crate) fn decide_existing_container(
    containers: &[UpContainerSummary],
    expected_config_hash: &str,
    rebuild: bool,
) -> Result<ExistingContainerDecision> {
    if rebuild {
        return if containers.is_empty() {
            Ok(ExistingContainerDecision::Create)
        } else {
            Ok(ExistingContainerDecision::Recreate {
                containers: containers.to_vec(),
            })
        };
    }

    let Some(container) = containers.first() else {
        return Ok(ExistingContainerDecision::Create);
    };

    if container.config_hash.as_deref() != Some(expected_config_hash) {
        bail!("Dev container configuration changed. Run decune rebuild to recreate it.");
    }

    if container.running {
        Ok(ExistingContainerDecision::ReuseRunning {
            id: container.id.clone(),
            name: container.name.clone(),
        })
    } else {
        Ok(ExistingContainerDecision::StartStopped {
            id: container.id.clone(),
            name: container.name.clone(),
        })
    }
}

pub(crate) fn default_workspace_folder(workspace: &Workspace) -> String {
    format!("/workspaces/{}", workspace.basename())
}

pub(crate) fn build_up_plan(
    workspace: &Workspace,
    explicit_config_path: Option<&Path>,
    cli_layer: ConfigLayer,
) -> Result<UpPlan> {
    let devcontainer_json = DevcontainerJson::load(workspace.root(), explicit_config_path)?;
    let metadata = parse_metadata(devcontainer_json.value().clone())?;
    let devcontainer_layer = metadata.to_config_layer()?;
    let global_layer =
        ConfigLayer::from_raw_decune(load_config_file(workspace.paths().global_config_path())?);
    let project_layer =
        ConfigLayer::from_raw_decune(load_config_file(workspace.paths().project_config_path())?);
    let config = resolve_config(ConfigMergeInput {
        image_metadata: None,
        global: Some(global_layer),
        devcontainer: Some(devcontainer_layer),
        project: Some(project_layer),
        cli: Some(cli_layer),
    });
    let (build_context, build_options) =
        dockerfile_build_input(workspace.root(), devcontainer_json.path(), &config)?;
    let mut hash_input = ConfigHashInput::new(&config);
    if let Some(context) = &build_context {
        hash_input.build = Some(build_hash_input(context)?);
    }
    let hash = config_hash(&hash_input);
    let resources = DockerResources::from_workspace(
        workspace,
        hash,
        devcontainer_json.path().display().to_string(),
    );
    let image = image_source(&config, &resources)?;
    let workspace_folder = config
        .devcontainer
        .workspace_folder
        .clone()
        .unwrap_or_else(|| default_workspace_folder(workspace));
    let mounts = workspace_mounts(workspace, &config)?;

    Ok(UpPlan {
        image,
        build_context,
        build_options,
        resources,
        config,
        workspace_folder,
        mounts,
    })
}

pub(crate) async fn run_detached_up(options: UpOptions) -> Result<UpOutcome> {
    let (_, _, outcome) = start_up_container(options).await?;

    Ok(outcome)
}

pub(crate) async fn run_attached_up(options: UpOptions) -> Result<i32> {
    let (client, plan, outcome) = start_up_container(options).await?;
    let exit_code = attach_shell(&client, &plan, &outcome.container_name).await?;

    Ok(clamp_exit_code(exit_code))
}

async fn start_up_container(options: UpOptions) -> Result<(DockerClient, UpPlan, UpOutcome)> {
    let workspace = Workspace::resolve(&options.workspace)?;
    let plan = build_up_plan(
        &workspace,
        options.config_path.as_deref(),
        options.cli_layer,
    )?;
    warn_about_deferred_features(&plan.config);

    let client = DockerClient::connect_from_env()?;
    let containers = list_workspace_containers(&client, workspace.id()).await?;

    match decide_existing_container(&containers, &plan.resources.config_hash, options.rebuild)? {
        ExistingContainerDecision::Create => {
            let outcome = create_detached_container(
                &client,
                workspace.root(),
                &plan,
                options.pull,
                options.no_cache,
            )
            .await?;
            Ok((client, plan, outcome))
        }
        ExistingContainerDecision::Recreate { containers } => {
            recreate_existing_containers(&client, &containers).await?;
            let outcome = create_detached_container(
                &client,
                workspace.root(),
                &plan,
                options.pull,
                options.no_cache,
            )
            .await?;
            Ok((client, plan, outcome))
        }
        ExistingContainerDecision::ReuseRunning { id, name } => {
            run_lifecycle_for_container(
                &client,
                workspace.root(),
                &plan,
                &name,
                LifecycleRunPath::Running,
            )
            .await?;
            ui::done(&format!("Reusing running dev container: {name}"));
            Ok(UpOutcome {
                container_id: id,
                container_name: name,
                reused: true,
            })
            .map(|outcome| (client, plan, outcome))
        }
        ExistingContainerDecision::StartStopped { id, name } => {
            start_container(&client, &name).await?;
            run_lifecycle_for_container(
                &client,
                workspace.root(),
                &plan,
                &name,
                LifecycleRunPath::Started,
            )
            .await?;
            ui::done(&format!("Started existing dev container: {name}"));
            Ok(UpOutcome {
                container_id: id,
                container_name: name,
                reused: true,
            })
            .map(|outcome| (client, plan, outcome))
        }
    }
}

async fn recreate_existing_containers(
    client: &DockerClient,
    containers: &[UpContainerSummary],
) -> Result<()> {
    for container in containers {
        stop_container(client, &container.id, REBUILD_STOP_TIMEOUT_SECONDS).await?;
        remove_container(client, &container.id, true, false).await?;
        ui::done(&format!(
            "Removed existing dev container for rebuild: {}",
            container.name
        ));
    }

    Ok(())
}

async fn create_detached_container(
    client: &DockerClient,
    workspace_root: &Path,
    plan: &UpPlan,
    pull: bool,
    no_cache: bool,
) -> Result<UpOutcome> {
    run_host_initialize_lifecycle(&plan.config, workspace_root)?;

    if let Some(context) = plan.build_context.clone() {
        let mut build_options = plan.build_options.clone();
        build_options.pull = pull;
        build_options.no_cache = no_cache;
        build_image(
            client,
            DockerBuildInput {
                image_tag: plan.image.clone(),
                labels: plan.resources.labels.clone().into_iter().collect(),
                context,
                options: build_options,
            },
        )
        .await?;
    } else {
        ensure_image(
            client,
            &plan.image,
            if pull {
                PullPolicy::Always
            } else {
                PullPolicy::Missing
            },
        )
        .await?;
    }

    let (entrypoint, command) = if plan.config.devcontainer.override_command {
        let (entrypoint, command) = devcontainer_keepalive_command();
        (Some(entrypoint), Some(command))
    } else {
        (None, None)
    };
    let spec = ContainerCreateSpec::from_resolved(ContainerCreateInput {
        image: &plan.image,
        resources: &plan.resources,
        config: &plan.config,
        entrypoint,
        command,
        working_dir: Some(plan.workspace_folder.clone()),
        mounts: plan.mounts.clone(),
    });
    let container_id = create_container(client, &spec).await?;
    start_new_container(client, &plan.resources.container_name).await?;
    run_lifecycle_for_container(
        client,
        workspace_root,
        plan,
        &plan.resources.container_name,
        LifecycleRunPath::New,
    )
    .await?;
    ui::done(&format!(
        "Started dev container: {}",
        plan.resources.container_name
    ));

    Ok(UpOutcome {
        container_id,
        container_name: plan.resources.container_name.clone(),
        reused: false,
    })
}

async fn run_lifecycle_for_container(
    client: &DockerClient,
    workspace_root: &Path,
    plan: &UpPlan,
    container_name: &str,
    path: LifecycleRunPath,
) -> Result<()> {
    let remote_user = resolve_remote_user(
        client,
        container_name,
        RemoteUserResolveInput {
            explicit_remote_user: plan.config.devcontainer.remote_user.as_deref(),
            image_metadata_remote_user: None,
        },
    )
    .await?;

    run_container_lifecycle(
        path,
        LifecycleRunContext {
            client,
            container: container_name,
            config: &plan.config,
            workspace_root,
            workspace_folder: &plan.workspace_folder,
            remote_user: &remote_user,
        },
    )
    .await
}

async fn attach_shell(client: &DockerClient, plan: &UpPlan, container_name: &str) -> Result<i64> {
    let remote_user = resolve_remote_user(
        client,
        container_name,
        RemoteUserResolveInput {
            explicit_remote_user: plan.config.devcontainer.remote_user.as_deref(),
            image_metadata_remote_user: None,
        },
    )
    .await?;
    let env = resolve_exec_env(
        client,
        container_name,
        &remote_user.user,
        remote_user.shell.as_deref(),
        &plan.config.devcontainer.remote_env,
        plan.config.devcontainer.user_env_probe,
    )
    .await?;
    let candidates =
        shell_command_candidates(plan.config.shell.as_deref(), remote_user.shell.as_deref());
    let (spec, attached) = first_successful_shell_candidate(candidates, |command| {
        let env = env.clone();
        let user = remote_user.user.clone();
        let working_dir = plan.workspace_folder.clone();

        async move {
            let spec = ExecCommandSpec {
                command: vec![command],
                user: Some(user),
                working_dir: Some(working_dir),
                env,
                tty: true,
            };
            let attached = exec_attach(client, container_name, &spec).await?;

            Ok::<_, anyhow::Error>((spec, attached))
        }
    })
    .await
    .with_context(|| format!("Failed to start an attached shell in container: {container_name}"))?;

    run_attached_exec_stdio(client, container_name, &spec, attached).await
}

pub(crate) async fn first_successful_shell_candidate<T, F, Fut>(
    candidates: Vec<String>,
    mut start_candidate: F,
) -> Result<T>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    if candidates.is_empty() {
        bail!("No shell command candidate is available");
    }

    let mut failures = Vec::new();
    for command in candidates {
        match start_candidate(command.clone()).await {
            Ok(result) => return Ok(result),
            Err(error) => failures.push(format!("{command}: {error:#}")),
        }
    }

    bail!(
        "Failed to start any shell command candidate. Tried: {}",
        failures.join("; ")
    )
}

pub(crate) fn shell_command_candidates(
    config_shell: Option<&str>,
    remote_user_shell: Option<&str>,
) -> Vec<String> {
    if let Some(shell) = normalized_shell(config_shell) {
        return vec![shell];
    }

    let mut candidates = Vec::new();
    if let Some(shell) = normalized_shell(remote_user_shell) {
        candidates.push(shell);
    }
    candidates.push("/bin/bash".to_owned());
    candidates.push("/bin/sh".to_owned());
    candidates.dedup();
    candidates
}

fn normalized_shell(shell: Option<&str>) -> Option<String> {
    shell
        .map(str::trim)
        .filter(|shell| !shell.is_empty())
        .map(ToOwned::to_owned)
}

fn clamp_exit_code(exit_code: i64) -> i32 {
    match exit_code {
        0..=255 => exit_code as i32,
        _ => 1,
    }
}

async fn start_new_container(client: &DockerClient, container_name: &str) -> Result<()> {
    match start_container(client, container_name).await {
        Ok(()) => Ok(()),
        Err(start_error) => {
            let cleanup = remove_container(client, container_name, true, true).await;
            match cleanup {
                Ok(()) => Err(start_error),
                Err(cleanup_error) => Err(start_error.context(format!(
                    "Failed to remove Docker container after start failure: {container_name}: {cleanup_error:#}"
                ))),
            }
        }
    }
}

fn image_source(config: &ResolvedConfig, resources: &DockerResources) -> Result<String> {
    match &config.devcontainer.source {
        Some(ResolvedDevcontainerSource::Image(image)) => Ok(image.clone()),
        Some(ResolvedDevcontainerSource::Dockerfile(_)) => Ok(resources.image_tag.clone()),
        None => bail!("Devcontainer image is required"),
    }
}

fn dockerfile_build_input(
    workspace_root: &Path,
    devcontainer_file: &Path,
    config: &ResolvedConfig,
) -> Result<(Option<ResolvedBuildContext>, DockerBuildOptions)> {
    match &config.devcontainer.source {
        Some(ResolvedDevcontainerSource::Dockerfile(build)) => Ok((
            Some(resolve_build_context(
                workspace_root,
                devcontainer_file,
                build,
            )?),
            DockerBuildOptions {
                build_args: build.args.clone(),
                target: build.target.clone(),
                cache_from: build.cache_from.clone(),
                ..DockerBuildOptions::default()
            },
        )),
        _ => Ok((None, DockerBuildOptions::default())),
    }
}

fn workspace_mounts(
    workspace: &Workspace,
    config: &ResolvedConfig,
) -> Result<Vec<DockerMountSpec>> {
    if config.devcontainer.workspace_mount.is_some() {
        bail!("workspaceMount is not supported yet");
    }

    Ok(vec![DockerMountSpec {
        source: Some(workspace.root().display().to_string()),
        target: default_workspace_folder(workspace),
        mount_type: MountType::Bind,
        read_only: false,
    }])
}

async fn list_workspace_containers(
    client: &DockerClient,
    workspace_id: &str,
) -> Result<Vec<UpContainerSummary>> {
    let containers = client
        .raw()
        .list_containers(Some(workspace_container_list_options(workspace_id)))
        .await
        .with_context(|| {
            format!("Failed to list Docker containers for workspace: {workspace_id}")
        })?;

    Ok(containers
        .into_iter()
        .filter_map(container_summary)
        .collect())
}

fn container_summary(container: ContainerSummary) -> Option<UpContainerSummary> {
    let id = container.id?;
    let name = container
        .names
        .and_then(|names| names.into_iter().next())
        .map(|name| name.trim_start_matches('/').to_owned())
        .unwrap_or_else(|| id.clone());
    let config_hash = container
        .labels
        .and_then(|labels| labels.get(CONFIG_HASH_LABEL).cloned());
    let running = container
        .state
        .is_some_and(|state| state.to_string() == "running");

    Some(UpContainerSummary {
        id,
        name,
        config_hash,
        running,
    })
}

fn warn_about_deferred_features(config: &ResolvedConfig) {
    if !config.features.is_empty() {
        ui::warn("Dev Container Features are not applied yet in this milestone");
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs, path::PathBuf};

    use crate::config::ConfigLayer;
    use crate::config::resolved::ResolvedDevcontainerSource;
    use crate::config::types::MountType;
    use crate::docker::client::DockerClient;
    use crate::docker::container::remove_container;
    use crate::docker::exec::{ExecCommandSpec, exec_capture};
    use crate::docker::image::remove_image;
    use crate::workspace::Workspace;

    use super::{
        ExistingContainerDecision, UpContainerSummary, UpOptions, build_up_plan,
        decide_existing_container, default_workspace_folder, first_successful_shell_candidate,
        list_workspace_containers, run_detached_up, shell_command_candidates,
    };

    #[test]
    fn existing_container_decision_creates_when_no_container_exists() {
        let decision = decide_existing_container(&[], "hash123", false).unwrap();

        assert_eq!(decision, ExistingContainerDecision::Create);
    }

    #[test]
    fn shell_candidates_use_only_explicit_config_shell() {
        assert_eq!(
            shell_command_candidates(Some(" /bin/zsh "), Some("/bin/fish")),
            vec!["/bin/zsh".to_owned()]
        );
    }

    #[test]
    fn shell_candidates_use_remote_login_shell_before_fallbacks() {
        assert_eq!(
            shell_command_candidates(None, Some("/bin/fish")),
            vec![
                "/bin/fish".to_owned(),
                "/bin/bash".to_owned(),
                "/bin/sh".to_owned()
            ]
        );
    }

    #[test]
    fn shell_candidates_fall_back_to_bash_then_sh() {
        assert_eq!(
            shell_command_candidates(None, None),
            vec!["/bin/bash".to_owned(), "/bin/sh".to_owned()]
        );
    }

    #[test]
    fn shell_candidate_fallback_tries_next_auto_candidate_after_start_failure() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let selected = runtime
            .block_on(first_successful_shell_candidate(
                vec!["/bin/bash".to_owned(), "/bin/sh".to_owned()],
                |command| async move {
                    if command == "/bin/bash" {
                        anyhow::bail!("start failed");
                    }

                    Ok::<_, anyhow::Error>(command)
                },
            ))
            .unwrap();

        assert_eq!(selected, "/bin/sh");
    }

    #[test]
    fn existing_container_decision_reuses_running_container_with_matching_hash() {
        let container = UpContainerSummary {
            id: "container-id".to_owned(),
            name: "decune-project-abc123".to_owned(),
            config_hash: Some("hash123".to_owned()),
            running: true,
        };

        let decision = decide_existing_container(&[container], "hash123", false).unwrap();

        assert_eq!(
            decision,
            ExistingContainerDecision::ReuseRunning {
                id: "container-id".to_owned(),
                name: "decune-project-abc123".to_owned()
            }
        );
    }

    #[test]
    fn existing_container_decision_starts_stopped_container_with_matching_hash() {
        let container = UpContainerSummary {
            id: "container-id".to_owned(),
            name: "decune-project-abc123".to_owned(),
            config_hash: Some("hash123".to_owned()),
            running: false,
        };

        let decision = decide_existing_container(&[container], "hash123", false).unwrap();

        assert_eq!(
            decision,
            ExistingContainerDecision::StartStopped {
                id: "container-id".to_owned(),
                name: "decune-project-abc123".to_owned()
            }
        );
    }

    #[test]
    fn existing_container_decision_rejects_changed_config_hash() {
        let container = UpContainerSummary {
            id: "container-id".to_owned(),
            name: "decune-project-abc123".to_owned(),
            config_hash: Some("old-hash".to_owned()),
            running: true,
        };

        let error = decide_existing_container(&[container], "new-hash", false).unwrap_err();

        assert!(error.to_string().contains("Run decune rebuild"));
    }

    #[test]
    fn existing_container_decision_recreates_when_rebuild_is_requested() {
        let container = UpContainerSummary {
            id: "container-id".to_owned(),
            name: "decune-project-abc123".to_owned(),
            config_hash: Some("old-hash".to_owned()),
            running: true,
        };

        let decision = decide_existing_container(&[container.clone()], "new-hash", true).unwrap();

        assert_eq!(
            decision,
            ExistingContainerDecision::Recreate {
                containers: vec![container]
            }
        );
    }

    #[test]
    fn default_workspace_folder_uses_workspace_basename() {
        let workspace = test_workspace("project");

        assert_eq!(default_workspace_folder(&workspace), "/workspaces/project");
    }

    #[test]
    fn build_up_plan_uses_image_source_and_default_workspace_mount() {
        let workspace = test_workspace("image-plan");
        write_devcontainer(
            &workspace,
            r#"
            {
              "image": "alpine:3.20",
              "workspaceFolder": "/workspace"
            }
            "#,
        );

        let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_eq!(plan.image, "alpine:3.20");
        assert!(plan.build_context.is_none());
        assert_eq!(plan.workspace_folder, "/workspace");
        assert_eq!(plan.mounts.len(), 1);
        assert_eq!(
            plan.mounts[0].source.as_deref(),
            Some(workspace.root().to_str().unwrap())
        );
        assert_eq!(plan.mounts[0].target, "/workspaces/image-plan");
        assert_eq!(plan.mounts[0].mount_type, MountType::Bind);
        assert!(!plan.mounts[0].read_only);
        assert!(matches!(
            plan.config.devcontainer.source,
            Some(ResolvedDevcontainerSource::Image(ref image)) if image == "alpine:3.20"
        ));
        assert_eq!(
            plan.resources.labels["devcontainer.config_file"],
            workspace
                .root()
                .join(".devcontainer/devcontainer.json")
                .display()
                .to_string()
        );
    }

    #[test]
    fn build_up_plan_uses_dockerfile_source_and_build_context() {
        let workspace = test_workspace("dockerfile-plan");
        fs::create_dir_all(workspace.root().join(".devcontainer")).unwrap();
        fs::write(
            workspace.root().join(".devcontainer/Dockerfile"),
            "FROM alpine\n",
        )
        .unwrap();
        write_devcontainer(
            &workspace,
            r#"
            {
              "build": {
                "dockerfile": "Dockerfile",
                "args": {
                  "VARIANT": "bookworm"
                },
                "target": "dev",
                "cacheFrom": "type=registry,ref=example.test/cache:latest"
              }
            }
            "#,
        );

        let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_eq!(plan.image, plan.resources.image_tag);
        let build_context = plan
            .build_context
            .expect("build context should be resolved");
        assert_eq!(
            build_context.context_dir,
            workspace.root().join(".devcontainer")
        );
        assert_eq!(
            build_context.dockerfile_path,
            workspace.root().join(".devcontainer/Dockerfile")
        );
        assert_eq!(
            build_context.dockerfile_in_context,
            PathBuf::from("Dockerfile")
        );
        assert_eq!(
            plan.build_options
                .build_args
                .get("VARIANT")
                .map(String::as_str),
            Some("bookworm")
        );
        assert_eq!(plan.build_options.target.as_deref(), Some("dev"));
        assert_eq!(
            plan.build_options.cache_from,
            vec!["type=registry,ref=example.test/cache:latest"]
        );
        assert!(!plan.build_options.no_cache);
        assert!(!plan.build_options.pull);
    }

    #[test]
    fn build_up_plan_hash_changes_when_dockerfile_content_changes() {
        let workspace = test_workspace("dockerfile-hash-plan");
        fs::create_dir_all(workspace.root().join(".devcontainer")).unwrap();
        fs::write(
            workspace.root().join(".devcontainer/Dockerfile"),
            "FROM alpine\n",
        )
        .unwrap();
        write_devcontainer(
            &workspace,
            r#"
            {
              "build": {
                "dockerfile": "Dockerfile"
              }
            }
            "#,
        );

        let first = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
        fs::write(
            workspace.root().join(".devcontainer/Dockerfile"),
            "FROM alpine\nRUN true\n",
        )
        .unwrap();
        let second = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_ne!(first.resources.config_hash, second.resources.config_hash);
        assert_ne!(first.image, second.image);
    }

    #[test]
    fn up_detach_creates_and_reuses_container_when_docker_tests_are_enabled() {
        if !docker_tests_enabled() {
            eprintln!("skipped: set DECUNE_DOCKER_TESTS=1 to run Docker integration tests");
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-detach");
            write_devcontainer(
                &workspace,
                r#"
                {
                  "image": "alpine:3.20"
                }
                "#,
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;

                let first = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                })
                .await?;
                assert_eq!(first.container_name, container_name);
                assert!(!first.reused);

                let inspect = client
                    .raw()
                    .inspect_container(&container_name, None)
                    .await?;
                assert_eq!(inspect.state.and_then(|state| state.running), Some(true));

                let second = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                })
                .await?;
                assert_eq!(second.container_name, container_name);
                assert!(second.reused);

                Ok(())
            }
            .await;

            let cleanup = remove_container(&client, &container_name, true, true).await;
            result.and(cleanup).unwrap();
        });
    }

    #[test]
    fn up_detach_reuses_container_when_built_image_tag_is_removed_if_docker_tests_are_enabled() {
        if !docker_tests_enabled() {
            eprintln!("skipped: set DECUNE_DOCKER_TESTS=1 to run Docker integration tests");
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-reuse-missing-image-tag");
            fs::create_dir_all(workspace.root().join(".devcontainer")).unwrap();
            fs::write(
                workspace.root().join(".devcontainer/Dockerfile"),
                r#"
                FROM alpine:3.20
                RUN adduser -D vscode
                USER vscode
                "#,
            )
            .unwrap();
            write_devcontainer(
                &workspace,
                r#"
                {
                  "build": {
                    "dockerfile": "Dockerfile"
                  }
                }
                "#,
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let image = plan.image.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;

                let first = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                })
                .await?;
                assert!(!first.reused);

                remove_image(&client, &image, true).await?;

                let second = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                })
                .await?;
                assert_eq!(second.container_name, container_name);
                assert!(second.reused);

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }

    #[test]
    fn up_detach_stops_lifecycle_after_failure_when_docker_tests_are_enabled() {
        if !docker_tests_enabled() {
            eprintln!("skipped: set DECUNE_DOCKER_TESTS=1 to run Docker integration tests");
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-lifecycle-failure");
            write_devcontainer(
                &workspace,
                r#"
                {
                  "image": "alpine:3.20",
                  "onCreateCommand": "printf on-create >/tmp/decune-lifecycle; exit 7",
                  "updateContentCommand": "printf update-content >>/tmp/decune-lifecycle"
                }
                "#,
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;

                let error = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                })
                .await
                .unwrap_err();
                let message = format!("{error:#}");
                assert!(message.contains("Lifecycle stage onCreateCommand failed"));
                assert!(message.contains("exit code 7"));

                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "cat /tmp/decune-lifecycle".to_owned(),
                        ],
                        user: None,
                        working_dir: None,
                        env: BTreeMap::new(),
                        tty: false,
                    },
                )
                .await?;

                assert_eq!(String::from_utf8(output.stdout).unwrap(), "on-create");

                Ok(())
            }
            .await;

            let cleanup = remove_container(&client, &container_name, true, true).await;
            result.and(cleanup).unwrap();
        });
    }

    #[test]
    fn up_detach_waits_for_parallel_lifecycle_siblings_after_failure_if_docker_tests_are_enabled() {
        if !docker_tests_enabled() {
            eprintln!("skipped: set DECUNE_DOCKER_TESTS=1 to run Docker integration tests");
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-parallel-lifecycle-failure");
            write_devcontainer(
                &workspace,
                r#"
                {
                  "image": "alpine:3.20",
                  "postAttachCommand": {
                    "a_slow": "sleep 1; printf done >/tmp/decune-parallel-lifecycle",
                    "z_fail": "exit 7"
                  }
                }
                "#,
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;

                let error = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                })
                .await
                .unwrap_err();
                let message = format!("{error:#}");
                assert!(message.contains("Lifecycle stage postAttachCommand.z_fail failed"));
                assert!(message.contains("exit code 7"));

                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "cat /tmp/decune-parallel-lifecycle".to_owned(),
                        ],
                        user: None,
                        working_dir: None,
                        env: BTreeMap::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(String::from_utf8(output.stdout).unwrap(), "done");

                Ok(())
            }
            .await;

            let cleanup = remove_container(&client, &container_name, true, true).await;
            result.and(cleanup).unwrap();
        });
    }

    #[test]
    fn up_detach_applies_remote_env_to_lifecycle_when_docker_tests_are_enabled() {
        if !docker_tests_enabled() {
            eprintln!("skipped: set DECUNE_DOCKER_TESTS=1 to run Docker integration tests");
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-lifecycle-remote-env");
            write_devcontainer(
                &workspace,
                r#"
                {
                  "image": "alpine:3.20",
                  "remoteEnv": {
                    "DECUNE_REMOTE_ENV_SENTINEL": "from-remote-env"
                  },
                  "postAttachCommand": "test \"$DECUNE_REMOTE_ENV_SENTINEL\" = from-remote-env && printf '%s' \"$DECUNE_REMOTE_ENV_SENTINEL\" >/tmp/decune-remote-env"
                }
                "#,
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;

                run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                })
                .await?;

                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "cat /tmp/decune-remote-env".to_owned(),
                        ],
                        user: None,
                        working_dir: None,
                        env: BTreeMap::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(String::from_utf8(output.stdout).unwrap(), "from-remote-env");

                Ok(())
            }
            .await;

            let cleanup = remove_container(&client, &container_name, true, true).await;
            result.and(cleanup).unwrap();
        });
    }

    #[test]
    fn up_detach_applies_user_env_probe_to_lifecycle_when_docker_tests_are_enabled() {
        if !docker_tests_enabled() {
            eprintln!("skipped: set DECUNE_DOCKER_TESTS=1 to run Docker integration tests");
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-lifecycle-user-env-probe");
            fs::create_dir_all(workspace.root().join(".devcontainer")).unwrap();
            fs::write(
                workspace.root().join(".devcontainer/Dockerfile"),
                r#"
                FROM alpine:3.20
                RUN printf '%s\n' \
                  'export DECUNE_PROBED_ENV=from-profile' \
                  'export DECUNE_ENV_PRIORITY=from-profile' \
                  >/etc/profile.d/decune-probe.sh
                "#,
            )
            .unwrap();
            write_devcontainer(
                &workspace,
                r#"
                {
                  "build": {
                    "dockerfile": "Dockerfile"
                  },
                  "userEnvProbe": "loginShell",
                  "remoteEnv": {
                    "DECUNE_ENV_PRIORITY": "from-remote-env"
                  },
                  "postAttachCommand": [
                    "/bin/sh",
                    "-c",
                    "test \"$DECUNE_PROBED_ENV\" = from-profile && test \"$DECUNE_ENV_PRIORITY\" = from-remote-env && printf '%s:%s' \"$DECUNE_PROBED_ENV\" \"$DECUNE_ENV_PRIORITY\" >/tmp/decune-user-env-probe"
                  ]
                }
                "#,
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let image = plan.image.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;

                run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                })
                .await?;

                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "cat /tmp/decune-user-env-probe".to_owned(),
                        ],
                        user: None,
                        working_dir: None,
                        env: BTreeMap::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(
                    String::from_utf8(output.stdout).unwrap(),
                    "from-profile:from-remote-env"
                );

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }

    #[test]
    fn up_detach_omits_remote_probe_env_for_root_hook_when_docker_tests_are_enabled() {
        if !docker_tests_enabled() {
            eprintln!("skipped: set DECUNE_DOCKER_TESTS=1 to run Docker integration tests");
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-root-hook-user-env-probe");
            fs::create_dir_all(workspace.root().join(".devcontainer")).unwrap();
            fs::write(
                workspace.root().join(".devcontainer/Dockerfile"),
                r#"
                FROM alpine:3.20
                RUN printf '%s\n' \
                  '#!/bin/sh' \
                  'export DECUNE_REMOTE_ONLY=from-decune' \
                  'exec /bin/sh "$@"' \
                  >/usr/local/bin/decune-probe-shell \
                  && chmod +x /usr/local/bin/decune-probe-shell \
                  && adduser -D -s /usr/local/bin/decune-probe-shell decune
                "#,
            )
            .unwrap();
            write_devcontainer(
                &workspace,
                r#"
                {
                  "build": {
                    "dockerfile": "Dockerfile"
                  },
                  "remoteUser": "decune",
                  "userEnvProbe": "loginShell",
                  "remoteEnv": {
                    "DECUNE_REMOTE_ENV": "from-remote-env"
                  }
                }
                "#,
            );
            fs::create_dir_all(workspace.root().join(".decune")).unwrap();
            fs::write(
                workspace.root().join(".decune/config.toml"),
                r#"
version = 1

[[hooks.before_post_attach]]
command = "test -z \"${DECUNE_REMOTE_ONLY+x}\" && test \"$DECUNE_REMOTE_ENV\" = from-remote-env && printf '%s' root-hook-clean >/tmp/decune-root-hook-env"
user = "root"
"#,
            )
            .unwrap();
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let image = plan.image.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;

                run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                })
                .await?;

                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "cat /tmp/decune-root-hook-env".to_owned(),
                        ],
                        user: None,
                        working_dir: None,
                        env: BTreeMap::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(String::from_utf8(output.stdout).unwrap(), "root-hook-clean");

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }

    #[test]
    fn up_detach_probes_env_with_remote_user_shell_when_docker_tests_are_enabled() {
        if !docker_tests_enabled() {
            eprintln!("skipped: set DECUNE_DOCKER_TESTS=1 to run Docker integration tests");
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-lifecycle-user-env-probe-login-shell");
            fs::create_dir_all(workspace.root().join(".devcontainer")).unwrap();
            fs::write(
                workspace.root().join(".devcontainer/Dockerfile"),
                r#"
                FROM alpine:3.20
                RUN printf '%s\n' \
                  '#!/bin/sh' \
                  'export DECUNE_LOGIN_SHELL_ENV=from-login-shell' \
                  'exec /bin/sh "$@"' \
                  >/usr/local/bin/decune-probe-shell \
                  && chmod +x /usr/local/bin/decune-probe-shell \
                  && adduser -D -s /usr/local/bin/decune-probe-shell decune
                "#,
            )
            .unwrap();
            write_devcontainer(
                &workspace,
                r#"
                {
                  "build": {
                    "dockerfile": "Dockerfile"
                  },
                  "remoteUser": "decune",
                  "userEnvProbe": "loginShell",
                  "postAttachCommand": [
                    "/bin/sh",
                    "-c",
                    "test \"$DECUNE_LOGIN_SHELL_ENV\" = from-login-shell && printf '%s' \"$DECUNE_LOGIN_SHELL_ENV\" >/tmp/decune-login-shell-env-probe"
                  ]
                }
                "#,
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let image = plan.image.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;

                run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                })
                .await?;

                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "cat /tmp/decune-login-shell-env-probe".to_owned(),
                        ],
                        user: None,
                        working_dir: None,
                        env: BTreeMap::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(String::from_utf8(output.stdout).unwrap(), "from-login-shell");

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }

    #[test]
    fn up_detach_removes_new_container_when_start_fails_if_docker_tests_are_enabled() {
        if !docker_tests_enabled() {
            eprintln!("skipped: set DECUNE_DOCKER_TESTS=1 to run Docker integration tests");
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-start-failure-cleanup");
            write_devcontainer(
                &workspace,
                r#"
                {
                  "image": "alpine:3.20",
                  "containerUser": "decune-missing-user"
                }
                "#,
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;

                let error = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                })
                .await
                .unwrap_err();
                assert!(
                    error
                        .to_string()
                        .contains("Failed to start Docker container")
                );

                let containers = list_workspace_containers(&client, workspace.id()).await?;
                assert!(
                    !containers
                        .iter()
                        .any(|container| container.name == container_name)
                );

                Ok(())
            }
            .await;

            let cleanup = remove_container(&client, &container_name, true, true).await;
            result.and(cleanup).unwrap();
        });
    }

    fn test_workspace(name: &str) -> Workspace {
        let root = temp_root(name);
        Workspace::resolve(&root).unwrap()
    }

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir()
            .join(format!("decune-up-test-{}", std::process::id()))
            .join(name);
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_devcontainer(workspace: &Workspace, contents: &str) {
        let path = workspace.root().join(".devcontainer/devcontainer.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn docker_tests_enabled() -> bool {
        std::env::var_os("DECUNE_DOCKER_TESTS").is_some_and(|value| value == "1")
    }
}
