use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::Path,
};

use anyhow::{Context, Result};

use crate::{
    config::{ConfigLayer, resolved::ResolvedDevcontainerSource},
    devcontainer::json::discover as discover_devcontainer_json,
    docker::{
        container::ContainerInspect,
        resource::{
            compose_project_name_from_labels, compose_service_name_from_labels,
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
    ContainerStatusSummary, HealthStatus, RuntimeRunState, VolumeOrigin, VolumeStatusSummary,
    WorkspaceMode,
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
    /// volume が属するワークスペースのパス。
    /// decune のラベルを持つ volume では、その `decune.workspace` ラベルのパス、
    /// Compose プロジェクトの volume では、プロジェクトを辿った状態かコンテナのラベルのパスである。
    /// 状態もコンテナも残っていないワークスペースのパスを、summary に示すのに使う。
    /// ワークスペースを指定した収集(`collect_workspace_docker_evidence`)では、
    /// decune のラベルを持つ volume には持たせない。
    /// 表示するパスは、指定したワークスペースのものを使うからである。
    pub(super) workspace_path: Option<String>,
    pub(super) origin: VolumeOrigin,
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
        if let Some(evidence) = container_evidence(&container) {
            containers.push(evidence);
        }
    }

    for (project_name, project_context) in &compose_projects {
        let project_containers = cli
            .list_compose_project_container_inspects_by_project(project_name)
            .await?;
        containers.extend(
            project_containers.into_iter().filter_map(|container| {
                container_evidence_with_context(&container, project_context)
            }),
        );
    }
    containers = dedupe_container_evidence(containers);

    let mut volumes = cli
        .list_volumes(workspace_id)
        .await?
        .into_iter()
        .map(|name| VolumeEvidence {
            workspace_id: workspace_id.to_owned(),
            name: Some(name),
            workspace_path: None,
            origin: VolumeOrigin::Mounts,
        })
        .collect::<Vec<_>>();
    for (project_name, project_context) in &compose_projects {
        volumes.extend(compose_project_volume_evidence(cli, project_name, project_context).await?);
    }
    let volumes = dedupe_volume_evidence(volumes);

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
        if let Some(evidence) = container_evidence(&container) {
            let context = ComposeProjectContext {
                workspace_id: evidence.workspace_id.clone(),
                workspace_path: evidence.workspace_path.clone(),
            };
            add_compose_project_context(&mut compose_projects, project_name.as_deref(), &context);
            containers.push(evidence);
        }
    }

    for (project_name, project_context) in &compose_projects {
        let project_containers = cli
            .list_compose_project_container_inspects_by_project(project_name)
            .await?;
        containers.extend(
            project_containers.into_iter().filter_map(|container| {
                container_evidence_with_context(&container, project_context)
            }),
        );
    }
    let containers = dedupe_container_evidence(containers);
    let mut volumes = cli
        .list_all_managed_volume_inspects()
        .await?
        .into_iter()
        .filter_map(volume_evidence)
        .collect::<Vec<_>>();
    for (project_name, project_context) in &compose_projects {
        volumes.extend(compose_project_volume_evidence(cli, project_name, project_context).await?);
    }
    let volumes = dedupe_volume_evidence(volumes);

    Ok(DockerEvidence {
        containers,
        volumes,
    })
}

fn container_evidence(container: &ContainerInspect) -> Option<ContainerEvidence> {
    let (workspace_id, labels) = managed_workspace_id_from_container(container)?;
    Some(container_evidence_from_labels(
        container,
        workspace_id,
        labels,
        None,
    ))
}

fn container_evidence_with_context(
    container: &ContainerInspect,
    context: &ComposeProjectContext,
) -> Option<ContainerEvidence> {
    let labels = container.config.as_ref()?.labels.as_ref()?;
    let workspace_id =
        managed_workspace_id_from_labels(labels).unwrap_or_else(|| context.workspace_id.clone());
    Some(container_evidence_from_labels(
        container,
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
    let service = compose_service_name_from_labels(labels);
    let run_state = container_run_state(container.state.as_ref());
    let health_status = container_health_status(container.state.as_ref());
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
        .and_then(compose_project_name_from_labels)
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

/// Compose プロジェクトのラベルを持つ volume を、
/// そのプロジェクトのワークスペースの decune-managed ボリュームとして返す。
/// Compose はサービスの匿名 volume と、
/// 自分で作っていない `external` の volume にプロジェクトのラベルを付けないので、
/// これらは含まれない。
async fn compose_project_volume_evidence(
    cli: &DockerCli,
    project_name: &str,
    context: &ComposeProjectContext,
) -> Result<Vec<VolumeEvidence>> {
    Ok(cli
        .list_compose_project_volumes(project_name)
        .await?
        .into_iter()
        .map(|name| VolumeEvidence {
            workspace_id: context.workspace_id.clone(),
            name: Some(name),
            workspace_path: context.workspace_path.clone(),
            origin: VolumeOrigin::Compose,
        })
        .collect())
}

/// 同じ名前の volume を一つにする。
/// decune のラベルと Compose プロジェクトのラベルの両方を持つ volume は、
/// 先に見つけた方の出どころで示す。
/// 呼び出し側は decune のラベルを持つ volume を先に渡すので、その volume は `mounts` になり、
/// コンテナの中の `status`(`SystemContainerQuerySource::collect_volumes`)と揃う。
fn dedupe_volume_evidence(volumes: Vec<VolumeEvidence>) -> Vec<VolumeEvidence> {
    let mut seen = BTreeSet::new();
    volumes
        .into_iter()
        .filter(|volume| {
            volume
                .name
                .as_ref()
                .is_none_or(|name| seen.insert(name.clone()))
        })
        .collect()
}

fn volume_evidence(volume: DockerVolumeInspect) -> Option<VolumeEvidence> {
    let labels = volume.labels.as_ref()?;
    let workspace_id = managed_workspace_id_from_labels(labels)?;
    let workspace_path = workspace_path_from_labels(labels);
    Some(VolumeEvidence {
        workspace_id,
        name: volume.name,
        workspace_path,
        origin: VolumeOrigin::Mounts,
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
            origin: value.origin,
        }
    }
}

fn container_run_state(
    state: Option<&crate::docker::container::ContainerState>,
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
    state: Option<&crate::docker::container::ContainerState>,
) -> HealthStatus {
    match state
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
            fake_docker::{ANONYMOUS_VOLUME_LABEL, FakeDocker},
        },
        state::{LifecycleState, WorkspaceModeSnapshot, WorkspaceState},
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
        let (_temp, root) = temp_root("invalid-state");
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
                    .filter_map(|container| container_evidence(&container))
                    .collect(),
                volumes: raw_volumes
                    .into_iter()
                    .filter_map(volume_evidence)
                    .collect(),
            }),
        );

        assert_eq!(inventory.workspaces, Vec::new());
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
        let evidence = container_evidence(&container.into_iter().next().unwrap()).unwrap();

        assert_eq!(evidence.workspace_id, WORKSPACE_ID);
        assert_eq!(evidence.workspace_path.as_deref(), Some("/workspace"));
        assert_eq!(evidence.run_state, ContainerRunState::Running);
        assert_eq!(evidence.health_status, HealthStatus::Healthy);
    }

    // 状態もコンテナも無いときも、volume のラベルからワークスペースのパスを収集する
    #[test]
    fn docker_evidence_collection_retains_volume_workspace_path_without_state_or_containers() {
        let runner = FakeRuntimeCommand::new(vec![
            Ok(output(
                br#"[{
                    "Name": "project-data",
                    "Labels": {
                        "decune.managed": "true",
                        "decune.workspace_id": "123456abcdef",
                        "decune.workspace": "/workspace"
                    }
                }]"#,
            )),
            Ok(output(b"project-data\n")),
            Ok(output(b"")),
        ]);
        let cli = DockerCli::new(Arc::new(runner));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let evidence = runtime
            .block_on(collect_docker_evidence(&cli, &[]))
            .unwrap();

        assert!(evidence.containers.is_empty());
        assert_eq!(evidence.volumes.len(), 1);
        let volume = &evidence.volumes[0];
        assert_eq!(volume.workspace_id, WORKSPACE_ID);
        assert_eq!(volume.name.as_deref(), Some("project-data"));
        assert_eq!(volume.workspace_path.as_deref(), Some("/workspace"));
    }

    /// decune-managed ボリュームの `(名前, 出どころ)` を名前の順に並べる。
    fn volume_origins(evidence: &DockerEvidence) -> Vec<(String, VolumeOrigin)> {
        let mut volumes = evidence
            .volumes
            .iter()
            .map(|volume| (volume.name.clone().unwrap(), volume.origin))
            .collect::<Vec<_>>();
        volumes.sort_by(|left, right| left.0.cmp(&right.0));
        volumes
    }

    /// Compose プロジェクト `project` のワークスペースが見る volume。
    /// プロジェクトの volume と decune のラベルを持つ volume のほかに、
    /// ラベルの無い volume、Compose が作っていない `external` の volume、
    /// サービスの匿名 volume を持つ。
    fn docker_with_workspace_volumes() -> FakeDocker {
        let docker = FakeDocker::new();
        docker.add_volume("project_data", &[("com.docker.compose.project", "project")]);
        docker.add_volume(
            "cache",
            &[
                ("decune.managed", "true"),
                ("decune.workspace_id", WORKSPACE_ID),
            ],
        );
        docker.add_volume("unlabeled", &[]);
        docker.add_volume("external", &[("com.docker.compose.volume", "external")]);
        docker.add_volume("anonymous", &[(ANONYMOUS_VOLUME_LABEL, "")]);
        docker.add_container(
            "primary-id",
            &[
                ("decune.managed", "true"),
                ("decune.workspace_id", WORKSPACE_ID),
                ("com.docker.compose.project", "project"),
            ],
            &[
                "project_data",
                "cache",
                "unlabeled",
                "external",
                "anonymous",
            ],
        );
        docker
    }

    // ワークスペースの decune-managed ボリュームは、Compose プロジェクトの volume を `compose`、
    // decune のラベルを持つ volume を `mounts` として含み、
    // ラベルの無い volume、プロジェクトのラベルを持たない `external` の volume、
    // 匿名 volume を含まない。
    // Compose プロジェクトは、ワークスペースのコンテナのラベルから辿る
    #[test]
    fn workspace_volumes_are_project_and_labeled_volumes_with_origin() {
        let docker = docker_with_workspace_volumes();
        let cli = DockerCli::new(Arc::new(docker));

        let evidence =
            block_on(collect_workspace_docker_evidence(&cli, WORKSPACE_ID, None)).unwrap();

        assert_eq!(
            volume_origins(&evidence),
            vec![
                ("cache".to_owned(), VolumeOrigin::Mounts),
                ("project_data".to_owned(), VolumeOrigin::Compose),
            ]
        );
    }

    // コンテナが残っていなくても、状態に記録した Compose プロジェクトの volume を数える
    #[test]
    fn workspace_volumes_include_project_volumes_found_from_state_only() {
        let docker = FakeDocker::new();
        docker.add_volume("project_data", &[("com.docker.compose.project", "project")]);
        let cli = DockerCli::new(Arc::new(docker));
        let state = WorkspaceState {
            compose_project_name: Some("project".to_owned()),
            ..state("primary-id", "hash")
        };

        let evidence = block_on(collect_workspace_docker_evidence(
            &cli,
            WORKSPACE_ID,
            Some(&state),
        ))
        .unwrap();

        assert_eq!(
            volume_origins(&evidence),
            vec![("project_data".to_owned(), VolumeOrigin::Compose)]
        );
    }

    // `WORKSPACE` なしの `status` も、状態に記録した Compose プロジェクトの volume を
    // そのワークスペースの decune-managed ボリュームとして数え、Docker のリソースが無いとはしない
    #[test]
    fn all_docker_evidence_counts_project_volumes_of_state_project() {
        let docker = FakeDocker::new();
        docker.add_volume("project_data", &[("com.docker.compose.project", "project")]);
        let cli = DockerCli::new(Arc::new(docker));
        let state = WorkspaceState {
            compose_project_name: Some("project".to_owned()),
            ..state("primary-id", "hash")
        };
        let states = vec![state_evidence(WORKSPACE_ID, state)];

        let evidence = block_on(collect_docker_evidence(&cli, &states)).unwrap();
        let inventory = build_status_inventory(states, Ok(evidence));

        let [workspace] = inventory.workspaces.as_slice() else {
            panic!("{:?}", inventory.workspaces);
        };
        assert_eq!(
            workspace.volumes,
            vec![VolumeStatusSummary {
                name: Some("project_data".to_owned()),
                origin: VolumeOrigin::Compose,
            }]
        );
        assert!(
            workspace
                .issues
                .iter()
                .all(|issue| issue.code != "state-only"),
            "{:?}",
            workspace.issues
        );
    }

    fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    fn compose_sidecar_runtime() -> FakeRuntimeCommand {
        FakeRuntimeCommand::new(vec![
            Ok(output(b"")),
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
        ])
    }

    fn assert_compose_sidecar_evidence(evidence: &DockerEvidence) {
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
    }

    fn assert_compose_sidecar_status(state: WorkspaceState, evidence: DockerEvidence) {
        let status = workspace_status_with_config(
            WORKSPACE_ID.to_owned(),
            &WorkspaceEvidence {
                state: Some(Ok(state)),
                containers: evidence.containers,
                volumes: evidence.volumes,
            },
            false,
            Some(&CurrentWorkspaceConfig {
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
        assert_eq!(
            compose_services(&status),
            vec!["app".to_owned(), "db".to_owned()]
        );
    }

    fn assert_compose_project_label_filter_used(runner: &FakeRuntimeCommand) {
        let commands = runner.commands();

        assert!(commands.iter().any(|command| {
            command
                .args_vec()
                .contains(&"label=com.docker.compose.project=project".to_owned())
        }));
    }

    #[test]
    fn workspace_docker_evidence_includes_compose_sidecar_from_state_project() {
        let runner = compose_sidecar_runtime();
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

        assert_compose_sidecar_evidence(&evidence);
        assert_compose_sidecar_status(state, evidence);
        assert_compose_project_label_filter_used(&runner);
    }
    #[test]
    fn all_docker_evidence_includes_compose_sidecar_from_state_project() {
        let runner = FakeRuntimeCommand::new(vec![
            Ok(output(b"")),
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
            Ok(output(b"container-id\n")),
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
                    "{{.ID}}",
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
            mode: WorkspaceModeSnapshot::Unknown,
            container_id: container_id.to_owned(),
            image: "image".to_owned(),
            config_hash: config_hash.to_owned(),
            config_file: None,
            compose_project_name: None,
            published_ports: Vec::new(),
            clone_isolation: crate::state::CloneIsolationRuntimeState::default(),
            created_at: "unix:1".to_owned(),
            last_started_at: "unix:2".to_owned(),
            last_used_at: None,
            lifecycle: LifecycleState::default(),
            exec_context: None,
        }
    }
    fn temp_root(name: &str) -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::Builder::new()
            .prefix("decune-status-evidence-tests-")
            .tempdir()
            .unwrap();
        let root = temp.path().join(name);
        fs::create_dir_all(&root).unwrap();
        (temp, root)
    }
    fn output(stdout: &[u8]) -> RuntimeOutput {
        RuntimeOutput {
            stdout: stdout.to_vec(),
            stderr: Vec::new(),
            exit_code: 0,
        }
    }
}
