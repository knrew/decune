#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result, bail};
use bollard::{
    errors::Error as DockerError,
    models::{ContainerCreateBody, HostConfig, PortBinding, PortMap},
    query_parameters::{
        CreateContainerOptionsBuilder, ListContainersOptions, ListContainersOptionsBuilder,
        RemoveContainerOptionsBuilder, StartContainerOptionsBuilder, StopContainerOptionsBuilder,
        WaitContainerOptionsBuilder,
    },
};
use futures_util::TryStreamExt;

use crate::{
    config::resolved::{ResolvedConfig, ResolvedRunArg},
    docker::{
        client::DockerClient,
        mounts::DockerMountSpec,
        ports::DockerPublishPort,
        resource::{DockerResources, managed_workspace_label_filters},
    },
};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ContainerHostConfig {
    pub(crate) init: bool,
    pub(crate) privileged: bool,
    pub(crate) cap_add: Vec<String>,
    pub(crate) security_opt: Vec<String>,
    pub(crate) extra_hosts: Vec<String>,
    pub(crate) dns: Vec<String>,
    pub(crate) dns_search: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContainerCreateSpec {
    pub(crate) image: String,
    pub(crate) name: String,
    pub(crate) entrypoint: Option<Vec<String>>,
    pub(crate) command: Option<Vec<String>>,
    pub(crate) labels: BTreeMap<String, String>,
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) working_dir: Option<String>,
    pub(crate) user: Option<String>,
    pub(crate) mounts: Vec<DockerMountSpec>,
    pub(crate) publish_ports: Vec<DockerPublishPort>,
    pub(crate) host_config: ContainerHostConfig,
}

#[derive(Debug, Clone)]
pub(crate) struct ContainerCreateInput<'a> {
    pub(crate) image: &'a str,
    pub(crate) resources: &'a DockerResources,
    pub(crate) config: &'a ResolvedConfig,
    pub(crate) entrypoint: Option<Vec<String>>,
    pub(crate) command: Option<Vec<String>>,
    pub(crate) working_dir: Option<String>,
    pub(crate) mounts: Vec<DockerMountSpec>,
}

impl ContainerCreateSpec {
    pub(crate) fn from_resolved(input: ContainerCreateInput<'_>) -> Self {
        Self {
            image: input.image.to_owned(),
            name: input.resources.container_name.clone(),
            entrypoint: input.entrypoint,
            command: input.command,
            labels: input.resources.labels.clone(),
            env: input.config.devcontainer.container_env.clone(),
            working_dir: input.working_dir,
            user: input.config.devcontainer.container_user.clone(),
            mounts: input.mounts,
            publish_ports: input
                .config
                .devcontainer
                .publish_ports
                .iter()
                .map(|port| DockerPublishPort {
                    container: port.container,
                    host: port.host,
                    host_ip: port.host_ip.clone(),
                    protocol: port.protocol,
                })
                .collect(),
            host_config: host_config_from_resolved(input.config),
        }
    }
}

pub(crate) fn create_container_body(spec: &ContainerCreateSpec) -> ContainerCreateBody {
    let publish_port_keys = publish_port_keys(&spec.publish_ports);

    ContainerCreateBody {
        image: Some(spec.image.clone()),
        entrypoint: spec.entrypoint.clone(),
        cmd: spec.command.clone(),
        labels: non_empty_map(spec.labels.clone()),
        env: non_empty_vec(env_entries(&spec.env)),
        working_dir: spec.working_dir.clone(),
        user: spec.user.clone(),
        exposed_ports: non_empty_vec(publish_port_keys),
        host_config: Some(create_host_config(spec)),
        ..Default::default()
    }
}

pub(crate) fn devcontainer_keepalive_command() -> (Vec<String>, Vec<String>) {
    (
        vec!["/bin/sh".to_owned()],
        vec![
            "-c".to_owned(),
            "trap 'exit 0' TERM\nwhile sleep 1 & wait $!; do :; done".to_owned(),
        ],
    )
}

pub(crate) async fn create_container(
    client: &DockerClient,
    spec: &ContainerCreateSpec,
) -> Result<String> {
    let options = CreateContainerOptionsBuilder::default()
        .name(&spec.name)
        .build();
    let body = create_container_body(spec);

    let response = client
        .raw()
        .create_container(Some(options), body)
        .await
        .with_context(|| format!("Failed to create Docker container: {}", spec.name))?;

    Ok(response.id)
}

pub(crate) async fn start_container(client: &DockerClient, container: &str) -> Result<()> {
    let options = StartContainerOptionsBuilder::default().build();

    match client.raw().start_container(container, Some(options)).await {
        Ok(()) => Ok(()),
        Err(error) if is_container_already_started(&error) => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("Failed to start Docker container: {container}"))
        }
    }
}

pub(crate) async fn stop_container(
    client: &DockerClient,
    container: &str,
    timeout_seconds: i32,
) -> Result<()> {
    let options = StopContainerOptionsBuilder::default()
        .t(timeout_seconds)
        .build();

    match client.raw().stop_container(container, Some(options)).await {
        Ok(()) => wait_until_container_stopped(client, container).await,
        Err(error) if is_container_not_found(&error) || is_container_already_stopped(&error) => {
            Ok(())
        }
        Err(error) => {
            Err(error).with_context(|| format!("Failed to stop Docker container: {container}"))
        }
    }
}

async fn wait_until_container_stopped(client: &DockerClient, container: &str) -> Result<()> {
    let options = WaitContainerOptionsBuilder::default()
        .condition("not-running")
        .build();
    if client
        .raw()
        .wait_container(container, Some(options))
        .try_next()
        .await
        .with_context(|| format!("Failed to wait for Docker container to stop: {container}"))?
        .is_none()
    {
        bail!("Docker container stop wait ended without a response: {container}");
    }

    Ok(())
}

pub(crate) async fn remove_container(
    client: &DockerClient,
    container: &str,
    force: bool,
    remove_volumes: bool,
) -> Result<()> {
    let options = RemoveContainerOptionsBuilder::default()
        .force(force)
        .v(remove_volumes)
        .build();

    match client
        .raw()
        .remove_container(container, Some(options))
        .await
    {
        Ok(()) => Ok(()),
        Err(error) if is_container_not_found(&error) => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("Failed to remove Docker container: {container}"))
        }
    }
}

fn create_host_config(spec: &ContainerCreateSpec) -> HostConfig {
    HostConfig {
        init: spec.host_config.init.then_some(true),
        privileged: spec.host_config.privileged.then_some(true),
        cap_add: non_empty_vec(spec.host_config.cap_add.clone()),
        security_opt: non_empty_vec(spec.host_config.security_opt.clone()),
        extra_hosts: non_empty_vec(spec.host_config.extra_hosts.clone()),
        dns: non_empty_vec(spec.host_config.dns.clone()),
        dns_search: non_empty_vec(spec.host_config.dns_search.clone()),
        mounts: non_empty_vec(
            spec.mounts
                .iter()
                .map(DockerMountSpec::to_bollard_mount)
                .collect(),
        ),
        port_bindings: publish_port_bindings(&spec.publish_ports),
        ..Default::default()
    }
}

fn host_config_from_resolved(config: &ResolvedConfig) -> ContainerHostConfig {
    let mut host_config = ContainerHostConfig {
        init: config.devcontainer.init,
        privileged: config.devcontainer.privileged,
        cap_add: config.devcontainer.cap_add.clone(),
        security_opt: config.devcontainer.security_opt.clone(),
        ..ContainerHostConfig::default()
    };

    for run_arg in &config.devcontainer.run_args {
        match run_arg {
            ResolvedRunArg::AddHost(value) => host_config.extra_hosts.push(value.clone()),
            ResolvedRunArg::Dns(value) => host_config.dns.push(value.clone()),
            ResolvedRunArg::DnsSearch(value) => host_config.dns_search.push(value.clone()),
        }
    }

    host_config
}

fn env_entries(env: &BTreeMap<String, String>) -> Vec<String> {
    env.iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect()
}

fn publish_port_keys(publish_ports: &[DockerPublishPort]) -> Vec<String> {
    publish_ports.iter().map(DockerPublishPort::key).collect()
}

fn publish_port_bindings(publish_ports: &[DockerPublishPort]) -> Option<PortMap> {
    let mut bindings = PortMap::new();

    for publish_port in publish_ports {
        let binding = PortBinding {
            host_ip: publish_port.host_ip.clone(),
            host_port: publish_port.host.map(|port| port.to_string()),
        };

        bindings
            .entry(publish_port.key())
            .or_insert_with(|| Some(Vec::new()))
            .get_or_insert_with(Vec::new)
            .push(binding);
    }

    non_empty_map(bindings)
}

fn non_empty_vec<T>(values: Vec<T>) -> Option<Vec<T>> {
    if values.is_empty() {
        None
    } else {
        Some(values)
    }
}

fn non_empty_map<K, V>(values: impl IntoIterator<Item = (K, V)>) -> Option<HashMap<K, V>>
where
    K: Eq + std::hash::Hash,
{
    let values = values.into_iter().collect::<HashMap<_, _>>();

    if values.is_empty() {
        None
    } else {
        Some(values)
    }
}

fn is_container_not_found(error: &DockerError) -> bool {
    docker_status_code(error) == Some(404)
}

fn is_container_already_started(error: &DockerError) -> bool {
    docker_status_code(error) == Some(304)
}

fn is_container_already_stopped(error: &DockerError) -> bool {
    docker_status_code(error) == Some(304)
}

fn docker_status_code(error: &DockerError) -> Option<u16> {
    match error {
        DockerError::DockerResponseServerError { status_code, .. } => Some(*status_code),
        _ => None,
    }
}

pub(crate) fn workspace_container_list_options(workspace_id: &str) -> ListContainersOptions {
    let filters = managed_workspace_label_filters(workspace_id)
        .into_iter()
        .collect::<HashMap<_, _>>();

    ListContainersOptionsBuilder::new()
        .all(true)
        .filters(&filters)
        .build()
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs};

    use crate::{
        config::{
            layer::{LayerPublishPort, LayerRunArg},
            resolved::ResolvedConfig,
            types::{MountType, PortProtocol},
        },
        docker::{
            client::DockerClient,
            exec::{ExecCommandSpec, exec_capture, exec_capture_output},
            image::{PullPolicy, ensure_image},
            mounts::DockerMountSpec,
            ports::DockerPublishPort,
            resource::DockerResources,
        },
        workspace::Workspace,
    };

    use super::workspace_container_list_options;
    use super::{
        ContainerCreateInput, ContainerCreateSpec, ContainerHostConfig, create_container,
        create_container_body, devcontainer_keepalive_command, is_container_already_started,
        is_container_already_stopped, is_container_not_found, remove_container, start_container,
        stop_container,
    };

    #[test]
    fn workspace_container_list_options_searches_only_managed_workspace_containers() {
        let options = workspace_container_list_options("abc123def456");
        let filters = options.filters.unwrap();

        assert!(options.all);
        assert_eq!(
            filters.get("label"),
            Some(&vec![
                "decune.managed=true".to_owned(),
                "decune.workspace_id=abc123def456".to_owned(),
            ])
        );
    }

    #[test]
    fn create_spec_from_resolved_config_maps_devcontainer_runtime_fields() {
        let workspace = test_workspace("resolved-create-spec");
        let resources = DockerResources::from_workspace(
            &workspace,
            "config123",
            workspace
                .root()
                .join(".devcontainer/devcontainer.json")
                .display()
                .to_string(),
        );
        let mut config = ResolvedConfig::default();
        config.devcontainer.container_env = BTreeMap::from([
            ("RUST_LOG".to_owned(), "debug".to_owned()),
            ("WORKSPACE".to_owned(), "/workspaces/project".to_owned()),
        ]);
        config.devcontainer.container_user = Some("vscode".to_owned());
        config.devcontainer.publish_ports = vec![LayerPublishPort {
            container: 8080,
            host: Some(18080),
            host_ip: Some("127.0.0.1".to_owned()),
            protocol: PortProtocol::Tcp,
        }];
        config.devcontainer.init = true;
        config.devcontainer.privileged = true;
        config.devcontainer.cap_add = vec!["SYS_PTRACE".to_owned()];
        config.devcontainer.security_opt = vec!["seccomp=unconfined".to_owned()];
        config.devcontainer.run_args = vec![
            LayerRunArg::AddHost("host.docker.internal:host-gateway".to_owned()),
            LayerRunArg::Dns("1.1.1.1".to_owned()),
            LayerRunArg::DnsSearch("example.test".to_owned()),
        ];

        let spec = ContainerCreateSpec::from_resolved(ContainerCreateInput {
            image: "alpine:3.20",
            resources: &resources,
            config: &config,
            entrypoint: Some(vec!["/bin/sh".to_owned()]),
            command: Some(vec!["-c".to_owned(), "sleep 60".to_owned()]),
            working_dir: Some("/workspaces/project".to_owned()),
            mounts: vec![DockerMountSpec {
                source: Some("/host/project".to_owned()),
                target: "/workspaces/project".to_owned(),
                mount_type: MountType::Bind,
                read_only: false,
            }],
        });

        assert_eq!(spec.image, "alpine:3.20");
        assert_eq!(spec.name, resources.container_name);
        assert_eq!(spec.labels, resources.labels);
        assert_eq!(spec.env, config.devcontainer.container_env);
        assert_eq!(spec.user.as_deref(), Some("vscode"));
        assert_eq!(spec.working_dir.as_deref(), Some("/workspaces/project"));
        assert_eq!(spec.mounts.len(), 1);
        assert_eq!(
            spec.publish_ports,
            vec![DockerPublishPort {
                container: 8080,
                host: Some(18080),
                host_ip: Some("127.0.0.1".to_owned()),
                protocol: PortProtocol::Tcp,
            }]
        );
        assert!(spec.host_config.init);
        assert!(spec.host_config.privileged);
        assert_eq!(spec.host_config.cap_add, vec!["SYS_PTRACE"]);
        assert_eq!(spec.host_config.security_opt, vec!["seccomp=unconfined"]);
        assert_eq!(
            spec.host_config.extra_hosts,
            vec!["host.docker.internal:host-gateway"]
        );
        assert_eq!(spec.host_config.dns, vec!["1.1.1.1"]);
        assert_eq!(spec.host_config.dns_search, vec!["example.test"]);
    }

    #[test]
    fn create_spec_from_default_resolved_config_omits_optional_runtime_fields() {
        let workspace = test_workspace("default-create-spec");
        let resources = DockerResources::from_workspace(
            &workspace,
            "config123",
            workspace
                .root()
                .join(".devcontainer/devcontainer.json")
                .display()
                .to_string(),
        );

        let spec = ContainerCreateSpec::from_resolved(ContainerCreateInput {
            image: "alpine:3.20",
            resources: &resources,
            config: &ResolvedConfig::default(),
            entrypoint: None,
            command: None,
            working_dir: None,
            mounts: Vec::new(),
        });

        assert!(spec.env.is_empty());
        assert!(spec.user.is_none());
        assert!(spec.working_dir.is_none());
        assert!(spec.mounts.is_empty());
        assert!(spec.publish_ports.is_empty());
        assert_eq!(spec.host_config, ContainerHostConfig::default());
    }

    #[test]
    fn create_container_body_includes_container_config_and_host_config() {
        let labels = BTreeMap::from([
            ("decune.managed".to_owned(), "true".to_owned()),
            ("decune.workspace_id".to_owned(), "abc123def456".to_owned()),
        ]);
        let spec = ContainerCreateSpec {
            image: "alpine:latest".to_owned(),
            name: "decune-project-abc123def456".to_owned(),
            entrypoint: Some(vec!["/bin/sh".to_owned()]),
            command: Some(vec![
                "-c".to_owned(),
                "trap 'exit 0' TERM\nwhile sleep 1 & wait $!; do :; done".to_owned(),
            ]),
            labels: labels.clone(),
            env: BTreeMap::from([("WORKSPACE".to_owned(), "/workspaces/project".to_owned())]),
            working_dir: Some("/workspaces/project".to_owned()),
            user: Some("vscode".to_owned()),
            mounts: vec![DockerMountSpec {
                source: Some("/host/project".to_owned()),
                target: "/workspaces/project".to_owned(),
                mount_type: MountType::Bind,
                read_only: true,
            }],
            publish_ports: vec![DockerPublishPort {
                container: 8080,
                host: Some(18080),
                host_ip: Some("127.0.0.1".to_owned()),
                protocol: PortProtocol::Tcp,
            }],
            host_config: ContainerHostConfig {
                init: true,
                privileged: true,
                cap_add: vec!["SYS_PTRACE".to_owned()],
                security_opt: vec!["seccomp=unconfined".to_owned()],
                extra_hosts: vec!["host.docker.internal:host-gateway".to_owned()],
                dns: vec!["1.1.1.1".to_owned()],
                dns_search: vec!["example.test".to_owned()],
            },
        };

        let body = create_container_body(&spec);
        let host_config = body.host_config.unwrap();

        assert_eq!(body.image.as_deref(), Some("alpine:latest"));
        assert_eq!(body.entrypoint, Some(vec!["/bin/sh".to_owned()]));
        assert_eq!(
            body.cmd,
            Some(vec![
                "-c".to_owned(),
                "trap 'exit 0' TERM\nwhile sleep 1 & wait $!; do :; done".to_owned()
            ])
        );
        assert_eq!(body.labels, Some(labels.into_iter().collect()));
        assert_eq!(
            body.env,
            Some(vec!["WORKSPACE=/workspaces/project".to_owned()])
        );
        assert_eq!(body.working_dir.as_deref(), Some("/workspaces/project"));
        assert_eq!(body.user.as_deref(), Some("vscode"));
        assert_eq!(body.exposed_ports, Some(vec!["8080/tcp".to_owned()]));
        assert_eq!(host_config.init, Some(true));
        assert_eq!(host_config.privileged, Some(true));
        assert_eq!(host_config.cap_add, Some(vec!["SYS_PTRACE".to_owned()]));
        assert_eq!(
            host_config.security_opt,
            Some(vec!["seccomp=unconfined".to_owned()])
        );
        assert_eq!(
            host_config.extra_hosts,
            Some(vec!["host.docker.internal:host-gateway".to_owned()])
        );
        assert_eq!(host_config.dns, Some(vec!["1.1.1.1".to_owned()]));
        assert_eq!(
            host_config.dns_search,
            Some(vec!["example.test".to_owned()])
        );
        assert_eq!(
            host_config.mounts.as_ref().unwrap()[0].read_only,
            Some(true)
        );
        assert_eq!(
            host_config
                .port_bindings
                .unwrap()
                .get("8080/tcp")
                .unwrap()
                .as_ref()
                .unwrap()[0]
                .host_port
                .as_deref(),
            Some("18080")
        );
    }

    #[test]
    fn create_container_body_preserves_multiple_bindings_for_same_container_port() {
        let spec = ContainerCreateSpec {
            image: "alpine:latest".to_owned(),
            name: "decune-project-abc123def456".to_owned(),
            entrypoint: None,
            command: None,
            labels: BTreeMap::new(),
            env: BTreeMap::new(),
            working_dir: None,
            user: None,
            mounts: Vec::new(),
            publish_ports: vec![
                DockerPublishPort {
                    container: 80,
                    host: Some(8080),
                    host_ip: Some("127.0.0.1".to_owned()),
                    protocol: PortProtocol::Tcp,
                },
                DockerPublishPort {
                    container: 80,
                    host: Some(9090),
                    host_ip: Some("0.0.0.0".to_owned()),
                    protocol: PortProtocol::Tcp,
                },
            ],
            host_config: ContainerHostConfig::default(),
        };

        let body = create_container_body(&spec);
        let host_config = body.host_config.unwrap();
        let port_bindings = host_config.port_bindings.unwrap();
        let bindings = port_bindings.get("80/tcp").unwrap().as_ref().unwrap();

        assert_eq!(bindings.len(), 2);
        assert_eq!(bindings[0].host_port.as_deref(), Some("8080"));
        assert_eq!(bindings[0].host_ip.as_deref(), Some("127.0.0.1"));
        assert_eq!(bindings[1].host_port.as_deref(), Some("9090"));
        assert_eq!(bindings[1].host_ip.as_deref(), Some("0.0.0.0"));
    }

    #[test]
    fn devcontainer_keepalive_command_exits_promptly_on_term() {
        let (entrypoint, command) = devcontainer_keepalive_command();

        assert_eq!(entrypoint, vec!["/bin/sh"]);
        assert_eq!(command.len(), 2);
        assert_eq!(command[0], "-c");

        let script = &command[1];
        assert!(script.contains("trap 'exit 0' TERM"));
        assert!(script.contains("sleep 1 & wait $!"));
        assert!(!script.contains("sleep 1000"));
    }

    #[test]
    fn container_lifecycle_error_classification_treats_expected_states_as_idempotent() {
        let not_found = docker_server_error(404, "No such container");
        let not_modified = docker_server_error(304, "container already in requested state");
        let conflict = docker_server_error(409, "container conflict");

        assert!(is_container_not_found(&not_found));
        assert!(is_container_already_started(&not_modified));
        assert!(is_container_already_stopped(&not_modified));
        assert!(!is_container_not_found(&conflict));
        assert!(!is_container_already_started(&conflict));
        assert!(!is_container_already_stopped(&conflict));
    }

    #[test]
    fn minimal_container_can_be_created_started_stopped_and_removed_when_docker_tests_are_enabled()
    {
        if !docker_tests_enabled() {
            eprintln!("skipped: set DECUNE_DOCKER_TESTS=1 to run Docker integration tests");
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let client = DockerClient::connect_from_env().unwrap();
            let name = format!("decune-test-container-{}", std::process::id());
            let result: anyhow::Result<()> = async {
                ensure_image(&client, "alpine:3.20", PullPolicy::Missing).await?;
                remove_container(&client, &name, true, true).await?;

                let (entrypoint, command) = devcontainer_keepalive_command();
                let spec = ContainerCreateSpec {
                    image: "alpine:3.20".to_owned(),
                    name: name.clone(),
                    entrypoint: Some(entrypoint),
                    command: Some(command),
                    labels: BTreeMap::from([
                        ("decune.managed".to_owned(), "true".to_owned()),
                        ("decune.workspace_id".to_owned(), "testworkspace".to_owned()),
                    ]),
                    env: BTreeMap::new(),
                    working_dir: None,
                    user: None,
                    mounts: Vec::new(),
                    publish_ports: Vec::new(),
                    host_config: ContainerHostConfig::default(),
                };

                let id = create_container(&client, &spec).await?;
                assert!(!id.is_empty());

                start_container(&client, &name).await?;
                start_container(&client, &name).await?;
                let inspect = client.raw().inspect_container(&name, None).await?;
                assert_eq!(inspect.state.and_then(|state| state.running), Some(true));
                stop_container(&client, &name, 1).await?;
                stop_container(&client, &name, 1).await?;
                remove_container(&client, &name, true, true).await?;
                remove_container(&client, &name, true, true).await?;

                Ok(())
            }
            .await;

            let cleanup = remove_container(&client, &name, true, true).await;
            result.and(cleanup).unwrap();
        });
    }

    #[test]
    fn workspace_container_label_filter_finds_only_managed_workspace_when_docker_tests_are_enabled()
    {
        if !docker_tests_enabled() {
            eprintln!("skipped: set DECUNE_DOCKER_TESTS=1 to run Docker integration tests");
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let client = DockerClient::connect_from_env().unwrap();
            let suffix = std::process::id();
            let matching = format!("decune-test-label-matching-{suffix}");
            let other_workspace = format!("decune-test-label-other-{suffix}");
            let unmanaged = format!("decune-test-label-unmanaged-{suffix}");
            let names = [&matching, &other_workspace, &unmanaged];

            let result = async {
                ensure_image(&client, "alpine:3.20", PullPolicy::Missing).await?;
                for name in names {
                    remove_container(&client, name, true, true).await?;
                }

                create_container(
                    &client,
                    &label_filter_test_spec(
                        &matching,
                        BTreeMap::from([
                            ("decune.managed".to_owned(), "true".to_owned()),
                            (
                                "decune.workspace_id".to_owned(),
                                "target-workspace".to_owned(),
                            ),
                        ]),
                    ),
                )
                .await?;
                create_container(
                    &client,
                    &label_filter_test_spec(
                        &other_workspace,
                        BTreeMap::from([
                            ("decune.managed".to_owned(), "true".to_owned()),
                            (
                                "decune.workspace_id".to_owned(),
                                "other-workspace".to_owned(),
                            ),
                        ]),
                    ),
                )
                .await?;
                create_container(
                    &client,
                    &label_filter_test_spec(
                        &unmanaged,
                        BTreeMap::from([(
                            "decune.workspace_id".to_owned(),
                            "target-workspace".to_owned(),
                        )]),
                    ),
                )
                .await?;

                let containers = client
                    .raw()
                    .list_containers(Some(workspace_container_list_options("target-workspace")))
                    .await?;
                let listed_names = containers
                    .into_iter()
                    .flat_map(|container| container.names.unwrap_or_default())
                    .collect::<Vec<_>>();

                assert!(docker_names_contain(&listed_names, &matching));
                assert!(!docker_names_contain(&listed_names, &other_workspace));
                assert!(!docker_names_contain(&listed_names, &unmanaged));

                Ok(())
            }
            .await;

            let cleanup: anyhow::Result<()> = async {
                for name in names {
                    remove_container(&client, name, true, true).await?;
                }
                Ok(())
            }
            .await;
            result.and(cleanup).unwrap();
        });
    }

    #[test]
    fn create_spec_mounts_read_only_bind_and_publishes_port_when_docker_tests_are_enabled() {
        if !docker_tests_enabled() {
            eprintln!("skipped: set DECUNE_DOCKER_TESTS=1 to run Docker integration tests");
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let client = DockerClient::connect_from_env().unwrap();
            let name = format!("decune-test-create-spec-{}", std::process::id());
            let host_directory = tempfile::tempdir().unwrap();
            let host_file = host_directory.path().join("message.txt");
            fs::write(&host_file, "mounted from host\n").unwrap();

            let result = async {
                ensure_image(&client, "alpine:3.20", PullPolicy::Missing).await?;
                remove_container(&client, &name, true, true).await?;

                let (entrypoint, command) = devcontainer_keepalive_command();
                let spec = ContainerCreateSpec {
                    image: "alpine:3.20".to_owned(),
                    name: name.clone(),
                    entrypoint: Some(entrypoint),
                    command: Some(command),
                    labels: BTreeMap::from([
                        ("decune.managed".to_owned(), "true".to_owned()),
                        ("decune.workspace_id".to_owned(), "testworkspace".to_owned()),
                    ]),
                    env: BTreeMap::new(),
                    working_dir: None,
                    user: None,
                    mounts: vec![DockerMountSpec {
                        source: Some(host_directory.path().display().to_string()),
                        target: "/mnt/decune-test".to_owned(),
                        mount_type: MountType::Bind,
                        read_only: true,
                    }],
                    publish_ports: vec![DockerPublishPort {
                        container: 8080,
                        host: None,
                        host_ip: Some("127.0.0.1".to_owned()),
                        protocol: PortProtocol::Tcp,
                    }],
                    host_config: ContainerHostConfig::default(),
                };

                create_container(&client, &spec).await?;
                start_container(&client, &name).await?;

                let read_output = exec_capture(
                    &client,
                    &name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "cat /mnt/decune-test/message.txt".to_owned(),
                        ],
                        user: None,
                        working_dir: None,
                        env: BTreeMap::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(
                    String::from_utf8(read_output.stdout).unwrap(),
                    "mounted from host\n"
                );

                let write_output = exec_capture_output(
                    &client,
                    &name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "echo denied > /mnt/decune-test/new.txt".to_owned(),
                        ],
                        user: None,
                        working_dir: None,
                        env: BTreeMap::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_ne!(write_output.exit_code, 0);
                assert!(!host_directory.path().join("new.txt").exists());

                let inspect = client.raw().inspect_container(&name, None).await?;
                let ports = inspect
                    .network_settings
                    .and_then(|settings| settings.ports)
                    .unwrap_or_default();
                let bindings = ports
                    .get("8080/tcp")
                    .and_then(|bindings| bindings.as_ref())
                    .expect("expected Docker to allocate a host port for 8080/tcp");
                let binding = bindings
                    .first()
                    .expect("expected at least one published port binding");

                assert_eq!(binding.host_ip.as_deref(), Some("127.0.0.1"));
                assert!(
                    binding
                        .host_port
                        .as_deref()
                        .is_some_and(|port| !port.is_empty())
                );

                Ok(())
            }
            .await;

            let cleanup = remove_container(&client, &name, true, true).await;
            result.and(cleanup).unwrap();
        });
    }

    fn docker_server_error(status_code: u16, message: &str) -> bollard::errors::Error {
        bollard::errors::Error::DockerResponseServerError {
            status_code,
            message: message.to_owned(),
        }
    }

    fn label_filter_test_spec(name: &str, labels: BTreeMap<String, String>) -> ContainerCreateSpec {
        ContainerCreateSpec {
            image: "alpine:3.20".to_owned(),
            name: name.to_owned(),
            entrypoint: Some(vec!["/bin/sh".to_owned()]),
            command: Some(vec!["-c".to_owned(), "sleep 60".to_owned()]),
            labels,
            env: BTreeMap::new(),
            working_dir: None,
            user: None,
            mounts: Vec::new(),
            publish_ports: Vec::new(),
            host_config: ContainerHostConfig::default(),
        }
    }

    fn docker_names_contain(names: &[String], expected: &str) -> bool {
        let expected = format!("/{expected}");
        names.iter().any(|name| name == &expected)
    }

    fn test_workspace(name: &str) -> Workspace {
        let directory = tempfile::Builder::new()
            .prefix("decune-container-test-")
            .tempdir()
            .unwrap();
        let root = directory.path().join(name);
        fs::create_dir_all(&root).unwrap();
        Workspace::resolve(&root).unwrap()
    }

    fn docker_tests_enabled() -> bool {
        std::env::var_os("DECUNE_DOCKER_TESTS").is_some_and(|value| value == "1")
    }
}
