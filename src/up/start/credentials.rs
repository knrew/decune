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

    pub(in crate::up) fn mount_policy(&self) -> &CredentialRuntimeMountPolicy {
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
