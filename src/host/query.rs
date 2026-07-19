use std::{
    collections::{BTreeSet, HashMap},
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use decune_container_protocol::{
    ERROR_CODE_CLI_QUERY_BUSY, ERROR_CODE_CLI_QUERY_FAILED, ERROR_CODE_CLI_QUERY_TIMEOUT,
    HostDaemonResponse,
};
use serde::Serialize;
use tokio::sync::{Semaphore, watch};

use crate::{
    docker::{
        container::ContainerInspect,
        resource::{compose_project_name_from_labels, managed_workspace_id_from_container},
    },
    host::{
        forward::{ForwardStatusList, list_active_forward_status_ports},
        query_context::{ContainerCliQueryContext, HostDaemonCliQueryPolicy},
    },
    ports::{
        container_forwarded_port_snapshots, container_published_port_snapshots,
        render_container_ports_json, render_container_ports_text,
    },
    runtime::{
        command::{QueryRuntimeCommandRunner, RuntimeCommandRunner, TokioRuntimeCommand},
        docker_cli::DockerCli,
    },
    state::{PublishedPortRuntimeState, WorkspaceState, load_state_file},
    status::container::{
        ContainerQueryContainersEvidence, ContainerQueryDockerEvidence,
        ContainerQueryRuntimeSnapshot, ContainerQuerySnapshot, ContainerQueryStateEvidence,
        ContainerQueryStateSnapshot, ContainerQueryVolumeEvidence,
        build_container_workspace_status, container_query_evidence_from_inspect,
        container_query_inspect_matches_scope, render_container_workspace_status,
    },
};

const ACTIVE_CONTAINER_CLI_QUERIES: usize = 8;
const CONTAINER_CLI_QUERY_TIMEOUT: Duration = Duration::from_secs(15);
const CONCURRENT_DOCKER_EVIDENCE_LOADS: usize = 2;
const DOCKER_EVIDENCE_LOAD_TIMEOUT: Duration = Duration::from_secs(10);
const QUERY_DOCKER_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const SUCCESS_CACHE_TTL: Duration = Duration::from_secs(2);
const FAILURE_CACHE_TTL: Duration = Duration::from_millis(500);

type QueryFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
type QueryEvidenceResult = std::result::Result<QueryEvidence, QueryEvidenceFailure>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ContainerCliQuery {
    StatusText,
    PortsText,
    PortsJson,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
enum QueryEvidenceKind {
    Containers,
    Volumes,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
struct QueryEvidenceKey {
    query_context_fingerprint: String,
    workspace_id: String,
    kind: QueryEvidenceKind,
}

impl QueryEvidenceKey {
    fn from_context(context: &ContainerCliQueryContext, kind: QueryEvidenceKind) -> Self {
        Self {
            query_context_fingerprint: context.context_fingerprint().to_owned(),
            workspace_id: context.workspace_id().to_owned(),
            kind,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "kind", content = "evidence")]
enum QueryEvidence {
    Containers(ContainerQueryContainersEvidence),
    Volumes(Vec<ContainerQueryVolumeEvidence>),
}

impl QueryEvidence {
    const fn matches_kind(&self, kind: QueryEvidenceKind) -> bool {
        matches!(
            (self, kind),
            (Self::Containers(_), QueryEvidenceKind::Containers)
                | (Self::Volumes(_), QueryEvidenceKind::Volumes)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum QueryEvidenceFailure {
    Unavailable,
    TimedOut,
}

#[derive(Clone)]
struct DockerContainerLoadHint {
    compose_project_name: Option<String>,
    published_ports: Vec<PublishedPortRuntimeState>,
}

impl DockerContainerLoadHint {
    fn from_state(state: Option<&WorkspaceState>) -> Self {
        Self {
            compose_project_name: state.and_then(|state| state.compose_project_name.clone()),
            published_ports: state.map_or_else(Vec::new, |state| state.published_ports.clone()),
        }
    }
}

enum QueryEvidenceLoad {
    Containers(DockerContainerLoadHint),
    Volumes,
}

impl QueryEvidenceLoad {
    const fn kind(&self) -> QueryEvidenceKind {
        match self {
            Self::Containers(_) => QueryEvidenceKind::Containers,
            Self::Volumes => QueryEvidenceKind::Volumes,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct LocalEvidenceFailure;

trait ContainerQuerySource: Send + Sync {
    fn load_state<'a>(
        &'a self,
        context: &'a ContainerCliQueryContext,
    ) -> QueryFuture<'a, std::result::Result<Option<WorkspaceState>, LocalEvidenceFailure>>;

    fn load_forwarding<'a>(
        &'a self,
        context: &'a ContainerCliQueryContext,
    ) -> QueryFuture<'a, std::result::Result<ForwardStatusList, LocalEvidenceFailure>>;

    fn load_containers<'a>(
        &'a self,
        workspace_id: &'a str,
        hint: DockerContainerLoadHint,
    ) -> QueryFuture<'a, std::result::Result<ContainerQueryContainersEvidence, QueryEvidenceFailure>>;

    fn load_volumes<'a>(
        &'a self,
        workspace_id: &'a str,
    ) -> QueryFuture<'a, std::result::Result<Vec<ContainerQueryVolumeEvidence>, QueryEvidenceFailure>>;
}

#[derive(Clone)]
struct SystemContainerQuerySource {
    docker: DockerCli,
}

impl Default for SystemContainerQuerySource {
    fn default() -> Self {
        let runner: Arc<dyn RuntimeCommandRunner> = Arc::new(QueryRuntimeCommandRunner::new(
            Arc::new(TokioRuntimeCommand),
            QUERY_DOCKER_COMMAND_TIMEOUT,
        ));
        Self {
            docker: DockerCli::new(runner),
        }
    }
}

impl SystemContainerQuerySource {
    async fn collect_containers(
        &self,
        workspace_id: &str,
        hint: DockerContainerLoadHint,
    ) -> Result<ContainerQueryContainersEvidence> {
        let mut containers = self
            .docker
            .list_workspace_container_inspects(workspace_id)
            .await?;
        let mut compose_projects = BTreeSet::new();
        if let Some(project_name) = hint
            .compose_project_name
            .as_deref()
            .map(str::trim)
            .filter(|project_name| !project_name.is_empty())
        {
            compose_projects.insert(project_name.to_owned());
        }
        for container in &containers {
            let Some((managed_workspace_id, labels)) =
                managed_workspace_id_from_container(container)
            else {
                continue;
            };
            if managed_workspace_id != workspace_id {
                continue;
            }
            if let Some(project_name) = compose_project_name_from_labels(labels) {
                compose_projects.insert(project_name);
            }
        }

        for project_name in compose_projects.clone() {
            containers.extend(
                self.docker
                    .list_compose_project_container_inspects_by_project(&project_name)
                    .await?,
            );
        }
        let containers = dedupe_container_inspects(containers)
            .into_iter()
            .filter(|container| {
                container_query_inspect_matches_scope(container, workspace_id, &compose_projects)
            })
            .collect::<Vec<_>>();
        let published_ports =
            container_published_port_snapshots(&containers, &hint.published_ports);
        let containers = containers
            .iter()
            .filter_map(|container| {
                container_query_evidence_from_inspect(container, workspace_id, &compose_projects)
            })
            .collect();

        Ok(ContainerQueryContainersEvidence {
            containers,
            published_ports,
        })
    }

    async fn collect_volumes(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<ContainerQueryVolumeEvidence>> {
        Ok(self
            .docker
            .list_volumes(workspace_id)
            .await?
            .into_iter()
            .map(|name| ContainerQueryVolumeEvidence { name: Some(name) })
            .collect())
    }
}

impl ContainerQuerySource for SystemContainerQuerySource {
    fn load_state<'a>(
        &'a self,
        context: &'a ContainerCliQueryContext,
    ) -> QueryFuture<'a, std::result::Result<Option<WorkspaceState>, LocalEvidenceFailure>> {
        Box::pin(async move {
            load_state_file(context.state_dir()).map_err(|_error| LocalEvidenceFailure)
        })
    }

    fn load_forwarding<'a>(
        &'a self,
        context: &'a ContainerCliQueryContext,
    ) -> QueryFuture<'a, std::result::Result<ForwardStatusList, LocalEvidenceFailure>> {
        Box::pin(async move {
            list_active_forward_status_ports(context.forward_status_dir())
                .await
                .map_err(|_error| LocalEvidenceFailure)
        })
    }

    fn load_containers<'a>(
        &'a self,
        workspace_id: &'a str,
        hint: DockerContainerLoadHint,
    ) -> QueryFuture<'a, std::result::Result<ContainerQueryContainersEvidence, QueryEvidenceFailure>>
    {
        Box::pin(async move {
            self.collect_containers(workspace_id, hint)
                .await
                .map_err(|_error| QueryEvidenceFailure::Unavailable)
        })
    }

    fn load_volumes<'a>(
        &'a self,
        workspace_id: &'a str,
    ) -> QueryFuture<'a, std::result::Result<Vec<ContainerQueryVolumeEvidence>, QueryEvidenceFailure>>
    {
        Box::pin(async move {
            self.collect_volumes(workspace_id)
                .await
                .map_err(|_error| QueryEvidenceFailure::Unavailable)
        })
    }
}

fn dedupe_container_inspects(containers: Vec<ContainerInspect>) -> Vec<ContainerInspect> {
    let mut seen = BTreeSet::new();
    containers
        .into_iter()
        .filter(|container| {
            container
                .id
                .as_deref()
                .filter(|id| !id.trim().is_empty())
                .is_none_or(|id| seen.insert(id.to_owned()))
        })
        .collect()
}

trait QueryClock: Send + Sync {
    fn now(&self) -> Duration;
}

struct SystemQueryClock {
    origin: Instant,
}

impl SystemQueryClock {
    fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl QueryClock for SystemQueryClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }
}

#[derive(Clone)]
struct QueryEvidenceCache {
    inner: Arc<QueryEvidenceCacheInner>,
}

struct QueryEvidenceCacheInner {
    entries: Mutex<HashMap<QueryEvidenceKey, QueryCacheEntry>>,
    source: Arc<dyn ContainerQuerySource>,
    clock: Arc<dyn QueryClock>,
    docker_loads: Semaphore,
    load_timeout: Duration,
}

enum QueryCacheEntry {
    Loading {
        receiver: watch::Receiver<Option<QueryEvidenceResult>>,
    },
    Ready {
        result: QueryEvidenceResult,
        completed_at: Duration,
    },
}

enum QueryCacheLookup {
    Hit(QueryEvidenceResult),
    Wait(watch::Receiver<Option<QueryEvidenceResult>>),
}

impl QueryEvidenceCache {
    fn new(source: Arc<dyn ContainerQuerySource>) -> Self {
        Self::with_clock(
            source,
            Arc::new(SystemQueryClock::new()),
            CONCURRENT_DOCKER_EVIDENCE_LOADS,
            DOCKER_EVIDENCE_LOAD_TIMEOUT,
        )
    }

    fn with_clock(
        source: Arc<dyn ContainerQuerySource>,
        clock: Arc<dyn QueryClock>,
        concurrent_loads: usize,
        load_timeout: Duration,
    ) -> Self {
        Self {
            inner: Arc::new(QueryEvidenceCacheInner {
                entries: Mutex::new(HashMap::new()),
                source,
                clock,
                docker_loads: Semaphore::new(concurrent_loads),
                load_timeout,
            }),
        }
    }

    async fn get(&self, key: QueryEvidenceKey, load: QueryEvidenceLoad) -> QueryEvidenceResult {
        if key.kind != load.kind() {
            return Err(QueryEvidenceFailure::Unavailable);
        }
        let (lookup, pending_load) = {
            let mut entries = self
                .inner
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let decision = match entries.get(&key) {
                Some(QueryCacheEntry::Loading { receiver }) => {
                    (QueryCacheLookup::Wait(receiver.clone()), None)
                }
                Some(QueryCacheEntry::Ready {
                    result,
                    completed_at,
                }) if cache_entry_is_fresh(result, *completed_at, self.inner.clock.now()) => {
                    (QueryCacheLookup::Hit(result.clone()), None)
                }
                Some(QueryCacheEntry::Ready { .. }) | None => {
                    let (sender, receiver) = watch::channel::<Option<QueryEvidenceResult>>(None);
                    entries.insert(
                        key.clone(),
                        QueryCacheEntry::Loading {
                            receiver: receiver.clone(),
                        },
                    );
                    (
                        QueryCacheLookup::Wait(receiver),
                        Some((key.clone(), load, sender)),
                    )
                }
            };
            drop(entries);
            decision
        };
        if let Some((key, load, sender)) = pending_load {
            self.spawn_load(key, load, sender);
        }

        match lookup {
            QueryCacheLookup::Hit(result) => result,
            QueryCacheLookup::Wait(receiver) => wait_for_load(&self.inner, &key, receiver).await,
        }
    }

    fn spawn_load(
        &self,
        key: QueryEvidenceKey,
        load: QueryEvidenceLoad,
        sender: watch::Sender<Option<QueryEvidenceResult>>,
    ) {
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            let result =
                tokio::time::timeout(inner.load_timeout, load_query_evidence(&inner, &key, load))
                    .await
                    .map_or(Err(QueryEvidenceFailure::TimedOut), |result| result);
            let result = match result {
                Ok(evidence) if evidence.matches_kind(key.kind) => Ok(evidence),
                Ok(_) => Err(QueryEvidenceFailure::Unavailable),
                Err(error) => Err(error),
            };
            let completed_at = inner.clock.now();
            inner
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(
                    key,
                    QueryCacheEntry::Ready {
                        result: result.clone(),
                        completed_at,
                    },
                );
            _ = sender.send(Some(result));
        });
    }
}

async fn load_query_evidence(
    inner: &QueryEvidenceCacheInner,
    key: &QueryEvidenceKey,
    load: QueryEvidenceLoad,
) -> QueryEvidenceResult {
    let permit = inner
        .docker_loads
        .acquire()
        .await
        .map_err(|_error| QueryEvidenceFailure::Unavailable)?;
    let result = match load {
        QueryEvidenceLoad::Containers(hint) => inner
            .source
            .load_containers(&key.workspace_id, hint)
            .await
            .map(QueryEvidence::Containers),
        QueryEvidenceLoad::Volumes => inner
            .source
            .load_volumes(&key.workspace_id)
            .await
            .map(QueryEvidence::Volumes),
    };
    drop(permit);
    result
}

fn cache_entry_is_fresh(
    result: &QueryEvidenceResult,
    completed_at: Duration,
    now: Duration,
) -> bool {
    let ttl = if result.is_ok() {
        SUCCESS_CACHE_TTL
    } else {
        FAILURE_CACHE_TTL
    };
    now.saturating_sub(completed_at) < ttl
}

async fn wait_for_load(
    inner: &QueryEvidenceCacheInner,
    key: &QueryEvidenceKey,
    mut receiver: watch::Receiver<Option<QueryEvidenceResult>>,
) -> QueryEvidenceResult {
    loop {
        let result = receiver.borrow().clone();
        if let Some(result) = result {
            return result;
        }
        if receiver.changed().await.is_err() {
            cache_closed_load_failure(inner, key, &receiver);
            return Err(QueryEvidenceFailure::Unavailable);
        }
    }
}

fn cache_closed_load_failure(
    inner: &QueryEvidenceCacheInner,
    key: &QueryEvidenceKey,
    closed_receiver: &watch::Receiver<Option<QueryEvidenceResult>>,
) {
    let mut entries = inner
        .entries
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let load_is_current = matches!(
        entries.get(key),
        Some(QueryCacheEntry::Loading { receiver })
            if receiver.same_channel(closed_receiver)
    );
    if load_is_current {
        entries.insert(
            key.clone(),
            QueryCacheEntry::Ready {
                result: Err(QueryEvidenceFailure::Unavailable),
                completed_at: inner.clock.now(),
            },
        );
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContainerQueryCollection {
    pub(crate) snapshot: ContainerQuerySnapshot,
    pub(crate) warnings: Vec<String>,
}

#[derive(Clone)]
pub(crate) struct ContainerQueryCoordinator {
    context: ContainerCliQueryContext,
    source: Arc<dyn ContainerQuerySource>,
    cache: QueryEvidenceCache,
}

impl ContainerQueryCoordinator {
    pub(crate) fn new(context: ContainerCliQueryContext) -> Self {
        let source: Arc<dyn ContainerQuerySource> = Arc::new(SystemContainerQuerySource::default());
        let cache = QueryEvidenceCache::new(Arc::clone(&source));
        Self {
            context,
            source,
            cache,
        }
    }

    pub(crate) async fn collect(&self) -> ContainerQueryCollection {
        let (state, forwarding) = futures_util::join!(
            self.source.load_state(&self.context),
            self.source.load_forwarding(&self.context),
        );
        let state_ref = state.as_ref().ok().and_then(Option::as_ref);
        let hint = DockerContainerLoadHint::from_state(state_ref);
        let container_key =
            QueryEvidenceKey::from_context(&self.context, QueryEvidenceKind::Containers);
        let volume_key = QueryEvidenceKey::from_context(&self.context, QueryEvidenceKind::Volumes);
        let (containers, volumes) = futures_util::join!(
            self.cache
                .get(container_key, QueryEvidenceLoad::Containers(hint)),
            self.cache.get(volume_key, QueryEvidenceLoad::Volumes),
        );

        build_query_collection(
            self.context.workspace_id(),
            state,
            forwarding,
            containers,
            volumes,
        )
    }

    #[cfg(test)]
    fn with_source(
        context: ContainerCliQueryContext,
        source: Arc<dyn ContainerQuerySource>,
        clock: Arc<dyn QueryClock>,
    ) -> Self {
        let cache = QueryEvidenceCache::with_clock(
            Arc::clone(&source),
            clock,
            CONCURRENT_DOCKER_EVIDENCE_LOADS,
            DOCKER_EVIDENCE_LOAD_TIMEOUT,
        );
        Self {
            context,
            source,
            cache,
        }
    }
}

pub(super) struct ContainerCliQueryService {
    coordinator: ContainerQueryCoordinator,
    active_queries: Arc<Semaphore>,
    query_timeout: Duration,
}

impl ContainerCliQueryService {
    pub(super) fn new(context: ContainerCliQueryContext) -> Self {
        Self {
            coordinator: ContainerQueryCoordinator::new(context),
            active_queries: Arc::new(Semaphore::new(ACTIVE_CONTAINER_CLI_QUERIES)),
            query_timeout: CONTAINER_CLI_QUERY_TIMEOUT,
        }
    }

    #[cfg(test)]
    pub(super) fn with_runtime_command(
        context: ContainerCliQueryContext,
        runtime_command: Arc<dyn RuntimeCommandRunner>,
    ) -> Self {
        let query_runner =
            QueryRuntimeCommandRunner::new(runtime_command, QUERY_DOCKER_COMMAND_TIMEOUT);
        let source: Arc<dyn ContainerQuerySource> = Arc::new(SystemContainerQuerySource {
            docker: DockerCli::new(Arc::new(query_runner)),
        });
        let coordinator = ContainerQueryCoordinator::with_source(
            context,
            source,
            Arc::new(SystemQueryClock::new()),
        );
        Self::with_limits(
            coordinator,
            ACTIVE_CONTAINER_CLI_QUERIES,
            CONTAINER_CLI_QUERY_TIMEOUT,
        )
    }

    pub(super) async fn execute(&self, query: ContainerCliQuery) -> Result<Vec<u8>> {
        bounded_cli_query_response(&self.active_queries, self.query_timeout, || async {
            render_container_cli_query(&self.coordinator, query).await
        })
        .await
    }

    #[cfg(test)]
    fn with_limits(
        coordinator: ContainerQueryCoordinator,
        active_queries: usize,
        query_timeout: Duration,
    ) -> Self {
        Self {
            coordinator,
            active_queries: Arc::new(Semaphore::new(active_queries)),
            query_timeout,
        }
    }
}

/// Query runtime fixed at daemon startup. Carrying the service inside the enabled
/// variant guarantees by type that an admitted query always has a service to run on.
#[derive(Clone)]
pub(super) enum ContainerCliQueryRuntime {
    Disabled,
    Enabled(Arc<ContainerCliQueryService>),
}

impl ContainerCliQueryRuntime {
    pub(super) fn from_policy(policy: &HostDaemonCliQueryPolicy) -> Self {
        match policy {
            HostDaemonCliQueryPolicy::Disabled => Self::Disabled,
            HostDaemonCliQueryPolicy::Enabled(context) => {
                Self::Enabled(Arc::new(ContainerCliQueryService::new(context.clone())))
            }
        }
    }

    #[cfg(test)]
    pub(super) fn from_policy_with_runtime_command(
        policy: &HostDaemonCliQueryPolicy,
        runtime_command: Arc<dyn RuntimeCommandRunner>,
    ) -> Self {
        match policy {
            HostDaemonCliQueryPolicy::Disabled => Self::Disabled,
            HostDaemonCliQueryPolicy::Enabled(context) => Self::Enabled(Arc::new(
                ContainerCliQueryService::with_runtime_command(context.clone(), runtime_command),
            )),
        }
    }
}

async fn bounded_cli_query_response<F, Fut>(
    active_queries: &Semaphore,
    query_timeout: Duration,
    execute: F,
) -> Result<Vec<u8>>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<HostDaemonResponse>>,
{
    let Ok(permit) = active_queries.try_acquire() else {
        return serialize_cli_query_response(&HostDaemonResponse::error(
            ERROR_CODE_CLI_QUERY_BUSY,
            "Container CLI query capacity is exhausted",
        ));
    };
    let result = tokio::time::timeout(query_timeout, async {
        let response = execute().await?;
        serialize_cli_query_response(&response)
    })
    .await;
    drop(permit);

    let response = match result {
        Ok(Ok(response)) => return Ok(response),
        Ok(Err(_error)) => {
            HostDaemonResponse::error(ERROR_CODE_CLI_QUERY_FAILED, "Container CLI query failed")
        }
        Err(_elapsed) => HostDaemonResponse::error(
            ERROR_CODE_CLI_QUERY_TIMEOUT,
            "Container CLI query timed out",
        ),
    };
    serialize_cli_query_response(&response)
}

fn serialize_cli_query_response(response: &HostDaemonResponse) -> Result<Vec<u8>> {
    serde_json::to_vec(response).context("Failed to serialize container CLI query response")
}

async fn render_container_cli_query(
    coordinator: &ContainerQueryCoordinator,
    query: ContainerCliQuery,
) -> Result<HostDaemonResponse> {
    let collection = coordinator.collect().await;
    let output = match query {
        ContainerCliQuery::StatusText => {
            let status = build_container_workspace_status(&collection.snapshot);
            render_container_workspace_status(&status)
        }
        ContainerCliQuery::PortsText => render_container_ports_text(&collection.snapshot.ports),
        ContainerCliQuery::PortsJson => render_container_ports_json(&collection.snapshot.ports)?,
    };

    Ok(HostDaemonResponse::success_with_warnings(
        output,
        collection.warnings,
    ))
}

fn build_query_collection(
    workspace_id: &str,
    state: std::result::Result<Option<WorkspaceState>, LocalEvidenceFailure>,
    forwarding: std::result::Result<ForwardStatusList, LocalEvidenceFailure>,
    containers: QueryEvidenceResult,
    volumes: QueryEvidenceResult,
) -> ContainerQueryCollection {
    let mut warnings = Vec::new();
    let state_evidence = match state {
        Ok(Some(state)) => {
            ContainerQueryStateEvidence::Available(ContainerQueryStateSnapshot::from_state(&state))
        }
        Ok(None) => ContainerQueryStateEvidence::Missing,
        Err(_) => {
            warnings.push("Recorded workspace state is unavailable".to_owned());
            ContainerQueryStateEvidence::Unavailable
        }
    };

    let (mut ports, forwarding_warning) = forwarding.map_or_else(
        |_failure| (Vec::new(), Some("Port forwarding status is unavailable")),
        |status| {
            let warning = (!status.warnings.is_empty())
                .then_some("Some port forwarding status entries are unavailable");
            (container_forwarded_port_snapshots(status.ports), warning)
        },
    );
    if let Some(warning) = forwarding_warning {
        warnings.push(warning.to_owned());
    }

    let containers = match containers {
        Ok(QueryEvidence::Containers(evidence)) => Some(evidence),
        Ok(QueryEvidence::Volumes(_)) => {
            warnings.push(
                docker_evidence_warning(
                    QueryEvidenceKind::Containers,
                    QueryEvidenceFailure::Unavailable,
                )
                .to_owned(),
            );
            None
        }
        Err(failure) => {
            warnings
                .push(docker_evidence_warning(QueryEvidenceKind::Containers, failure).to_owned());
            None
        }
    };
    let volumes = match volumes {
        Ok(QueryEvidence::Volumes(evidence)) => evidence,
        Ok(QueryEvidence::Containers(_)) => {
            warnings.push(
                docker_evidence_warning(
                    QueryEvidenceKind::Volumes,
                    QueryEvidenceFailure::Unavailable,
                )
                .to_owned(),
            );
            Vec::new()
        }
        Err(failure) => {
            warnings.push(docker_evidence_warning(QueryEvidenceKind::Volumes, failure).to_owned());
            Vec::new()
        }
    };

    let docker = containers.map_or(ContainerQueryDockerEvidence::Unavailable, |containers| {
        ports.extend(containers.published_ports);
        ContainerQueryDockerEvidence::Available(ContainerQueryRuntimeSnapshot {
            containers: containers.containers,
            volumes,
        })
    });

    ContainerQueryCollection {
        snapshot: ContainerQuerySnapshot {
            workspace_id: workspace_id.to_owned(),
            state: state_evidence,
            docker,
            ports,
        },
        warnings,
    }
}

const fn docker_evidence_warning(
    kind: QueryEvidenceKind,
    failure: QueryEvidenceFailure,
) -> &'static str {
    match (kind, failure) {
        (QueryEvidenceKind::Containers, QueryEvidenceFailure::Unavailable) => {
            "Docker container evidence is unavailable"
        }
        (QueryEvidenceKind::Containers, QueryEvidenceFailure::TimedOut) => {
            "Docker container evidence timed out"
        }
        (QueryEvidenceKind::Volumes, QueryEvidenceFailure::Unavailable) => {
            "Docker volume evidence is unavailable"
        }
        (QueryEvidenceKind::Volumes, QueryEvidenceFailure::TimedOut) => {
            "Docker volume evidence timed out"
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    };

    use serde_json::json;
    use tokio::sync::Semaphore;

    use super::*;
    use crate::{
        host::{
            forward::{ActiveForwardPort, ForwardStatusSource},
            query_context::HostDaemonCliQueryPolicy,
        },
        runtime::command::{FakeRuntimeCommand, RuntimeOutput},
        state::{CloneIsolationRuntimeState, LifecycleState, WorkspaceModeSnapshot},
        status::container::{ContainerQueryContainerEvidence, HealthStatus, RuntimeRunState},
    };

    const WORKSPACE_ID: &str = "123456abcdef";
    const OTHER_WORKSPACE_ID: &str = "abcdef123456";
    const HOST_PATH: &str = "/host/private/workspace";
    const RAW_CONFIG_HASH: &str = "raw-config-hash-secret-marker";
    const RAW_STDERR: &str = "raw-stderr-secret-marker";
    const RAW_PROJECT_LABEL: &str = "raw-project-label-secret-marker";
    const SECRET: &str = "container-secret-marker";

    fn decode_response(response: Result<Vec<u8>>) -> HostDaemonResponse {
        serde_json::from_slice(&response.unwrap()).unwrap()
    }

    #[derive(Default)]
    struct FakeClock {
        millis: AtomicU64,
    }

    impl FakeClock {
        fn advance(&self, duration: Duration) {
            let millis = u64::try_from(duration.as_millis()).unwrap();
            self.millis.fetch_add(millis, Ordering::SeqCst);
        }
    }

    impl QueryClock for FakeClock {
        fn now(&self) -> Duration {
            Duration::from_millis(self.millis.load(Ordering::SeqCst))
        }
    }

    struct QueueSource {
        container_results: Mutex<
            VecDeque<std::result::Result<ContainerQueryContainersEvidence, QueryEvidenceFailure>>,
        >,
        container_calls: AtomicUsize,
        panic_on_container_call: Option<usize>,
    }

    impl QueueSource {
        fn new(
            results: impl IntoIterator<
                Item = std::result::Result<ContainerQueryContainersEvidence, QueryEvidenceFailure>,
            >,
        ) -> Self {
            Self {
                container_results: Mutex::new(results.into_iter().collect()),
                container_calls: AtomicUsize::new(0),
                panic_on_container_call: None,
            }
        }

        fn panic_once_then(
            result: std::result::Result<ContainerQueryContainersEvidence, QueryEvidenceFailure>,
        ) -> Self {
            Self {
                container_results: Mutex::new(VecDeque::from([result])),
                container_calls: AtomicUsize::new(0),
                panic_on_container_call: Some(0),
            }
        }
    }

    impl ContainerQuerySource for QueueSource {
        fn load_state<'a>(
            &'a self,
            _context: &'a ContainerCliQueryContext,
        ) -> QueryFuture<'a, std::result::Result<Option<WorkspaceState>, LocalEvidenceFailure>>
        {
            Box::pin(async { Ok(None) })
        }

        fn load_forwarding<'a>(
            &'a self,
            _context: &'a ContainerCliQueryContext,
        ) -> QueryFuture<'a, std::result::Result<ForwardStatusList, LocalEvidenceFailure>> {
            Box::pin(async {
                Ok(ForwardStatusList {
                    ports: Vec::new(),
                    warnings: Vec::new(),
                })
            })
        }

        fn load_containers<'a>(
            &'a self,
            _workspace_id: &'a str,
            _hint: DockerContainerLoadHint,
        ) -> QueryFuture<
            'a,
            std::result::Result<ContainerQueryContainersEvidence, QueryEvidenceFailure>,
        > {
            Box::pin(async move {
                let call = self.container_calls.fetch_add(1, Ordering::SeqCst);
                assert_ne!(
                    self.panic_on_container_call,
                    Some(call),
                    "simulated query evidence load panic"
                );
                self.container_results.lock().unwrap().pop_front().unwrap()
            })
        }

        fn load_volumes<'a>(
            &'a self,
            _workspace_id: &'a str,
        ) -> QueryFuture<
            'a,
            std::result::Result<Vec<ContainerQueryVolumeEvidence>, QueryEvidenceFailure>,
        > {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    struct BlockingSource {
        calls: AtomicUsize,
        active: AtomicUsize,
        max_active: AtomicUsize,
        started: Semaphore,
        release: Semaphore,
    }

    impl BlockingSource {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                active: AtomicUsize::new(0),
                max_active: AtomicUsize::new(0),
                started: Semaphore::new(0),
                release: Semaphore::new(0),
            }
        }

        async fn wait(&self) {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            self.started.add_permits(1);
            self.release.acquire().await.unwrap().forget();
            self.active.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl ContainerQuerySource for BlockingSource {
        fn load_state<'a>(
            &'a self,
            _context: &'a ContainerCliQueryContext,
        ) -> QueryFuture<'a, std::result::Result<Option<WorkspaceState>, LocalEvidenceFailure>>
        {
            Box::pin(async { Ok(None) })
        }

        fn load_forwarding<'a>(
            &'a self,
            _context: &'a ContainerCliQueryContext,
        ) -> QueryFuture<'a, std::result::Result<ForwardStatusList, LocalEvidenceFailure>> {
            Box::pin(async {
                Ok(ForwardStatusList {
                    ports: Vec::new(),
                    warnings: Vec::new(),
                })
            })
        }

        fn load_containers<'a>(
            &'a self,
            _workspace_id: &'a str,
            _hint: DockerContainerLoadHint,
        ) -> QueryFuture<
            'a,
            std::result::Result<ContainerQueryContainersEvidence, QueryEvidenceFailure>,
        > {
            Box::pin(async move {
                self.wait().await;
                Ok(container_evidence("loaded"))
            })
        }

        fn load_volumes<'a>(
            &'a self,
            _workspace_id: &'a str,
        ) -> QueryFuture<
            'a,
            std::result::Result<Vec<ContainerQueryVolumeEvidence>, QueryEvidenceFailure>,
        > {
            Box::pin(async move {
                self.wait().await;
                Ok(Vec::new())
            })
        }
    }

    struct PendingThenSuccessSource {
        calls: AtomicUsize,
        started: Semaphore,
    }

    impl PendingThenSuccessSource {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                started: Semaphore::new(0),
            }
        }
    }

    impl ContainerQuerySource for PendingThenSuccessSource {
        fn load_state<'a>(
            &'a self,
            _context: &'a ContainerCliQueryContext,
        ) -> QueryFuture<'a, std::result::Result<Option<WorkspaceState>, LocalEvidenceFailure>>
        {
            Box::pin(async { Ok(None) })
        }

        fn load_forwarding<'a>(
            &'a self,
            _context: &'a ContainerCliQueryContext,
        ) -> QueryFuture<'a, std::result::Result<ForwardStatusList, LocalEvidenceFailure>> {
            Box::pin(async {
                Ok(ForwardStatusList {
                    ports: Vec::new(),
                    warnings: Vec::new(),
                })
            })
        }

        fn load_containers<'a>(
            &'a self,
            _workspace_id: &'a str,
            _hint: DockerContainerLoadHint,
        ) -> QueryFuture<
            'a,
            std::result::Result<ContainerQueryContainersEvidence, QueryEvidenceFailure>,
        > {
            Box::pin(async move {
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                self.started.add_permits(1);
                if call == 0 {
                    std::future::pending().await
                } else {
                    Ok(container_evidence("after-timeout"))
                }
            })
        }

        fn load_volumes<'a>(
            &'a self,
            _workspace_id: &'a str,
        ) -> QueryFuture<
            'a,
            std::result::Result<Vec<ContainerQueryVolumeEvidence>, QueryEvidenceFailure>,
        > {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    struct CoordinatorSource {
        state_reads: AtomicUsize,
        forward_queries: AtomicUsize,
        container_loads: AtomicUsize,
        volume_loads: AtomicUsize,
    }

    impl CoordinatorSource {
        fn new() -> Self {
            Self {
                state_reads: AtomicUsize::new(0),
                forward_queries: AtomicUsize::new(0),
                container_loads: AtomicUsize::new(0),
                volume_loads: AtomicUsize::new(0),
            }
        }
    }

    impl ContainerQuerySource for CoordinatorSource {
        fn load_state<'a>(
            &'a self,
            _context: &'a ContainerCliQueryContext,
        ) -> QueryFuture<'a, std::result::Result<Option<WorkspaceState>, LocalEvidenceFailure>>
        {
            Box::pin(async move {
                self.state_reads.fetch_add(1, Ordering::SeqCst);
                Ok(Some(workspace_state()))
            })
        }

        fn load_forwarding<'a>(
            &'a self,
            _context: &'a ContainerCliQueryContext,
        ) -> QueryFuture<'a, std::result::Result<ForwardStatusList, LocalEvidenceFailure>> {
            Box::pin(async move {
                self.forward_queries.fetch_add(1, Ordering::SeqCst);
                Ok(ForwardStatusList {
                    ports: vec![ActiveForwardPort {
                        host_ip: "127.0.0.1".to_owned(),
                        host_port: 3000,
                        requested_host_port: 3000,
                        service: Some("app".to_owned()),
                        container_port: 3000,
                        protocol: "tcp".to_owned(),
                        source: ForwardStatusSource::Configured,
                        label: Some("web".to_owned()),
                    }],
                    warnings: Vec::new(),
                })
            })
        }

        fn load_containers<'a>(
            &'a self,
            _workspace_id: &'a str,
            _hint: DockerContainerLoadHint,
        ) -> QueryFuture<
            'a,
            std::result::Result<ContainerQueryContainersEvidence, QueryEvidenceFailure>,
        > {
            Box::pin(async move {
                self.container_loads.fetch_add(1, Ordering::SeqCst);
                Ok(container_evidence("primary"))
            })
        }

        fn load_volumes<'a>(
            &'a self,
            _workspace_id: &'a str,
        ) -> QueryFuture<
            'a,
            std::result::Result<Vec<ContainerQueryVolumeEvidence>, QueryEvidenceFailure>,
        > {
            Box::pin(async move {
                self.volume_loads.fetch_add(1, Ordering::SeqCst);
                Ok(vec![ContainerQueryVolumeEvidence {
                    name: Some("workspace-volume".to_owned()),
                }])
            })
        }
    }

    #[test]
    fn parallel_cold_misses_share_one_semantic_load() {
        run_async(async {
            let source = Arc::new(BlockingSource::new());
            let cache = test_cache(Arc::clone(&source), Arc::new(FakeClock::default()), 2);
            let key = key(WORKSPACE_ID, QueryEvidenceKind::Containers);
            let first = tokio::spawn({
                let cache = cache.clone();
                let key = key.clone();
                async move { cache.get(key, container_load()).await }
            });
            let second = tokio::spawn({
                let cache = cache.clone();
                let key = key.clone();
                async move { cache.get(key, container_load()).await }
            });

            source.started.acquire().await.unwrap().forget();
            tokio::task::yield_now().await;
            assert_eq!(source.calls.load(Ordering::SeqCst), 1);

            source.release.add_permits(1);
            assert_eq!(first.await.unwrap(), second.await.unwrap());
            assert_eq!(source.calls.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn docker_loads_never_exceed_two_across_keys() {
        run_async(async {
            let source = Arc::new(BlockingSource::new());
            let cache = test_cache(Arc::clone(&source), Arc::new(FakeClock::default()), 2);
            let tasks = [
                key(WORKSPACE_ID, QueryEvidenceKind::Containers),
                key(WORKSPACE_ID, QueryEvidenceKind::Volumes),
                key(OTHER_WORKSPACE_ID, QueryEvidenceKind::Containers),
            ]
            .into_iter()
            .map(|key| {
                let cache = cache.clone();
                let load = load_for_kind(key.kind);
                tokio::spawn(async move { cache.get(key, load).await })
            })
            .collect::<Vec<_>>();

            for _ in 0..2 {
                source.started.acquire().await.unwrap().forget();
            }
            tokio::task::yield_now().await;
            assert_eq!(source.max_active.load(Ordering::SeqCst), 2);
            assert_eq!(source.calls.load(Ordering::SeqCst), 2);

            source.release.add_permits(2);
            source.started.acquire().await.unwrap().forget();
            assert_eq!(source.max_active.load(Ordering::SeqCst), 2);
            source.release.add_permits(1);
            for task in tasks {
                task.await.unwrap().unwrap();
            }
        });
    }

    #[test]
    fn success_and_failure_ttl_boundaries_use_completion_time() {
        run_async(async {
            let clock = Arc::new(FakeClock::default());
            let source = Arc::new(QueueSource::new([
                Ok(container_evidence("first")),
                Ok(container_evidence("second")),
                Err(QueryEvidenceFailure::Unavailable),
                Ok(container_evidence("after-failure")),
            ]));
            let cache = test_cache(Arc::clone(&source), Arc::clone(&clock), 2);
            let key = key(WORKSPACE_ID, QueryEvidenceKind::Containers);

            let first = cache.get(key.clone(), container_load()).await.unwrap();
            clock.advance(Duration::from_millis(1999));
            assert_eq!(
                cache.get(key.clone(), container_load()).await.unwrap(),
                first
            );
            assert_eq!(source.container_calls.load(Ordering::SeqCst), 1);

            clock.advance(Duration::from_millis(1));
            let second = cache.get(key.clone(), container_load()).await.unwrap();
            assert_ne!(second, first);
            assert_eq!(source.container_calls.load(Ordering::SeqCst), 2);

            clock.advance(SUCCESS_CACHE_TTL);
            assert_eq!(
                cache.get(key.clone(), container_load()).await,
                Err(QueryEvidenceFailure::Unavailable)
            );
            clock.advance(Duration::from_millis(499));
            assert_eq!(
                cache.get(key.clone(), container_load()).await,
                Err(QueryEvidenceFailure::Unavailable)
            );
            assert_eq!(source.container_calls.load(Ordering::SeqCst), 3);

            clock.advance(Duration::from_millis(1));
            cache.get(key, container_load()).await.unwrap();
            assert_eq!(source.container_calls.load(Ordering::SeqCst), 4);
        });
    }

    #[test]
    fn expired_success_refresh_failure_never_returns_stale_evidence() {
        run_async(async {
            let clock = Arc::new(FakeClock::default());
            let source = Arc::new(QueueSource::new([
                Ok(container_evidence("fresh")),
                Err(QueryEvidenceFailure::Unavailable),
            ]));
            let cache = test_cache(source, Arc::clone(&clock), 2);
            let key = key(WORKSPACE_ID, QueryEvidenceKind::Containers);

            cache.get(key.clone(), container_load()).await.unwrap();
            clock.advance(SUCCESS_CACHE_TTL);

            assert_eq!(
                cache.get(key, container_load()).await,
                Err(QueryEvidenceFailure::Unavailable)
            );
        });
    }

    #[test]
    fn leader_cancellation_does_not_cancel_load_or_strand_waiter() {
        run_async(async {
            let source = Arc::new(BlockingSource::new());
            let cache = test_cache(Arc::clone(&source), Arc::new(FakeClock::default()), 2);
            let key = key(WORKSPACE_ID, QueryEvidenceKind::Containers);
            let leader = tokio::spawn({
                let cache = cache.clone();
                let key = key.clone();
                async move { cache.get(key, container_load()).await }
            });

            source.started.acquire().await.unwrap().forget();
            let waiter = tokio::spawn({
                let cache = cache.clone();
                async move { cache.get(key, container_load()).await }
            });
            tokio::task::yield_now().await;
            leader.abort();
            source.release.add_permits(1);

            waiter.await.unwrap().unwrap();
            assert!(leader.await.unwrap_err().is_cancelled());
            assert_eq!(source.calls.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn load_task_panic_wakes_waiter_and_allows_retry_after_failure_ttl() {
        run_async(async {
            let clock = Arc::new(FakeClock::default());
            let source = Arc::new(QueueSource::panic_once_then(Ok(container_evidence(
                "after-panic",
            ))));
            let cache = test_cache(Arc::clone(&source), Arc::clone(&clock), 2);
            let key = key(WORKSPACE_ID, QueryEvidenceKind::Containers);

            assert_eq!(
                cache.get(key.clone(), container_load()).await,
                Err(QueryEvidenceFailure::Unavailable)
            );
            clock.advance(Duration::from_millis(499));
            assert_eq!(
                cache.get(key.clone(), container_load()).await,
                Err(QueryEvidenceFailure::Unavailable)
            );
            assert_eq!(source.container_calls.load(Ordering::SeqCst), 1);

            clock.advance(Duration::from_millis(1));
            assert_eq!(
                cache.get(key, container_load()).await.unwrap(),
                QueryEvidence::Containers(container_evidence("after-panic"))
            );
            assert_eq!(source.container_calls.load(Ordering::SeqCst), 2);
        });
    }

    #[test]
    fn stale_closed_load_does_not_overwrite_a_new_load_generation() {
        let source = Arc::new(QueueSource::new([]));
        let cache = test_cache(source, Arc::new(FakeClock::default()), 2);
        let key = key(WORKSPACE_ID, QueryEvidenceKind::Containers);
        let (closed_sender, closed_receiver) = watch::channel::<Option<QueryEvidenceResult>>(None);
        let (_current_sender, current_receiver) =
            watch::channel::<Option<QueryEvidenceResult>>(None);
        drop(closed_sender);
        cache.inner.entries.lock().unwrap().insert(
            key.clone(),
            QueryCacheEntry::Loading {
                receiver: current_receiver.clone(),
            },
        );

        cache_closed_load_failure(&cache.inner, &key, &closed_receiver);

        assert!(matches!(
            cache.inner.entries.lock().unwrap().get(&key),
            Some(QueryCacheEntry::Loading { receiver })
                if receiver.same_channel(&current_receiver)
        ));
    }

    #[test]
    fn load_timeout_wakes_all_waiters_and_allows_retry() {
        run_async(async {
            tokio::time::pause();
            let clock = Arc::new(FakeClock::default());
            let source = Arc::new(PendingThenSuccessSource::new());
            let cache = test_cache(Arc::clone(&source), Arc::clone(&clock), 2);
            let key = key(WORKSPACE_ID, QueryEvidenceKind::Containers);
            let first = tokio::spawn({
                let cache = cache.clone();
                let key = key.clone();
                async move { cache.get(key, container_load()).await }
            });
            let second = tokio::spawn({
                let cache = cache.clone();
                let key = key.clone();
                async move { cache.get(key, container_load()).await }
            });

            source.started.acquire().await.unwrap().forget();
            tokio::time::advance(DOCKER_EVIDENCE_LOAD_TIMEOUT).await;
            tokio::task::yield_now().await;
            assert_eq!(first.await.unwrap(), Err(QueryEvidenceFailure::TimedOut));
            assert_eq!(second.await.unwrap(), Err(QueryEvidenceFailure::TimedOut));

            clock.advance(FAILURE_CACHE_TTL);
            cache.get(key, container_load()).await.unwrap();
            assert_eq!(source.calls.load(Ordering::SeqCst), 2);
        });
    }

    #[test]
    fn coordinator_reads_state_and_forwarding_once_per_query_and_shares_cache() {
        run_async(async {
            let context = test_context();
            let source = Arc::new(CoordinatorSource::new());
            let clock = Arc::new(FakeClock::default());
            let coordinator_source: Arc<dyn ContainerQuerySource> =
                Arc::<CoordinatorSource>::clone(&source);
            let coordinator_clock: Arc<dyn QueryClock> = clock;
            let coordinator = ContainerQueryCoordinator::with_source(
                context,
                coordinator_source,
                coordinator_clock,
            );

            let status_snapshot = coordinator.collect().await;
            assert_eq!(source.state_reads.load(Ordering::SeqCst), 1);
            assert_eq!(source.forward_queries.load(Ordering::SeqCst), 1);
            assert_eq!(source.container_loads.load(Ordering::SeqCst), 1);
            assert_eq!(source.volume_loads.load(Ordering::SeqCst), 1);
            assert_eq!(status_snapshot.snapshot.ports.len(), 1);

            let ports_snapshot = coordinator.collect().await;
            assert_eq!(source.state_reads.load(Ordering::SeqCst), 2);
            assert_eq!(source.forward_queries.load(Ordering::SeqCst), 2);
            assert_eq!(source.container_loads.load(Ordering::SeqCst), 1);
            assert_eq!(source.volume_loads.load(Ordering::SeqCst), 1);
            assert_eq!(ports_snapshot.snapshot, status_snapshot.snapshot);
        });
    }

    #[test]
    fn query_service_renders_status_and_ports_with_shared_response_contract() {
        run_async(async {
            let context = test_context();
            let source = Arc::new(CoordinatorSource::new());
            let coordinator_source: Arc<dyn ContainerQuerySource> =
                Arc::<CoordinatorSource>::clone(&source);
            let coordinator = ContainerQueryCoordinator::with_source(
                context,
                coordinator_source,
                Arc::new(FakeClock::default()),
            );
            let service = ContainerCliQueryService::with_limits(
                coordinator,
                ACTIVE_CONTAINER_CLI_QUERIES,
                CONTAINER_CLI_QUERY_TIMEOUT,
            );

            let status = decode_response(service.execute(ContainerCliQuery::StatusText).await);
            let ports_text = decode_response(service.execute(ContainerCliQuery::PortsText).await);
            let ports_json = decode_response(service.execute(ContainerCliQuery::PortsJson).await);

            assert!(status.ok);
            assert!(status.error.is_none());
            assert!(status.warnings.is_empty());
            let status_output = status.output.unwrap();
            assert!(status_output.contains("Workspace ID: 123456abcdef"));
            assert!(status_output.contains("Live workspace: not checked"));
            assert!(status_output.ends_with('\n'));
            assert!(!status_output.ends_with("\n\n"));

            assert!(ports_text.ok);
            assert!(ports_text.warnings.is_empty());
            let ports_text_output = ports_text.output.unwrap();
            assert!(ports_text_output.starts_with("LOCAL"));
            assert!(ports_text_output.contains("127.0.0.1:3000"));
            assert!(ports_text_output.ends_with('\n'));

            assert!(ports_json.ok);
            assert!(ports_json.warnings.is_empty());
            let ports_json_output = ports_json.output.unwrap();
            let ports: Vec<serde_json::Value> = serde_json::from_str(&ports_json_output).unwrap();
            assert_eq!(ports.len(), 1);
            assert_eq!(ports[0]["host_port"], 3000);
            assert!(ports[0].get("workspace").is_none());
            assert!(ports[0].get("workspace_id").is_none());
            assert!(ports_json_output.ends_with('\n'));

            assert_eq!(source.state_reads.load(Ordering::SeqCst), 3);
            assert_eq!(source.forward_queries.load(Ordering::SeqCst), 3);
            assert_eq!(source.container_loads.load(Ordering::SeqCst), 1);
            assert_eq!(source.volume_loads.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn query_service_keeps_warnings_out_of_json_output() {
        run_async(async {
            let context = test_context();
            let source = Arc::new(QueueSource::new([Err(QueryEvidenceFailure::TimedOut)]));
            let coordinator_source: Arc<dyn ContainerQuerySource> =
                Arc::<QueueSource>::clone(&source);
            let coordinator = ContainerQueryCoordinator::with_source(
                context,
                coordinator_source,
                Arc::new(FakeClock::default()),
            );
            let service = ContainerCliQueryService::with_limits(
                coordinator,
                ACTIVE_CONTAINER_CLI_QUERIES,
                CONTAINER_CLI_QUERY_TIMEOUT,
            );

            let response = decode_response(service.execute(ContainerCliQuery::PortsJson).await);

            assert!(response.ok);
            assert_eq!(
                response.warnings,
                vec!["Docker container evidence timed out"]
            );
            let output = response.output.unwrap();
            assert_eq!(
                serde_json::from_str::<Vec<serde_json::Value>>(&output).unwrap(),
                Vec::<serde_json::Value>::new()
            );
            assert!(!output.contains("timed out"));
            for forbidden in [
                HOST_PATH,
                RAW_CONFIG_HASH,
                RAW_PROJECT_LABEL,
                SECRET,
                RAW_STDERR,
            ] {
                assert!(!output.contains(forbidden));
                assert!(
                    response
                        .warnings
                        .iter()
                        .all(|warning| !warning.contains(forbidden))
                );
            }
        });
    }

    #[test]
    fn query_admission_never_exceeds_eight_and_rejects_busy_without_execution() {
        run_async(async {
            let active_queries = Arc::new(Semaphore::new(ACTIVE_CONTAINER_CLI_QUERIES));
            let started = Arc::new(Semaphore::new(0));
            let release = Arc::new(Semaphore::new(0));
            let tasks = (0..ACTIVE_CONTAINER_CLI_QUERIES)
                .map(|_| {
                    let active_queries = Arc::clone(&active_queries);
                    let started = Arc::clone(&started);
                    let release = Arc::clone(&release);
                    tokio::spawn(async move {
                        bounded_cli_query_response(
                            &active_queries,
                            CONTAINER_CLI_QUERY_TIMEOUT,
                            || async move {
                                started.add_permits(1);
                                release.acquire().await.unwrap().forget();
                                Ok(HostDaemonResponse::success("ready\n"))
                            },
                        )
                        .await
                    })
                })
                .collect::<Vec<_>>();
            for _ in 0..ACTIVE_CONTAINER_CLI_QUERIES {
                started.acquire().await.unwrap().forget();
            }
            let busy_executed = Arc::new(AtomicBool::new(false));
            let busy_executed_for_query = Arc::clone(&busy_executed);

            let busy = decode_response(
                bounded_cli_query_response(
                    &active_queries,
                    CONTAINER_CLI_QUERY_TIMEOUT,
                    || async move {
                        busy_executed_for_query.store(true, Ordering::SeqCst);
                        Ok(HostDaemonResponse::success("must not execute"))
                    },
                )
                .await,
            );

            assert!(!busy.ok);
            assert_eq!(busy.error.unwrap().code, ERROR_CODE_CLI_QUERY_BUSY);
            assert!(busy.output.is_none());
            assert!(busy.warnings.is_empty());
            assert!(!busy_executed.load(Ordering::SeqCst));

            release.add_permits(ACTIVE_CONTAINER_CLI_QUERIES);
            for task in tasks {
                assert!(decode_response(task.await.unwrap()).ok);
            }
        });
    }

    #[test]
    fn query_timeout_and_fatal_error_return_sanitized_error_only_responses() {
        run_async(async {
            tokio::time::pause();
            let active_queries = Arc::new(Semaphore::new(1));
            let timeout_task = tokio::spawn({
                let active_queries = Arc::clone(&active_queries);
                async move {
                    bounded_cli_query_response(
                        &active_queries,
                        CONTAINER_CLI_QUERY_TIMEOUT,
                        || async { std::future::pending::<Result<HostDaemonResponse>>().await },
                    )
                    .await
                }
            });
            tokio::task::yield_now().await;
            tokio::time::advance(CONTAINER_CLI_QUERY_TIMEOUT).await;
            let timed_out = decode_response(timeout_task.await.unwrap());

            assert!(!timed_out.ok);
            assert_eq!(timed_out.error.unwrap().code, ERROR_CODE_CLI_QUERY_TIMEOUT);
            assert!(timed_out.output.is_none());
            assert!(timed_out.warnings.is_empty());

            let fatal = decode_response(
                bounded_cli_query_response(
                    &active_queries,
                    CONTAINER_CLI_QUERY_TIMEOUT,
                    || async { anyhow::bail!("fatal error containing {SECRET} and {HOST_PATH}") },
                )
                .await,
            );
            let serialized = serde_json::to_string(&fatal).unwrap();

            assert!(!fatal.ok);
            assert_eq!(fatal.error.unwrap().code, ERROR_CODE_CLI_QUERY_FAILED);
            assert!(fatal.output.is_none());
            assert!(fatal.warnings.is_empty());
            assert!(!serialized.contains(SECRET));
            assert!(!serialized.contains(HOST_PATH));
        });
    }

    #[test]
    fn cached_docker_failure_degrades_to_the_same_sanitized_warning() {
        run_async(async {
            let context = test_context();
            let source = Arc::new(QueueSource::new([Err(QueryEvidenceFailure::Unavailable)]));
            let coordinator_source: Arc<dyn ContainerQuerySource> =
                Arc::<QueueSource>::clone(&source);
            let coordinator = ContainerQueryCoordinator::with_source(
                context,
                coordinator_source,
                Arc::new(FakeClock::default()),
            );

            let first = coordinator.collect().await;
            let second = coordinator.collect().await;

            assert_eq!(
                first.warnings,
                vec!["Docker container evidence is unavailable"]
            );
            assert_eq!(second.warnings, first.warnings);
            assert_eq!(
                first.snapshot.docker,
                ContainerQueryDockerEvidence::Unavailable
            );
            assert_eq!(source.container_calls.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn collector_projects_raw_inspect_before_cache_and_uses_only_server_targets() {
        run_async(async {
            let runner = FakeRuntimeCommand::new(vec![
                Ok(output(compose_inspects_json())),
                Ok(output(b"primary-id\nsidecar-id\nforeign-id\n")),
                Ok(output(primary_inspect_json())),
                Ok(output(b"primary-id\n")),
            ]);
            let query_runner = QueryRuntimeCommandRunner::new(
                Arc::new(runner.clone()),
                QUERY_DOCKER_COMMAND_TIMEOUT,
            );
            let source = SystemContainerQuerySource {
                docker: DockerCli::new(Arc::new(query_runner)),
            };

            let evidence = source
                .collect_containers(
                    WORKSPACE_ID,
                    DockerContainerLoadHint {
                        compose_project_name: Some(RAW_PROJECT_LABEL.to_owned()),
                        published_ports: Vec::new(),
                    },
                )
                .await
                .unwrap();
            let debug = format!("{evidence:?}");
            let serialized = serde_json::to_string(&evidence).unwrap();

            assert_eq!(evidence.containers.len(), 2);
            assert_eq!(evidence.published_ports.len(), 1);
            assert!(!debug.contains("foreign-id"));
            for forbidden in [
                HOST_PATH,
                RAW_CONFIG_HASH,
                RAW_PROJECT_LABEL,
                SECRET,
                RAW_STDERR,
            ] {
                assert!(!debug.contains(forbidden), "{debug}");
                assert!(!serialized.contains(forbidden), "{serialized}");
            }
            let commands = runner.commands();
            assert_eq!(commands.len(), 4);
            assert!(commands.iter().all(|command| {
                command.timeout_duration() == Some(QUERY_DOCKER_COMMAND_TIMEOUT)
            }));
            assert!(commands.iter().all(|command| {
                !command.args_vec().iter().any(|arg| {
                    arg.contains(HOST_PATH)
                        || arg.contains("client-resource")
                        || arg == "text"
                        || arg == "json"
                })
            }));
            assert!(commands.iter().any(|command| {
                command
                    .args_vec()
                    .contains(&format!("label=decune.workspace_id={WORKSPACE_ID}"))
            }));
            assert!(commands.iter().any(|command| {
                command.args_vec().contains(&format!(
                    "label=com.docker.compose.project={RAW_PROJECT_LABEL}"
                ))
            }));
        });
    }

    #[test]
    fn cache_key_and_typed_failures_exclude_paths_secrets_and_raw_stderr() {
        let context = test_context();
        let _production_coordinator = ContainerQueryCoordinator::new(context.clone());
        let key = QueryEvidenceKey::from_context(&context, QueryEvidenceKind::Containers);
        let key_debug = format!("{key:?}");
        let key_json = serde_json::to_string(&key).unwrap();
        let failure = serde_json::to_string(&QueryEvidenceFailure::Unavailable).unwrap();
        let evidence = QueryEvidence::Containers(container_evidence("safe-container"));
        let evidence_debug = format!("{evidence:?}");
        let evidence_json = serde_json::to_string(&evidence).unwrap();

        for forbidden in [
            HOST_PATH,
            RAW_CONFIG_HASH,
            RAW_PROJECT_LABEL,
            SECRET,
            RAW_STDERR,
        ] {
            assert!(!key_debug.contains(forbidden));
            assert!(!key_json.contains(forbidden));
            assert!(!failure.contains(forbidden));
            assert!(!evidence_debug.contains(forbidden));
            assert!(!evidence_json.contains(forbidden));
        }
        assert_eq!(
            serde_json::to_value(&key).unwrap(),
            json!({
                "query_context_fingerprint": context.context_fingerprint(),
                "workspace_id": WORKSPACE_ID,
                "kind": "containers",
            })
        );
    }

    fn test_cache<S>(
        source: Arc<S>,
        clock: Arc<FakeClock>,
        concurrent_loads: usize,
    ) -> QueryEvidenceCache
    where
        S: ContainerQuerySource + 'static,
    {
        QueryEvidenceCache::with_clock(
            source,
            clock,
            concurrent_loads,
            DOCKER_EVIDENCE_LOAD_TIMEOUT,
        )
    }

    fn key(workspace_id: &str, kind: QueryEvidenceKind) -> QueryEvidenceKey {
        QueryEvidenceKey {
            query_context_fingerprint: format!("{workspace_id:0<64}"),
            workspace_id: workspace_id.to_owned(),
            kind,
        }
    }

    fn load_for_kind(kind: QueryEvidenceKind) -> QueryEvidenceLoad {
        match kind {
            QueryEvidenceKind::Containers => container_load(),
            QueryEvidenceKind::Volumes => QueryEvidenceLoad::Volumes,
        }
    }

    fn container_load() -> QueryEvidenceLoad {
        QueryEvidenceLoad::Containers(DockerContainerLoadHint {
            compose_project_name: None,
            published_ports: Vec::new(),
        })
    }

    fn container_evidence(name: &str) -> ContainerQueryContainersEvidence {
        ContainerQueryContainersEvidence {
            containers: vec![ContainerQueryContainerEvidence::new(
                Some("container-id".to_owned()),
                Some(name.to_owned()),
                Some("app".to_owned()),
                RuntimeRunState::Running,
                HealthStatus::Healthy,
                Some(RAW_CONFIG_HASH),
            )],
            published_ports: Vec::new(),
        }
    }

    fn workspace_state() -> WorkspaceState {
        WorkspaceState {
            version: 1,
            workspace: HOST_PATH.to_owned(),
            mode: WorkspaceModeSnapshot::Compose,
            container_id: "container-id".to_owned(),
            image: SECRET.to_owned(),
            config_hash: RAW_CONFIG_HASH.to_owned(),
            config_file: Some(format!("{HOST_PATH}/.devcontainer/devcontainer.json")),
            compose_project_name: Some(RAW_PROJECT_LABEL.to_owned()),
            published_ports: Vec::new(),
            clone_isolation: CloneIsolationRuntimeState::default(),
            created_at: "2026-07-19T00:00:00Z".to_owned(),
            last_started_at: "2026-07-19T00:00:00Z".to_owned(),
            last_used_at: None,
            lifecycle: LifecycleState::all_completed(),
        }
    }

    fn test_context() -> ContainerCliQueryContext {
        match HostDaemonCliQueryPolicy::enabled_for_test(
            WORKSPACE_ID,
            std::path::PathBuf::from(format!("{HOST_PATH}/state")),
            std::path::PathBuf::from(format!("{HOST_PATH}/runtime")),
        ) {
            HostDaemonCliQueryPolicy::Enabled(context) => context,
            HostDaemonCliQueryPolicy::Disabled => unreachable!(),
        }
    }

    fn run_async(future: impl Future<Output = ()>) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future);
    }

    fn output(stdout: impl AsRef<[u8]>) -> RuntimeOutput {
        RuntimeOutput {
            stdout: stdout.as_ref().to_vec(),
            stderr: RAW_STDERR.as_bytes().to_vec(),
            exit_code: 0,
        }
    }

    fn primary_inspect_json() -> &'static [u8] {
        br#"[{
            "Id": "primary-id",
            "Name": "/project-app-1",
            "Config": {
                "Env": ["TOKEN=container-secret-marker"],
                "Labels": {
                    "decune.managed": "true",
                    "decune.workspace_id": "123456abcdef",
                    "decune.workspace": "/host/private/workspace",
                    "decune.config_hash": "raw-config-hash-secret-marker",
                    "com.docker.compose.project": "raw-project-label-secret-marker",
                    "com.docker.compose.service": "app"
                }
            },
            "Mounts": [{
                "Type": "bind",
                "Source": "/host/private/workspace",
                "Destination": "/workspace"
            }],
            "State": {
                "Running": true,
                "Health": { "Status": "healthy" }
            },
            "NetworkSettings": {
                "Ports": {
                    "8080/tcp": [{
                        "HostIp": "127.0.0.1",
                        "HostPort": "18080"
                    }]
                }
            }
        }]"#
    }

    fn compose_inspects_json() -> &'static [u8] {
        br#"[{
            "Id": "primary-id",
            "Name": "/project-app-1",
            "Config": {
                "Labels": {
                    "decune.managed": "true",
                    "decune.workspace_id": "123456abcdef",
                    "decune.config_hash": "raw-config-hash-secret-marker",
                    "com.docker.compose.project": "raw-project-label-secret-marker",
                    "com.docker.compose.service": "app"
                }
            },
            "State": { "Running": true }
        }, {
            "Id": "sidecar-id",
            "Name": "/project-db-1",
            "Config": {
                "Env": ["PASSWORD=container-secret-marker"],
                "Labels": {
                    "com.docker.compose.project": "raw-project-label-secret-marker",
                    "com.docker.compose.service": "db"
                }
            },
            "Mounts": [{
                "Type": "bind",
                "Source": "/host/private/workspace",
                "Destination": "/data"
            }],
            "State": { "Running": false }
        }, {
            "Id": "foreign-id",
            "Name": "/project-foreign-1",
            "Config": {
                "Labels": {
                    "decune.managed": "true",
                    "decune.workspace_id": "654321fedcba",
                    "com.docker.compose.project": "raw-project-label-secret-marker",
                    "com.docker.compose.service": "foreign"
                }
            },
            "State": { "Running": true },
            "NetworkSettings": {
                "Ports": {
                    "9999/tcp": [{
                        "HostIp": "127.0.0.1",
                        "HostPort": "19999"
                    }]
                }
            }
        }]"#
    }
}
