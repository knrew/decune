use super::*;

pub(in crate::up) struct CredentialRuntime {
    _git_credentials: GitCredentialRuntime,
    _github_cli: GithubCliRuntime,
    _ssh_agent: SshAgentRuntime,
    _forward: ForwardRuntime,
    service_forward: Vec<ServiceForwardRuntime>,
    mount_policy: CredentialRuntimeMountPolicy,
}

impl CredentialRuntime {
    fn new(
        git_credentials: GitCredentialRuntime,
        github_cli: GithubCliRuntime,
        ssh_agent: SshAgentRuntime,
        forward: ForwardRuntime,
        service_forward: Vec<ServiceForwardRuntime>,
    ) -> Self {
        let required_mounts = git_credentials
            .mounts()
            .iter()
            .chain(github_cli.mounts())
            .chain(ssh_agent.mounts())
            .chain(forward.mounts())
            .map(|mount| UpMountSummary {
                source: mount.source.clone(),
                target: mount.target.clone(),
                mount_type: mount.mount_type,
                read_only: mount.read_only,
            })
            .collect();

        Self {
            _git_credentials: git_credentials,
            _github_cli: github_cli,
            _ssh_agent: ssh_agent,
            _forward: forward,
            service_forward,
            mount_policy: CredentialRuntimeMountPolicy::new(required_mounts),
        }
    }

    pub(in crate::up) const fn mount_policy(&self) -> &CredentialRuntimeMountPolicy {
        &self.mount_policy
    }

    pub(super) fn service_forward(&self) -> &[ServiceForwardRuntime] {
        &self.service_forward
    }
}

pub(super) async fn container_tool_platform_for_plan(
    client: &DockerClient,
    plan: &UpPlan,
    existing_container_image: Option<&str>,
) -> Result<ContainerToolPlatform> {
    let image = existing_container_image.unwrap_or(&plan.image);
    image_container_tool_platform(client, image).await
}

pub(super) fn add_credential_runtime_mounts(
    plan: UpPlan,
    runtime_dir: &Path,
    platform: ContainerToolPlatform,
) -> Result<(UpPlan, CredentialRuntime)> {
    let ssh_agent = prepare_ssh_agent_runtime(&plan.config)?;
    let github_cli = prepare_github_cli_runtime(&plan.config, runtime_dir)?;
    let forward = prepare_forward_runtime(&plan.forward_ports, runtime_dir, platform)?;
    let service_forward = prepare_service_forward_runtimes(
        &plan.forward_ports,
        primary_compose_service(&plan),
        runtime_dir,
        platform,
    )?;
    add_prepared_credential_runtime_mounts(
        plan,
        runtime_dir,
        github_cli,
        ssh_agent,
        forward,
        service_forward,
        platform,
    )
}

#[cfg(test)]
pub(in crate::up) fn add_credential_runtime_mounts_with_ssh_socket(
    plan: UpPlan,
    runtime_dir: &Path,
    ssh_auth_sock: Option<&Path>,
) -> Result<(UpPlan, CredentialRuntime)> {
    let platform = ContainerToolPlatform::LinuxAmd64;
    let ssh_agent = crate::host::credentials::prepare_ssh_agent_runtime_with_socket(
        &plan.config,
        ssh_auth_sock,
    )?;
    let github_cli = crate::host::credentials::prepare_github_cli_runtime_with_token(
        &plan.config,
        runtime_dir,
        None,
    )?;
    let forward = prepare_forward_runtime(&plan.forward_ports, runtime_dir, platform)?;
    let service_forward = prepare_service_forward_runtimes(
        &plan.forward_ports,
        primary_compose_service(&plan),
        runtime_dir,
        platform,
    )?;
    add_prepared_credential_runtime_mounts(
        plan,
        runtime_dir,
        github_cli,
        ssh_agent,
        forward,
        service_forward,
        platform,
    )
}

#[cfg(test)]
pub(in crate::up) fn add_credential_runtime_mounts_with_inputs(
    plan: UpPlan,
    runtime_dir: &Path,
    ssh_auth_sock: Option<&Path>,
    github_token: Option<&str>,
) -> Result<(UpPlan, CredentialRuntime)> {
    let platform = ContainerToolPlatform::LinuxAmd64;
    let ssh_agent = crate::host::credentials::prepare_ssh_agent_runtime_with_socket(
        &plan.config,
        ssh_auth_sock,
    )?;
    let github_cli = crate::host::credentials::prepare_github_cli_runtime_with_token(
        &plan.config,
        runtime_dir,
        github_token,
    )?;
    let forward = prepare_forward_runtime(&plan.forward_ports, runtime_dir, platform)?;
    let service_forward = prepare_service_forward_runtimes(
        &plan.forward_ports,
        primary_compose_service(&plan),
        runtime_dir,
        platform,
    )?;
    add_prepared_credential_runtime_mounts(
        plan,
        runtime_dir,
        github_cli,
        ssh_agent,
        forward,
        service_forward,
        platform,
    )
}

fn add_prepared_credential_runtime_mounts(
    mut plan: UpPlan,
    runtime_dir: &Path,
    github_cli: GithubCliRuntime,
    ssh_agent: SshAgentRuntime,
    forward: ForwardRuntime,
    service_forward: Vec<ServiceForwardRuntime>,
    platform: ContainerToolPlatform,
) -> Result<(UpPlan, CredentialRuntime)> {
    let git_credentials = prepare_git_credential_runtime(&plan.config, runtime_dir, platform)?;
    extend_runtime_mounts(&mut plan.mounts, git_credentials.mounts());
    extend_runtime_mounts(&mut plan.mounts, github_cli.mounts());
    extend_runtime_mounts(&mut plan.mounts, ssh_agent.mounts());
    extend_runtime_mounts(&mut plan.mounts, forward.mounts());
    plan.config
        .devcontainer
        .container_env
        .extend(github_cli.container_env().clone());
    plan.config
        .devcontainer
        .container_env
        .extend(ssh_agent.container_env().clone());
    prepare_feature_entrypoint_sentinel_runtime(&plan, runtime_dir)?;

    Ok((
        plan,
        CredentialRuntime::new(
            git_credentials,
            github_cli,
            ssh_agent,
            forward,
            service_forward,
        ),
    ))
}

fn extend_runtime_mounts(mounts: &mut Vec<DockerMountSpec>, runtime_mounts: &[DockerMountSpec]) {
    for mount in runtime_mounts {
        let target = normalize_container_path(&mount.target);
        if mounts
            .iter()
            .any(|existing| normalize_container_path(&existing.target) == target)
        {
            continue;
        }
        mounts.push(mount.clone());
    }
}

fn primary_compose_service(plan: &UpPlan) -> Option<&str> {
    match &plan.config.devcontainer.source {
        Some(ResolvedDevcontainerSource::Compose(compose)) => Some(&compose.service),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener;

    use super::{
        add_credential_runtime_mounts_with_inputs, add_credential_runtime_mounts_with_ssh_socket,
    };
    use crate::{
        config::{
            resolved::ResolvedConfig,
            types::{GitHttpsMode, GithubCredentialsMode, PortProtocol},
        },
        docker::ports::ResolvedForwardPort,
        host::credentials::{
            DECUNE_RUNTIME_TARGET, GITHUB_CLI_CONFIG_TARGET, GITHUB_CLI_TOKEN_TARGET,
            SSH_AGENT_SOCKET_TARGET,
        },
        up::{
            ExistingContainerDecision, UpContainerSummary,
            existing::decide_existing_container,
            start::generated_compose_override_content,
            test_support::{mount_summary, test_up_plan_with_config},
        },
    };

    #[test]
    fn credential_runtime_mounts_add_ssh_agent_without_hashing_socket_path() {
        let temp = tempfile::tempdir().unwrap();
        let socket_path_a = temp.path().join("agent-a.sock");
        let socket_path_b = temp.path().join("agent-b.sock");
        let _listener_a = UnixListener::bind(&socket_path_a).unwrap();
        let _listener_b = UnixListener::bind(&socket_path_b).unwrap();
        let runtime_dir = temp.path().join("runtime");
        let mut config = ResolvedConfig::default();
        config.credentials.git.https = GitHttpsMode::Off;
        let plan = test_up_plan_with_config(config);

        let (plan_a, _runtime_a) = add_credential_runtime_mounts_with_ssh_socket(
            plan.clone(),
            &runtime_dir,
            Some(&socket_path_a),
        )
        .unwrap();
        let (plan_b, _runtime_b) =
            add_credential_runtime_mounts_with_ssh_socket(plan, &runtime_dir, Some(&socket_path_b))
                .unwrap();

        assert_eq!(plan_a.resources.config_hash, "stable-hash");
        assert_eq!(plan_b.resources.config_hash, "stable-hash");
        assert_eq!(
            plan_a
                .config
                .devcontainer
                .container_env
                .get("SSH_AUTH_SOCK")
                .map(String::as_str),
            Some(SSH_AGENT_SOCKET_TARGET)
        );
        assert_eq!(
            plan_a
                .mounts
                .iter()
                .find(|mount| mount.target == SSH_AGENT_SOCKET_TARGET)
                .and_then(|mount| mount.source.as_deref()),
            socket_path_a.to_str()
        );
        assert_eq!(
            plan_b
                .mounts
                .iter()
                .find(|mount| mount.target == SSH_AGENT_SOCKET_TARGET)
                .and_then(|mount| mount.source.as_deref()),
            socket_path_b.to_str()
        );
    }
    #[test]
    fn credential_runtime_mounts_add_github_token_file_without_hashing_token_or_env() {
        let temp = tempfile::tempdir().unwrap();
        let runtime_dir = temp.path().join("runtime");
        let mut config = ResolvedConfig::default();
        config.credentials.git.https = GitHttpsMode::Off;
        let plan = test_up_plan_with_config(config);

        let (plan_a, _runtime_a) = add_credential_runtime_mounts_with_inputs(
            plan.clone(),
            &runtime_dir,
            None,
            Some("first-secret\n"),
        )
        .unwrap();
        let (plan_b, _runtime_b) = add_credential_runtime_mounts_with_inputs(
            plan,
            &runtime_dir,
            None,
            Some("second-secret\n"),
        )
        .unwrap();

        assert_eq!(plan_a.resources.config_hash, "stable-hash");
        assert_eq!(plan_b.resources.config_hash, "stable-hash");
        assert!(
            plan_a
                .config
                .devcontainer
                .container_env
                .values()
                .all(|value| !value.contains("first-secret"))
        );
        assert!(
            plan_a
                .resources
                .labels
                .values()
                .all(|value| !value.contains("first-secret"))
        );
        assert!(plan_a.mounts.iter().any(|mount| {
            mount.target == GITHUB_CLI_TOKEN_TARGET
                && mount
                    .source
                    .as_deref()
                    .is_some_and(|source| source.ends_with("secrets/github-token"))
                && mount.read_only
        }));
        assert_eq!(
            plan_a
                .config
                .devcontainer
                .container_env
                .get("GH_CONFIG_DIR")
                .map(String::as_str),
            Some(GITHUB_CLI_CONFIG_TARGET)
        );
        assert!(
            plan_a
                .mounts
                .iter()
                .any(|mount| mount.target == GITHUB_CLI_CONFIG_TARGET && !mount.read_only)
        );
    }
    #[test]
    fn credential_runtime_mounts_add_forward_agent_without_hashing_ports() {
        let temp = tempfile::tempdir().unwrap();
        let runtime_dir = temp.path().join("runtime");
        let mut config = ResolvedConfig::default();
        config.credentials.git.https = GitHttpsMode::Off;
        let mut plan = test_up_plan_with_config(config);
        plan.forward_ports = vec![ResolvedForwardPort {
            service: None,
            container: 4321,
            requested_host: 54321,
            host: 54321,
            host_ip: "127.0.0.1".to_owned(),
            protocol: PortProtocol::Tcp,
            require_local: false,
            label: None,
        }];

        let (plan, runtime) =
            add_credential_runtime_mounts_with_inputs(plan, &runtime_dir, None, None).unwrap();

        assert_eq!(plan.resources.config_hash, "stable-hash");
        assert!(runtime_dir.join("decune-forward-agent").is_file());
        assert!(plan.mounts.iter().any(|mount| {
            mount.target == DECUNE_RUNTIME_TARGET
                && mount.source.as_deref() == Some(runtime_dir.to_str().unwrap())
                && !mount.read_only
        }));
        assert!(
            runtime
                .mount_policy()
                .required_mounts()
                .iter()
                .any(|mount| mount.target == DECUNE_RUNTIME_TARGET)
        );
    }
    #[test]
    fn credential_runtime_mounts_add_forward_runtime_without_ports() {
        let temp = tempfile::tempdir().unwrap();
        let runtime_dir = temp.path().join("runtime");
        let mut config = ResolvedConfig::default();
        config.credentials.git.enabled = false;
        config.credentials.github.enabled = false;
        config.credentials.github.mode = GithubCredentialsMode::Off;
        let plan = test_up_plan_with_config(config);

        let (plan, runtime) =
            add_credential_runtime_mounts_with_inputs(plan, &runtime_dir, None, None).unwrap();

        assert_eq!(plan.resources.config_hash, "stable-hash");
        assert!(runtime_dir.join("decune-forward-agent").is_file());
        assert!(plan.mounts.iter().any(|mount| {
            mount.target == DECUNE_RUNTIME_TARGET
                && mount.source.as_deref() == Some(runtime_dir.to_str().unwrap())
                && !mount.read_only
        }));
        assert!(
            runtime
                .mount_policy()
                .required_mounts()
                .iter()
                .any(|mount| mount.target == DECUNE_RUNTIME_TARGET)
        );
    }
    #[test]
    fn compose_credentials_secret_leak_generated_override_injects_primary_runtime_mounts() {
        let temp = tempfile::tempdir().unwrap();
        let runtime_dir = temp.path().join("runtime");
        let socket_path = temp.path().join("agent.sock");
        let _listener = UnixListener::bind(&socket_path).unwrap();
        let mut config = ResolvedConfig::default();
        config
            .devcontainer
            .container_env
            .insert("APP_ENV".to_owned(), "compose-credentials-test".to_owned());
        let mut plan = test_up_plan_with_config(config);
        plan.forward_ports = vec![ResolvedForwardPort {
            service: None,
            container: 4321,
            requested_host: 54321,
            host: 54321,
            host_ip: "127.0.0.1".to_owned(),
            protocol: PortProtocol::Tcp,
            require_local: false,
            label: None,
        }];

        let (plan, _runtime) = add_credential_runtime_mounts_with_inputs(
            plan,
            &runtime_dir,
            Some(&socket_path),
            Some("compose-github-secret\n"),
        )
        .unwrap();
        let yaml = generated_compose_override_content("app", &plan).unwrap();

        assert!(yaml.contains("  'app':\n"));
        assert!(!yaml.contains("sidecar"));
        assert!(yaml.contains(DECUNE_RUNTIME_TARGET));
        assert!(yaml.contains(SSH_AGENT_SOCKET_TARGET));
        assert!(yaml.contains(GITHUB_CLI_TOKEN_TARGET));
        assert!(yaml.contains("read_only: true"));
        assert!(yaml.contains(GITHUB_CLI_CONFIG_TARGET));
        assert!(yaml.contains("type: tmpfs"));
        assert!(yaml.contains("'SSH_AUTH_SOCK': '/run/decune/ssh-agent.sock'"));
        assert!(yaml.contains("'GH_CONFIG_DIR': '/run/decune/gh'"));
        assert!(!yaml.contains("compose-github-secret"));
    }
    #[test]
    fn compose_credentials_generated_override_honors_disabled_credentials() {
        let temp = tempfile::tempdir().unwrap();
        let runtime_dir = temp.path().join("runtime");
        let socket_path = temp.path().join("agent.sock");
        let _listener = UnixListener::bind(&socket_path).unwrap();
        let mut config = ResolvedConfig::default();
        config.credentials.git.enabled = false;
        config.credentials.github.enabled = false;
        config.credentials.github.mode = GithubCredentialsMode::Off;
        let plan = test_up_plan_with_config(config);

        let (plan, _runtime) = add_credential_runtime_mounts_with_inputs(
            plan,
            &runtime_dir,
            Some(&socket_path),
            Some("disabled-github-secret\n"),
        )
        .unwrap();
        let yaml = generated_compose_override_content("app", &plan).unwrap();

        assert!(yaml.contains("  'app':\n"));
        assert!(yaml.contains(DECUNE_RUNTIME_TARGET));
        assert!(!yaml.contains(SSH_AGENT_SOCKET_TARGET));
        assert!(!yaml.contains(GITHUB_CLI_TOKEN_TARGET));
        assert!(!yaml.contains(GITHUB_CLI_CONFIG_TARGET));
        assert!(!yaml.contains("SSH_AUTH_SOCK"));
        assert!(!yaml.contains("GH_CONFIG_DIR"));
        assert!(!yaml.contains("disabled-github-secret"));
    }
    #[test]
    fn existing_container_decision_reuses_runtime_mount_when_forward_ports_are_added_later() {
        let runtime_dir = tempfile::tempdir().unwrap();
        let mut config = ResolvedConfig::default();
        config.credentials.git.enabled = false;
        config.credentials.github.enabled = false;
        config.credentials.github.mode = GithubCredentialsMode::Off;
        let mut plan = test_up_plan_with_config(config);
        plan.forward_ports = vec![ResolvedForwardPort {
            service: None,
            container: 4321,
            requested_host: 54321,
            host: 54321,
            host_ip: "127.0.0.1".to_owned(),
            protocol: PortProtocol::Tcp,
            require_local: false,
            label: None,
        }];
        let (_plan, runtime) =
            add_credential_runtime_mounts_with_inputs(plan, runtime_dir.path(), None, None)
                .unwrap();
        let container = UpContainerSummary {
            id: "container-id".to_owned(),
            name: "decune-project-abc123".to_owned(),
            image_id: None,
            config_hash: Some("stable-hash".to_owned()),
            config_file: None,
            mounts: Some(vec![mount_summary(
                runtime_dir.path().to_str(),
                DECUNE_RUNTIME_TARGET,
            )]),
            running: true,
        };

        let decision =
            decide_existing_container(&[container], "stable-hash", runtime.mount_policy(), false)
                .unwrap();

        assert_eq!(
            decision,
            ExistingContainerDecision::ReuseRunning {
                id: "container-id".to_owned(),
                name: "decune-project-abc123".to_owned()
            }
        );
    }
}
