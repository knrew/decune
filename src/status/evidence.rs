use std::{collections::BTreeMap, fs, io, path::Path};

use anyhow::{Context, Result};

use crate::{
    config::{ConfigLayer, resolved::ResolvedDevcontainerSource},
    devcontainer::json::discover as discover_devcontainer_json,
    docker::{
        container::ContainerInspect,
        resource::{
            config_hash_from_labels, managed_workspace_id_from_container,
            managed_workspace_id_from_labels, workspace_path_from_labels,
        },
    },
    runtime::docker_cli::{DockerCli, DockerVolumeInspect},
    state::{WorkspaceState, container_ids_match, load_state_file},
    up::{ForwardingResolution, build_read_only_up_plan_with_forwarding_resolution},
    workspace::{Workspace, is_valid_workspace_id},
};

use super::types::{
    ContainerStatusSummary, HealthStatus, RuntimeRunState, VolumeStatusSummary, WorkspaceMode,
};

pub(super) struct StateEvidence {
    pub(super) workspace_id: String,
    pub(super) state: Result<WorkspaceState, String>,
}

#[derive(Debug, Clone, Default)]
pub(super) struct DockerEvidence {
    pub(super) containers: Vec<ContainerEvidence>,
    pub(super) volumes: Vec<VolumeEvidence>,
}

#[derive(Debug, Clone)]
struct ComposeProjectContext {
    workspace_id: String,
    workspace_path: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) struct ContainerEvidence {
    pub(super) workspace_id: String,
    pub(super) id: Option<String>,
    pub(super) name: Option<String>,
    pub(super) service: Option<String>,
    pub(super) workspace_path: Option<String>,
    pub(super) config_hash: Option<String>,
    pub(super) run_state: ContainerRunState,
    pub(super) health_status: HealthStatus,
}

#[derive(Debug, Clone)]
pub(super) struct VolumeEvidence {
    pub(super) workspace_id: String,
    pub(super) name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ContainerRunState {
    Running,
    Stopped,
    Unknown,
}

#[derive(Debug, Clone, Default)]
pub(super) struct WorkspaceEvidence {
    pub(super) state: Option<Result<WorkspaceState, String>>,
    pub(super) containers: Vec<ContainerEvidence>,
    pub(super) volumes: Vec<VolumeEvidence>,
}

#[derive(Debug, Clone)]
pub(super) struct CurrentWorkspaceConfig {
    pub(super) mode: WorkspaceMode,
    pub(super) config_file: Option<String>,
    pub(super) config_hash: Option<String>,
    pub(super) error: Option<String>,
}

pub(super) fn current_workspace_config(workspace: &Workspace) -> Result<CurrentWorkspaceConfig> {
    let config_file = match discover_devcontainer_json(workspace.root(), None) {
        Ok(path) => Some(path.display().to_string()),
        Err(error) if is_missing_devcontainer_metadata_error(&error) => return Err(error),
        Err(error) => {
            return Ok(CurrentWorkspaceConfig {
                mode: WorkspaceMode::Unknown,
                config_file: None,
                config_hash: None,
                error: Some(format!("{error:#}")),
            });
        }
    };

    match build_read_only_up_plan_with_forwarding_resolution(
        workspace,
        None,
        ConfigLayer::default(),
        ForwardingResolution::IgnoreDetached,
        false,
        false,
    ) {
        Ok(plan) => Ok(CurrentWorkspaceConfig {
            mode: mode_from_source(plan.config.devcontainer.source.as_ref()),
            config_file,
            config_hash: Some(plan.resources.config_hash),
            error: None,
        }),
        Err(error) => Ok(CurrentWorkspaceConfig {
            mode: WorkspaceMode::Unknown,
            config_file,
            config_hash: None,
            error: Some(format!("{error:#}")),
        }),
    }
}

pub(super) async fn collect_workspace_docker_evidence(
    cli: &DockerCli,
    workspace_id: &str,
    state: Option<&WorkspaceState>,
) -> Result<DockerEvidence> {
    let context = ComposeProjectContext {
        workspace_id: workspace_id.to_owned(),
        workspace_path: state.map(|state| state.workspace.clone()),
    };
    let mut compose_projects = BTreeMap::<String, ComposeProjectContext>::new();
    add_compose_project_context(
        &mut compose_projects,
        state.and_then(|state| state.compose_project_name.as_deref()),
        &context,
    );

    let mut containers = Vec::new();
    for container in cli.list_workspace_container_inspects(workspace_id).await? {
        add_compose_project_context(
            &mut compose_projects,
            compose_project_name_from_container(&container).as_deref(),
            &context,
        );
        if let Some(evidence) = container_evidence(container) {
            containers.push(evidence);
        }
    }

    for (project_name, project_context) in compose_projects {
        let project_containers = cli
            .list_compose_project_container_inspects_by_project(&project_name)
            .await?;
        containers.extend(
            project_containers.into_iter().filter_map(|container| {
                container_evidence_with_context(container, &project_context)
            }),
        );
    }
    containers = dedupe_container_evidence(containers);

    let volumes = cli
        .list_volumes(workspace_id)
        .await?
        .into_iter()
        .map(|name| VolumeEvidence {
            workspace_id: workspace_id.to_owned(),
            name: Some(name),
        })
        .collect();

    Ok(DockerEvidence {
        containers,
        volumes,
    })
}

pub(super) fn load_status_states(root: &Path) -> Result<Vec<StateEvidence>> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to read decune state root: {}", root.display()));
        }
    };
    let mut states = Vec::new();

    for entry in entries {
        let entry = entry.with_context(|| {
            format!("Failed to read decune state root entry: {}", root.display())
        })?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(workspace_id) = path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
        else {
            continue;
        };
        if !is_valid_workspace_id(&workspace_id) {
            continue;
        }
        match load_state_file(&path) {
            Ok(Some(state)) => states.push(StateEvidence {
                workspace_id,
                state: Ok(state),
            }),
            Ok(None) => {}
            Err(error) => states.push(StateEvidence {
                workspace_id,
                state: Err(format!("{error:#}")),
            }),
        }
    }

    Ok(states)
}

pub(super) async fn collect_docker_evidence(
    cli: &DockerCli,
    states: &[StateEvidence],
) -> Result<DockerEvidence> {
    let mut compose_projects = BTreeMap::<String, ComposeProjectContext>::new();
    for state in states {
        if let Ok(state_value) = &state.state {
            let context = ComposeProjectContext {
                workspace_id: state.workspace_id.clone(),
                workspace_path: Some(state_value.workspace.clone()),
            };
            add_compose_project_context(
                &mut compose_projects,
                state_value.compose_project_name.as_deref(),
                &context,
            );
        }
    }

    let mut containers = Vec::new();
    for container in cli.list_all_managed_container_inspects().await? {
        let project_name = compose_project_name_from_container(&container);
        if let Some(evidence) = container_evidence(container) {
            let context = ComposeProjectContext {
                workspace_id: evidence.workspace_id.clone(),
                workspace_path: evidence.workspace_path.clone(),
            };
            add_compose_project_context(&mut compose_projects, project_name.as_deref(), &context);
            containers.push(evidence);
        }
    }

    for (project_name, project_context) in compose_projects {
        let project_containers = cli
            .list_compose_project_container_inspects_by_project(&project_name)
            .await?;
        containers.extend(
            project_containers.into_iter().filter_map(|container| {
                container_evidence_with_context(container, &project_context)
            }),
        );
    }
    let containers = dedupe_container_evidence(containers);
    let volumes = cli
        .list_all_managed_volume_inspects()
        .await?
        .into_iter()
        .filter_map(volume_evidence)
        .collect();

    Ok(DockerEvidence {
        containers,
        volumes,
    })
}

fn container_evidence(container: ContainerInspect) -> Option<ContainerEvidence> {
    let (workspace_id, labels) = managed_workspace_id_from_container(&container)?;
    Some(container_evidence_from_labels(
        &container,
        workspace_id,
        labels,
        None,
    ))
}

fn container_evidence_with_context(
    container: ContainerInspect,
    context: &ComposeProjectContext,
) -> Option<ContainerEvidence> {
    let labels = container.config.as_ref()?.labels.as_ref()?;
    let workspace_id =
        managed_workspace_id_from_labels(labels).unwrap_or_else(|| context.workspace_id.clone());
    Some(container_evidence_from_labels(
        &container,
        workspace_id,
        labels,
        context.workspace_path.as_deref(),
    ))
}

fn container_evidence_from_labels(
    container: &ContainerInspect,
    workspace_id: String,
    labels: &BTreeMap<String, String>,
    fallback_workspace_path: Option<&str>,
) -> ContainerEvidence {
    let workspace_path = workspace_path_from_labels(labels);
    let config_hash = config_hash_from_labels(labels);
    let service = compose_service_from_labels(labels);
    let run_state = container_run_state(&container.state);
    let health_status = container_health_status(&container.state);
    ContainerEvidence {
        workspace_id,
        id: container.id.clone(),
        name: container.name.clone(),
        service,
        workspace_path: workspace_path.or_else(|| fallback_workspace_path.map(str::to_owned)),
        config_hash,
        run_state,
        health_status,
    }
}

fn add_compose_project_context(
    projects: &mut BTreeMap<String, ComposeProjectContext>,
    project_name: Option<&str>,
    context: &ComposeProjectContext,
) {
    let Some(project_name) = project_name
        .map(str::trim)
        .filter(|project_name| !project_name.is_empty())
    else {
        return;
    };
    projects
        .entry(project_name.to_owned())
        .or_insert_with(|| context.clone());
}

fn compose_project_name_from_container(container: &ContainerInspect) -> Option<String> {
    container
        .config
        .as_ref()
        .and_then(|config| config.labels.as_ref())
        .and_then(|labels| labels.get("com.docker.compose.project"))
        .filter(|project_name| !project_name.trim().is_empty())
        .cloned()
}

fn dedupe_container_evidence(containers: Vec<ContainerEvidence>) -> Vec<ContainerEvidence> {
    let mut positions = BTreeMap::<String, usize>::new();
    let mut deduped = Vec::<ContainerEvidence>::new();

    for container in containers {
        let id = container
            .id
            .as_deref()
            .filter(|id| !id.trim().is_empty())
            .map(str::to_owned);
        let Some(id) = id else {
            deduped.push(container);
            continue;
        };

        if let Some(index) = positions.get(&id).copied() {
            if deduped[index].config_hash.is_none() && container.config_hash.is_some() {
                deduped[index] = container;
            }
        } else {
            positions.insert(id, deduped.len());
            deduped.push(container);
        }
    }

    deduped
}

fn volume_evidence(volume: DockerVolumeInspect) -> Option<VolumeEvidence> {
    let labels = volume.labels.as_ref()?;
    let workspace_id = managed_workspace_id_from_labels(labels)?;
    Some(VolumeEvidence {
        workspace_id,
        name: volume.name,
    })
}

impl From<&ContainerEvidence> for ContainerStatusSummary {
    fn from(value: &ContainerEvidence) -> Self {
        Self {
            id: value.id.clone(),
            name: value.name.clone(),
            service: value.service.clone(),
            run_state: value.run_state.into(),
            health_status: value.health_status,
        }
    }
}

impl From<ContainerRunState> for RuntimeRunState {
    fn from(value: ContainerRunState) -> Self {
        match value {
            ContainerRunState::Running => Self::Running,
            ContainerRunState::Stopped => Self::Stopped,
            ContainerRunState::Unknown => Self::Unknown,
        }
    }
}

impl From<&VolumeEvidence> for VolumeStatusSummary {
    fn from(value: &VolumeEvidence) -> Self {
        Self {
            name: value.name.clone(),
        }
    }
}

fn container_run_state(
    state: &Option<crate::docker::container::ContainerState>,
) -> ContainerRunState {
    let Some(state) = state else {
        return ContainerRunState::Unknown;
    };
    if state.running == Some(true) {
        return ContainerRunState::Running;
    }
    if state.running == Some(false) {
        return ContainerRunState::Stopped;
    }
    match state.status.as_deref() {
        Some("running") => ContainerRunState::Running,
        Some("created" | "exited" | "dead" | "paused" | "restarting" | "removing") => {
            ContainerRunState::Stopped
        }
        _ => ContainerRunState::Unknown,
    }
}

fn container_health_status(
    state: &Option<crate::docker::container::ContainerState>,
) -> HealthStatus {
    match state
        .as_ref()
        .and_then(|state| state.health.as_ref())
        .and_then(|health| health.status.as_deref())
    {
        Some("healthy") => HealthStatus::Healthy,
        Some("unhealthy") => HealthStatus::Unhealthy,
        Some("starting") => HealthStatus::Starting,
        Some(_) => HealthStatus::Unknown,
        None => HealthStatus::None,
    }
}

pub(super) fn state_container_is_present(
    state: &WorkspaceState,
    containers: &[ContainerEvidence],
) -> bool {
    containers.iter().any(|container| {
        container
            .id
            .as_deref()
            .is_some_and(|id| container_ids_match(id, &state.container_id))
    })
}

pub(super) const fn has_docker_evidence(evidence: &WorkspaceEvidence) -> bool {
    !evidence.containers.is_empty() || !evidence.volumes.is_empty()
}

const fn mode_from_source(source: Option<&ResolvedDevcontainerSource>) -> WorkspaceMode {
    match source {
        Some(ResolvedDevcontainerSource::Image(_)) => WorkspaceMode::Image,
        Some(ResolvedDevcontainerSource::Dockerfile(_)) => WorkspaceMode::Dockerfile,
        Some(ResolvedDevcontainerSource::Compose(_)) => WorkspaceMode::Compose,
        None => WorkspaceMode::Unknown,
    }
}

fn compose_service_from_labels(labels: &BTreeMap<String, String>) -> Option<String> {
    labels
        .get("com.docker.compose.project")
        .filter(|project| !project.trim().is_empty())?;
    labels
        .get("com.docker.compose.service")
        .filter(|service| !service.trim().is_empty())
        .cloned()
}

fn is_missing_devcontainer_metadata_error(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}");
    message.contains("Devcontainer metadata file was not found")
        || message.contains("Multiple devcontainer metadata files found")
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf, sync::Arc};

    use crate::{
        runtime::{
            command::{FakeRuntimeCommand, RuntimeOutput},
            docker_cli::DockerCli,
        },
        state::{LifecycleState, WorkspaceState},
        status::{
            inventory::{build_status_inventory, workspace_status_with_config},
            render::compose_services,
            types::{EnvironmentStatus, HealthStatus, WorkspaceMode, WorkspaceStatus},
        },
    };

    use super::*;

    const WORKSPACE_ID: &str = "123456abcdef";
    #[test]
    fn invalid_state_directory_ids_are_ignored() {
        let root = temp_root("invalid-state");
        fs::create_dir_all(root.join("../invalid")).unwrap();
        let invalid = root.join("not-a-valid-id");
        fs::create_dir_all(&invalid).unwrap();
        fs::write(invalid.join("state.toml"), "not toml").unwrap();

        let states = load_status_states(&root).unwrap();

        assert!(states.is_empty());
    }
    #[test]
    fn invalid_docker_label_ids_are_ignored() {
        let raw_containers: Vec<ContainerInspect> = serde_json::from_slice(
            br#"[{
                "Id": "container-id",
                "Config": {
                    "Labels": {
                        "decune.managed": "true",
                        "decune.workspace_id": "../victim",
                        "decune.workspace": "/workspace"
                    }
                },
                "State": { "Running": true }
            }]"#,
        )
        .unwrap();
        let raw_volumes: Vec<DockerVolumeInspect> = serde_json::from_slice(
            br#"[{
                "Name": "volume-name",
                "Labels": {
                    "decune.managed": "true",
                    "decune.workspace_id": "bad/id"
                }
            }]"#,
        )
        .unwrap();
        let inventory = build_status_inventory(
            Vec::new(),
            Ok(DockerEvidence {
                containers: raw_containers
                    .into_iter()
                    .filter_map(container_evidence)
                    .collect(),
                volumes: raw_volumes
                    .into_iter()
                    .filter_map(volume_evidence)
                    .collect(),
            }),
        );

        assert!(inventory.workspaces.is_empty());
    }
    #[test]
    fn container_and_volume_inspect_are_reduced_to_valid_evidence() {
        let container: Vec<ContainerInspect> = serde_json::from_slice(
            br#"[{
                "Id": "container-id",
                "Config": {
                    "Labels": {
                        "decune.managed": "true",
                        "decune.workspace_id": "123456abcdef",
                        "decune.workspace": "/workspace",
                        "decune.config_hash": "hash"
                    }
                },
                "State": {
                    "Status": "running",
                    "Health": { "Status": "healthy" }
                }
            }]"#,
        )
        .unwrap();
        let evidence = container_evidence(container.into_iter().next().unwrap()).unwrap();

        assert_eq!(evidence.workspace_id, WORKSPACE_ID);
        assert_eq!(evidence.workspace_path.as_deref(), Some("/workspace"));
        assert_eq!(evidence.run_state, ContainerRunState::Running);
        assert_eq!(evidence.health_status, HealthStatus::Healthy);
    }
    #[test]
    fn workspace_docker_evidence_includes_compose_sidecar_from_state_project() {
        let runner = FakeRuntimeCommand::new(vec![
            Ok(output(b"")),
            Ok(output(
                br#"[{
                    "Id": "primary-id",
                    "Name": "/project-app-1",
                    "Config": {
                        "Labels": {
                            "decune.managed": "true",
                            "decune.workspace_id": "123456abcdef",
                            "decune.workspace": "/workspace",
                            "decune.config_hash": "hash",
                            "com.docker.compose.project": "project",
                            "com.docker.compose.service": "app"
                        }
                    },
                    "State": {
                        "Running": true,
                        "Health": { "Status": "healthy" }
                    }
                },{
                    "Id": "sidecar-id",
                    "Name": "/project-db-1",
                    "Config": {
                        "Labels": {
                            "com.docker.compose.project": "project",
                            "com.docker.compose.service": "db"
                        }
                    },
                    "State": {
                        "Running": false,
                        "Health": { "Status": "unhealthy" }
                    }
                }]"#,
            )),
            Ok(output(
                br#"{"ID":"primary-id"}
{"ID":"sidecar-id"}
"#,
            )),
            Ok(output(
                br#"[{
                    "Id": "primary-id",
                    "Name": "/project-app-1",
                    "Config": {
                        "Labels": {
                            "decune.managed": "true",
                            "decune.workspace_id": "123456abcdef",
                            "decune.workspace": "/workspace",
                            "decune.config_hash": "hash",
                            "com.docker.compose.project": "project",
                            "com.docker.compose.service": "app"
                        }
                    },
                    "State": {
                        "Running": true,
                        "Health": { "Status": "healthy" }
                    }
                }]"#,
            )),
            Ok(output(br#"{"ID":"primary-id"}"#)),
        ]);
        let cli = DockerCli::new(Arc::new(runner.clone()));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let state = WorkspaceState {
            compose_project_name: Some("project".to_owned()),
            ..state("primary-id", "hash")
        };

        let evidence = runtime
            .block_on(collect_workspace_docker_evidence(
                &cli,
                WORKSPACE_ID,
                Some(&state),
            ))
            .unwrap();

        assert_eq!(evidence.containers.len(), 2);
        let sidecar = evidence
            .containers
            .iter()
            .find(|container| container.id.as_deref() == Some("sidecar-id"))
            .unwrap();
        assert_eq!(sidecar.workspace_id, WORKSPACE_ID);
        assert_eq!(sidecar.workspace_path.as_deref(), Some("/workspace"));
        assert_eq!(sidecar.service.as_deref(), Some("db"));
        assert_eq!(sidecar.run_state, ContainerRunState::Stopped);
        assert_eq!(sidecar.health_status, HealthStatus::Unhealthy);

        let status = workspace_status_with_config(
            WORKSPACE_ID.to_owned(),
            WorkspaceEvidence {
                state: Some(Ok(state)),
                containers: evidence.containers,
                volumes: evidence.volumes,
            },
            false,
            Some(CurrentWorkspaceConfig {
                mode: WorkspaceMode::Compose,
                config_file: Some("/workspace/.devcontainer/devcontainer.json".to_owned()),
                config_hash: Some("hash".to_owned()),
                error: None,
            }),
        );
        assert_eq!(status.environment_status, EnvironmentStatus::Partial);
        assert_eq!(status.health_status, HealthStatus::Mixed);
        assert_issue(&status, "partial-environment");
        assert_issue(&status, "unhealthy-container");

        let services = compose_services(&status);
        assert_eq!(services, vec!["app".to_owned(), "db".to_owned()]);
        let commands = runner.commands();
        assert!(commands.iter().any(|command| {
            command
                .args_vec()
                .contains(&"label=com.docker.compose.project=project".to_owned())
        }));
    }
    #[test]
    fn all_docker_evidence_includes_compose_sidecar_from_state_project() {
        let runner = FakeRuntimeCommand::new(vec![
            Ok(output(b"")),
            Ok(output(
                br#"[{
                    "Id": "primary-id",
                    "Name": "/project-app-1",
                    "Config": {
                        "Labels": {
                            "decune.managed": "true",
                            "decune.workspace_id": "123456abcdef",
                            "decune.workspace": "/workspace",
                            "decune.config_hash": "hash",
                            "com.docker.compose.project": "project",
                            "com.docker.compose.service": "app"
                        }
                    },
                    "State": { "Running": true }
                },{
                    "Id": "sidecar-id",
                    "Name": "/project-db-1",
                    "Config": {
                        "Labels": {
                            "com.docker.compose.project": "project",
                            "com.docker.compose.service": "db"
                        }
                    },
                    "State": { "Running": false }
                }]"#,
            )),
            Ok(output(
                br#"{"ID":"primary-id"}
{"ID":"sidecar-id"}
"#,
            )),
            Ok(output(
                br#"[{
                    "Id": "primary-id",
                    "Name": "/project-app-1",
                    "Config": {
                        "Labels": {
                            "decune.managed": "true",
                            "decune.workspace_id": "123456abcdef",
                            "decune.workspace": "/workspace",
                            "decune.config_hash": "hash",
                            "com.docker.compose.project": "project",
                            "com.docker.compose.service": "app"
                        }
                    },
                    "State": { "Running": true }
                }]"#,
            )),
            Ok(output(br#"{"ID":"primary-id"}"#)),
        ]);
        let cli = DockerCli::new(Arc::new(runner));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let state = WorkspaceState {
            compose_project_name: Some("project".to_owned()),
            ..state("primary-id", "hash")
        };
        let states = vec![state_evidence(WORKSPACE_ID, state)];

        let evidence = runtime
            .block_on(collect_docker_evidence(&cli, &states))
            .unwrap();

        assert_eq!(evidence.containers.len(), 2);
        assert!(evidence.containers.iter().any(|container| {
            container.id.as_deref() == Some("sidecar-id")
                && container.service.as_deref() == Some("db")
                && container.workspace_id == WORKSPACE_ID
        }));
    }
    #[test]
    fn docker_evidence_collection_uses_read_only_commands() {
        let runner = FakeRuntimeCommand::new(vec![
            Ok(output(
                br#"[{
                    "Name": "volume-name",
                    "Labels": {
                        "decune.managed": "true",
                        "decune.workspace_id": "123456abcdef"
                    }
                }]"#,
            )),
            Ok(output(b"volume-name\n")),
            Ok(output(
                br#"[{
                    "Id": "container-id",
                    "Config": {
                        "Labels": {
                            "decune.managed": "true",
                            "decune.workspace_id": "123456abcdef"
                        }
                    },
                    "State": { "Running": true }
                }]"#,
            )),
            Ok(output(br#"{"ID":"container-id"}"#)),
        ]);
        let cli = DockerCli::new(Arc::new(runner.clone()));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let evidence = runtime
            .block_on(collect_docker_evidence(&cli, &[]))
            .unwrap();

        assert_eq!(evidence.containers.len(), 1);
        assert_eq!(evidence.volumes.len(), 1);
        let commands = runner.commands();
        let args = commands
            .iter()
            .map(|command| command.args_vec().to_vec())
            .collect::<Vec<_>>();
        assert_eq!(
            args,
            vec![
                vec![
                    "ps",
                    "--all",
                    "--filter",
                    "label=decune.managed=true",
                    "--format",
                    "json",
                ],
                vec!["container", "inspect", "container-id"],
                vec![
                    "volume",
                    "ls",
                    "--filter",
                    "label=decune.managed=true",
                    "--format",
                    "{{.Name}}",
                ],
                vec!["volume", "inspect", "volume-name"],
            ]
        );
        for command in commands {
            let args = command.args_vec();
            assert!(
                matches!(
                    args,
                    [first, second, ..] if first == "container" && second == "inspect"
                ) || matches!(
                    args,
                    [first, second, ..] if first == "volume" && (second == "ls" || second == "inspect")
                ) || matches!(args, [first, ..] if first == "ps"),
                "{args:?}"
            );
        }
    }
    fn assert_issue(workspace: &WorkspaceStatus, code: &str) {
        assert!(
            workspace.issues.iter().any(|issue| issue.code == code),
            "{:?}",
            workspace.issues
        );
    }
    fn state_evidence(workspace_id: &str, state: WorkspaceState) -> StateEvidence {
        StateEvidence {
            workspace_id: workspace_id.to_owned(),
            state: Ok(state),
        }
    }
    fn state(container_id: &str, config_hash: &str) -> WorkspaceState {
        WorkspaceState {
            version: 1,
            workspace: "/workspace".to_owned(),
            container_id: container_id.to_owned(),
            image: "image".to_owned(),
            config_hash: config_hash.to_owned(),
            config_file: None,
            compose_project_name: None,
            published_ports: Vec::new(),
            created_at: "unix:1".to_owned(),
            last_started_at: "unix:2".to_owned(),
            last_used_at: None,
            lifecycle: LifecycleState::default(),
        }
    }
    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir()
            .join("decune-status-tests")
            .join(std::process::id().to_string())
            .join(name);
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }
    fn output(stdout: &[u8]) -> RuntimeOutput {
        RuntimeOutput {
            stdout: stdout.to_vec(),
            stderr: Vec::new(),
            exit_code: 0,
        }
    }
}
