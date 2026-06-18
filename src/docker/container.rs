use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::{
    config::resolved::{ResolvedConfig, ResolvedRunArg},
    docker::{
        client::DockerClient, mounts::DockerMountSpec, ports::DockerPublishPort,
        resource::DockerResources,
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
    pub(crate) run_args: Vec<DockerRunArg>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DockerRunArg {
    pub(crate) option: String,
    pub(crate) value: String,
}

#[derive(Debug, Clone, PartialEq)]
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
    client.cli().create_container(spec).await
}

pub(crate) async fn start_container(client: &DockerClient, container: &str) -> Result<()> {
    client.cli().start_container(container).await
}

pub(crate) async fn inspect_container_env(
    client: &DockerClient,
    container: &str,
) -> Result<BTreeMap<String, String>> {
    let inspect = client
        .cli()
        .inspect_container(container)
        .await
        .with_context(|| format!("Failed to inspect Docker container environment: {container}"))?;
    let entries = inspect
        .config
        .and_then(|config| config.env)
        .unwrap_or_default();

    Ok(container_env_from_entries(entries))
}

pub(crate) async fn stop_container(
    client: &DockerClient,
    container: &str,
    timeout_seconds: i32,
) -> Result<()> {
    client
        .cli()
        .stop_container(container, timeout_seconds)
        .await
}

pub(crate) async fn remove_container(
    client: &DockerClient,
    container: &str,
    force: bool,
    remove_volumes: bool,
) -> Result<()> {
    client
        .cli()
        .remove_container(container, force, remove_volumes)
        .await
}

fn host_config_from_resolved(config: &ResolvedConfig) -> ContainerHostConfig {
    let mut host_config = ContainerHostConfig {
        init: config.devcontainer.init_enabled(),
        privileged: config.devcontainer.privileged_enabled(),
        cap_add: config.devcontainer.cap_add.clone(),
        security_opt: config.devcontainer.security_opt.clone(),
        ..ContainerHostConfig::default()
    };

    for run_arg in &config.devcontainer.run_args {
        match run_arg {
            ResolvedRunArg::AddHost(value) => host_config.extra_hosts.push(value.clone()),
            ResolvedRunArg::Dns(value) => host_config.dns.push(value.clone()),
            ResolvedRunArg::DnsSearch(value) => host_config.dns_search.push(value.clone()),
            ResolvedRunArg::Passthrough { option, value } => {
                host_config.run_args.push(DockerRunArg {
                    option: option.clone(),
                    value: value.clone(),
                });
            }
        }
    }

    host_config
}

fn container_env_from_entries(entries: Vec<String>) -> BTreeMap<String, String> {
    entries
        .into_iter()
        .filter_map(|entry| {
            let (key, value) = entry.split_once('=')?;
            Some((key.to_owned(), value.to_owned()))
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct ContainerInspect {
    pub(crate) id: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) image: Option<String>,
    pub(crate) config: Option<ContainerInspectConfig>,
    pub(crate) state: Option<ContainerState>,
    pub(crate) mounts: Option<Vec<ContainerMount>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct ContainerInspectConfig {
    pub(crate) env: Option<Vec<String>>,
    pub(crate) labels: Option<BTreeMap<String, String>>,
    pub(crate) user: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct ContainerState {
    pub(crate) running: Option<bool>,
    pub(crate) exit_code: Option<i64>,
    pub(crate) pid: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct ContainerMount {
    #[serde(rename = "Type")]
    pub(crate) typ: Option<String>,
    pub(crate) source: Option<String>,
    pub(crate) destination: Option<String>,
    #[serde(rename = "RW")]
    pub(crate) rw: Option<bool>,
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

    use super::{
        ContainerCreateInput, ContainerCreateSpec, ContainerHostConfig, create_container,
        devcontainer_keepalive_command, inspect_container_env, remove_container, start_container,
        stop_container,
    };

    #[test]
    fn container_env_from_entries_preserves_values_with_equals_and_last_value_wins() {
        let env = super::container_env_from_entries(vec![
            "PATH=/usr/bin".to_owned(),
            "TOKEN=prefix=value".to_owned(),
            "NO_EQUALS".to_owned(),
            "PATH=/usr/local/bin:/usr/bin".to_owned(),
        ]);

        assert_eq!(
            env.get("PATH").map(String::as_str),
            Some("/usr/local/bin:/usr/bin")
        );
        assert_eq!(env.get("TOKEN").map(String::as_str), Some("prefix=value"));
        assert!(!env.contains_key("NO_EQUALS"));
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
        config.devcontainer.init = Some(true);
        config.devcontainer.privileged = Some(true);
        config.devcontainer.cap_add = vec!["SYS_PTRACE".to_owned()];
        config.devcontainer.security_opt = vec!["seccomp=unconfined".to_owned()];
        config.devcontainer.run_args = vec![
            LayerRunArg::AddHost("host.docker.internal:host-gateway".to_owned()),
            LayerRunArg::Dns("1.1.1.1".to_owned()),
            LayerRunArg::DnsSearch("example.test".to_owned()),
            LayerRunArg::Passthrough {
                option: "--network".to_owned(),
                value: "host".to_owned(),
            },
            LayerRunArg::Passthrough {
                option: "--device".to_owned(),
                value: "/dev/fuse".to_owned(),
            },
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
                consistency: None,
                bind_options: None,
                volume_options: None,
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
        assert_eq!(
            spec.host_config.run_args,
            vec![
                super::DockerRunArg {
                    option: "--network".to_owned(),
                    value: "host".to_owned(),
                },
                super::DockerRunArg {
                    option: "--device".to_owned(),
                    value: "/dev/fuse".to_owned(),
                },
            ]
        );
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
    fn minimal_container_can_be_created_started_stopped_and_removed() {
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
                let inspect = client.cli().inspect_container(&name).await?;
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
    fn stop_container_ignores_non_zero_wait_status() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let client = DockerClient::connect_from_env().unwrap();
            let name = format!("decune-test-stop-nonzero-{}", std::process::id());
            let result: anyhow::Result<()> = async {
                ensure_image(&client, "alpine:3.20", PullPolicy::Missing).await?;
                remove_container(&client, &name, true, true).await?;

                let spec = ContainerCreateSpec {
                    image: "alpine:3.20".to_owned(),
                    name: name.clone(),
                    entrypoint: Some(vec!["/bin/sh".to_owned()]),
                    command: Some(vec![
                        "-c".to_owned(),
                        "trap 'exit 1' TERM\nwhile sleep 1 & wait $!; do :; done".to_owned(),
                    ]),
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

                create_container(&client, &spec).await?;
                start_container(&client, &name).await?;
                stop_container(&client, &name, 1).await?;

                Ok(())
            }
            .await;

            let cleanup = remove_container(&client, &name, true, true).await;
            result.and(cleanup).unwrap();
        });
    }

    #[test]
    fn workspace_container_label_filter_finds_only_managed_workspace() {
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
                    .cli()
                    .list_workspace_containers("target-workspace")
                    .await?;
                let listed_names = containers
                    .into_iter()
                    .map(|container| container.name)
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
    fn create_spec_mounts_read_only_bind_and_publishes_port() {
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
                        consistency: None,
                        bind_options: None,
                        volume_options: None,
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

                let container_env = inspect_container_env(&client, &name).await?;
                assert_eq!(
                    container_env.get("PATH").map(String::as_str),
                    Some("/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
                );

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
                        redactions: Vec::new(),
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
                        redactions: Vec::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_ne!(write_output.exit_code, 0);
                assert!(!host_directory.path().join("new.txt").exists());

                Ok(())
            }
            .await;

            let cleanup = remove_container(&client, &name, true, true).await;
            result.and(cleanup).unwrap();
        });
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
        names
            .iter()
            .any(|name| name == expected || name == &format!("/{expected}"))
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
}
