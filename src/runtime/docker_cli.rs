use std::{collections::BTreeMap, path::Path, sync::Arc};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::{
    config::types::{MountType, PortProtocol},
    docker::{
        container::{ContainerCreateSpec, ContainerInspect, DockerExecInspect},
        image::{DockerImageInspect, DockerImageSummary},
        mounts::DockerMountSpec,
        ports::DockerPublishPort,
    },
    runtime::command::{
        RuntimeCommand, RuntimeCommandRunner, RuntimeOutput, RuntimeStdio, TokioRuntimeCommand,
        ensure_success,
    },
    up::UpContainerSummary,
};

#[derive(Clone)]
pub(crate) struct DockerCli {
    runner: Arc<dyn RuntimeCommandRunner>,
}

impl Default for DockerCli {
    fn default() -> Self {
        Self::new(Arc::new(TokioRuntimeCommand))
    }
}

impl DockerCli {
    pub(crate) fn new(runner: Arc<dyn RuntimeCommandRunner>) -> Self {
        Self { runner }
    }

    #[allow(dead_code)]
    pub(crate) async fn ping(&self) -> Result<()> {
        self.run_ok("ping Docker daemon", "daemon", docker_cmd(["version"]))
            .await
    }

    #[allow(dead_code)]
    pub(crate) async fn version_json(&self) -> Result<serde_json::Value> {
        let output = self
            .run_json_command(
                "read Docker version",
                "daemon",
                docker_cmd(["version", "--format", "json"]),
            )
            .await?;
        Ok(output)
    }

    pub(crate) async fn build(&self, input: DockerBuildCliInput<'_>) -> Result<RuntimeOutput> {
        let target = input.image_tag;
        let command = docker_build_command(&input);
        let output = self.runner.run_capture(command.clone()).await?;
        ensure_success("build Docker image", target, &command, &output)?;
        Ok(output)
    }

    pub(crate) async fn pull(&self, image: &str) -> Result<RuntimeOutput> {
        let command = docker_cmd(["pull", image]);
        let output = self.runner.run_capture(command.clone()).await?;
        ensure_success("pull Docker image", image, &command, &output)?;
        Ok(output)
    }

    pub(crate) async fn tag(&self, source: &str, target: &str) -> Result<()> {
        self.run_ok(
            "tag Docker image",
            target,
            docker_cmd(["tag", source, target]),
        )
        .await
    }

    pub(crate) async fn remove_image(&self, image: &str, force: bool) -> Result<()> {
        let mut command = docker_cmd(["image", "rm", "--no-prune"]);
        if force {
            command = command.arg("--force");
        }
        command = command.arg(image);
        self.run_ok_or_not_found("remove Docker image", image, command)
            .await
    }

    pub(crate) async fn inspect_image(&self, image: &str) -> Result<DockerImageInspect> {
        let value = self
            .run_json_command(
                "inspect Docker image",
                image,
                docker_cmd(["image", "inspect", image]),
            )
            .await?;
        let mut images: Vec<DockerImageInspect> = serde_json::from_value(value)
            .with_context(|| format!("Failed to parse Docker image inspect output: {image}"))?;
        images
            .pop()
            .with_context(|| format!("Docker image inspect returned no images: {image}"))
    }

    pub(crate) async fn inspect_image_if_present(
        &self,
        image: &str,
    ) -> Result<Option<DockerImageInspect>> {
        match self.inspect_image(image).await {
            Ok(inspect) => Ok(Some(inspect)),
            Err(error) if error.to_string().contains("No such image") => Ok(None),
            Err(error) if error.to_string().contains("not found") => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub(crate) async fn list_images(&self, reference: &str) -> Result<Vec<DockerImageSummary>> {
        let value = self
            .run_json_lines_command(
                "list Docker images",
                reference,
                docker_cmd(["image", "ls", "--all", "--format", "json", reference]),
            )
            .await?;
        Ok(value)
    }

    pub(crate) async fn create_container(&self, spec: &ContainerCreateSpec) -> Result<String> {
        let command = docker_create_command(spec);
        let output = self.runner.run_capture(command.clone()).await?;
        ensure_success("create Docker container", &spec.name, &command, &output)?;
        let id = output.stdout_string()?.trim().to_owned();
        Ok(id)
    }

    pub(crate) async fn start_container(&self, container: &str) -> Result<()> {
        let command = docker_cmd(["start", container]);
        let output = self.runner.run_capture(command.clone()).await?;
        if output.exit_code == 0 || output.stderr_string_lossy().contains("already started") {
            Ok(())
        } else {
            ensure_success("start Docker container", container, &command, &output)
        }
    }

    pub(crate) async fn stop_container(&self, container: &str, timeout_seconds: i32) -> Result<()> {
        let command = docker_cmd(["stop", "--time", &timeout_seconds.to_string(), container]);
        let output = self.runner.run_capture(command.clone()).await?;
        if output.exit_code == 0 || is_not_found_or_not_running(&output) {
            Ok(())
        } else {
            ensure_success("stop Docker container", container, &command, &output)
        }
    }

    pub(crate) async fn remove_container(
        &self,
        container: &str,
        force: bool,
        remove_volumes: bool,
    ) -> Result<()> {
        let mut command = docker_cmd(["rm"]);
        if force {
            command = command.arg("--force");
        }
        if remove_volumes {
            command = command.arg("--volumes");
        }
        command = command.arg(container);
        self.run_ok_or_not_found("remove Docker container", container, command)
            .await
    }

    pub(crate) async fn wait_container(&self, container: &str) -> Result<i64> {
        let command = docker_cmd(["wait", container]);
        let output = self.runner.run_capture(command.clone()).await?;
        ensure_success("wait for Docker container", container, &command, &output)?;
        let text = output.stdout_string()?;
        text.trim().parse::<i64>().with_context(|| {
            format!("Failed to parse Docker container wait exit code: {container}")
        })
    }

    pub(crate) async fn inspect_container(&self, container: &str) -> Result<ContainerInspect> {
        let value = self
            .run_json_command(
                "inspect Docker container",
                container,
                docker_cmd(["container", "inspect", container]),
            )
            .await?;
        let mut containers: Vec<ContainerInspect> =
            serde_json::from_value(value).with_context(|| {
                format!("Failed to parse Docker container inspect output: {container}")
            })?;
        containers.pop().with_context(|| {
            format!("Docker container inspect returned no containers: {container}")
        })
    }

    pub(crate) async fn list_workspace_containers(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<UpContainerSummary>> {
        let command = docker_cmd([
            "ps",
            "--all",
            "--filter",
            "label=decune.managed=true",
            "--filter",
            &format!("label=decune.workspace_id={workspace_id}"),
            "--format",
            "json",
        ]);
        let values: Vec<DockerPsRow> = self
            .run_json_lines_command("list Docker containers", workspace_id, command)
            .await?;
        values
            .into_iter()
            .map(UpContainerSummary::try_from)
            .collect()
    }

    pub(crate) async fn exec_capture(
        &self,
        container: &str,
        spec: &crate::docker::exec::ExecCommandSpec,
    ) -> Result<crate::docker::exec::ExecOutput> {
        let command = docker_exec_command(container, spec, false, false);
        let output = self.runner.run_capture(command.clone()).await?;
        Ok(crate::docker::exec::ExecOutput {
            stdout: output.stdout,
            stderr: output.stderr,
            exit_code: i64::from(output.exit_code),
        })
    }

    pub(crate) async fn exec_detached(
        &self,
        container: &str,
        spec: &crate::docker::exec::ExecCommandSpec,
    ) -> Result<String> {
        let command = docker_exec_command(container, spec, false, true);
        let output = self.runner.run_capture(command.clone()).await?;
        ensure_success("start detached Docker exec", container, &command, &output)?;
        let inspect = self.inspect_container(container).await?;
        Ok(inspect
            .state
            .and_then(|state| state.pid)
            .unwrap_or_default()
            .to_string())
    }

    pub(crate) async fn exec_status(
        &self,
        container: &str,
        spec: &crate::docker::exec::ExecCommandSpec,
    ) -> Result<i64> {
        let command = docker_exec_command(container, spec, spec.tty, false);
        let status = self
            .runner
            .run_status(command, RuntimeStdio::Inherit)
            .await?;
        Ok(i64::from(status))
    }

    pub(crate) async fn exec_inspect(
        &self,
        _exec_id: &str,
        container: &str,
    ) -> Result<DockerExecInspect> {
        let inspect = self.inspect_container(container).await?;
        Ok(DockerExecInspect {
            running: inspect.state.as_ref().and_then(|state| state.running),
            exit_code: inspect.state.as_ref().and_then(|state| state.exit_code),
        })
    }

    pub(crate) async fn list_volumes(&self, workspace_id: &str) -> Result<Vec<String>> {
        let command = docker_cmd([
            "volume",
            "ls",
            "--filter",
            "label=decune.managed=true",
            "--filter",
            &format!("label=decune.workspace_id={workspace_id}"),
            "--format",
            "{{.Name}}",
        ]);
        let output = self.runner.run_capture(command.clone()).await?;
        ensure_success("list Docker volumes", workspace_id, &command, &output)?;
        Ok(output
            .stdout_string()?
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect())
    }

    pub(crate) async fn remove_volume(&self, volume: &str, force: bool) -> Result<()> {
        let mut command = docker_cmd(["volume", "rm"]);
        if force {
            command = command.arg("--force");
        }
        command = command.arg(volume);
        self.run_ok_or_not_found("remove Docker volume", volume, command)
            .await
    }

    async fn run_ok(&self, action: &str, target: &str, command: RuntimeCommand) -> Result<()> {
        let output = self.runner.run_capture(command.clone()).await?;
        ensure_success(action, target, &command, &output)
    }

    async fn run_ok_or_not_found(
        &self,
        action: &str,
        target: &str,
        command: RuntimeCommand,
    ) -> Result<()> {
        let output = self.runner.run_capture(command.clone()).await?;
        if output.exit_code == 0 || is_not_found(&output) {
            Ok(())
        } else {
            ensure_success(action, target, &command, &output)
        }
    }

    async fn run_json_command<T: for<'de> Deserialize<'de>>(
        &self,
        action: &str,
        target: &str,
        command: RuntimeCommand,
    ) -> Result<T> {
        let output = self.runner.run_capture(command.clone()).await?;
        ensure_success(action, target, &command, &output)?;
        serde_json::from_slice(&output.stdout).with_context(|| {
            format!(
                "Failed to parse JSON output for {action}: {target}. stderr: {}",
                command.redact_output(&output.stderr_string_lossy())
            )
        })
    }

    async fn run_json_lines_command<T: for<'de> Deserialize<'de>>(
        &self,
        action: &str,
        target: &str,
        command: RuntimeCommand,
    ) -> Result<Vec<T>> {
        let output = self.runner.run_capture(command.clone()).await?;
        ensure_success(action, target, &command, &output)?;
        let stdout = output.stdout_string()?;
        stdout
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                serde_json::from_str::<T>(line)
                    .with_context(|| format!("Failed to parse JSON line for {action}: {target}"))
            })
            .collect()
    }
}

pub(crate) struct DockerBuildCliInput<'a> {
    pub(crate) image_tag: &'a str,
    pub(crate) dockerfile: &'a Path,
    pub(crate) context_dir: &'a Path,
    pub(crate) labels: &'a BTreeMap<String, String>,
    pub(crate) build_args: &'a BTreeMap<String, String>,
    pub(crate) target: Option<&'a str>,
    pub(crate) cache_from: &'a [String],
    pub(crate) no_cache: bool,
    pub(crate) pull: bool,
}

pub(crate) fn docker_build_command(input: &DockerBuildCliInput<'_>) -> RuntimeCommand {
    let dockerfile = input.dockerfile.to_string_lossy().into_owned();
    let context_dir = input.context_dir.to_string_lossy().into_owned();
    let mut command = docker_cmd(["build", "--tag", input.image_tag, "--file"]).arg(dockerfile);
    command = command.arg("--rm").arg("--force-rm");
    if input.no_cache {
        command = command.arg("--no-cache");
    }
    if input.pull {
        command = command.arg("--pull");
    }
    for (key, value) in input.labels {
        command = command.arg("--label").arg(format!("{key}={value}"));
    }
    for (key, value) in input.build_args {
        command = command.arg("--build-arg").arg(format!("{key}={value}"));
    }
    if let Some(target) = input.target {
        command = command.arg("--target").arg(target);
    }
    for cache in input.cache_from {
        command = command.arg("--cache-from").arg(cache);
    }
    command.arg(context_dir)
}

pub(crate) fn docker_create_command(spec: &ContainerCreateSpec) -> RuntimeCommand {
    let mut command = docker_cmd(["create", "--name", &spec.name]);
    for (key, value) in &spec.labels {
        command = command.arg("--label").arg(format!("{key}={value}"));
    }
    for (key, value) in &spec.env {
        command = command.arg("--env").arg(format!("{key}={value}"));
    }
    if let Some(working_dir) = &spec.working_dir {
        command = command.arg("--workdir").arg(working_dir);
    }
    if let Some(user) = &spec.user {
        command = command.arg("--user").arg(user);
    }
    if let Some(entrypoint) = &spec.entrypoint {
        command = command.arg("--entrypoint").arg(entrypoint.join(" "));
    }
    command = add_host_config_args(command, spec);
    for mount in &spec.mounts {
        command = command.arg("--mount").arg(mount.to_cli_mount());
    }
    for publish in &spec.publish_ports {
        command = command.arg("--publish").arg(publish.to_cli_publish());
    }
    command = command.arg(&spec.image);
    if let Some(command_args) = &spec.command {
        command = command.args(command_args);
    }
    command
}

fn add_host_config_args(mut command: RuntimeCommand, spec: &ContainerCreateSpec) -> RuntimeCommand {
    if spec.host_config.init {
        command = command.arg("--init");
    }
    if spec.host_config.privileged {
        command = command.arg("--privileged");
    }
    for cap in &spec.host_config.cap_add {
        command = command.arg("--cap-add").arg(cap);
    }
    for opt in &spec.host_config.security_opt {
        command = command.arg("--security-opt").arg(opt);
    }
    for host in &spec.host_config.extra_hosts {
        command = command.arg("--add-host").arg(host);
    }
    for dns in &spec.host_config.dns {
        command = command.arg("--dns").arg(dns);
    }
    for dns_search in &spec.host_config.dns_search {
        command = command.arg("--dns-search").arg(dns_search);
    }
    command
}

fn docker_exec_command(
    container: &str,
    spec: &crate::docker::exec::ExecCommandSpec,
    interactive: bool,
    detached: bool,
) -> RuntimeCommand {
    let mut command = docker_cmd(["exec"]);
    if interactive {
        command = command.arg("--interactive").arg("--tty");
    }
    if detached {
        command = command.arg("--detach");
    }
    if let Some(user) = &spec.user {
        command = command.arg("--user").arg(user);
    }
    if let Some(working_dir) = &spec.working_dir {
        command = command.arg("--workdir").arg(working_dir);
    }
    for (key, value) in &spec.env {
        command = command.arg("--env").arg(format!("{key}={value}"));
    }
    command.arg(container).args(&spec.command)
}

fn docker_cmd<const N: usize>(args: [&str; N]) -> RuntimeCommand {
    RuntimeCommand::new("docker").args(args)
}

fn is_not_found(output: &RuntimeOutput) -> bool {
    let stderr = output.stderr_string_lossy().to_ascii_lowercase();
    stderr.contains("no such") || stderr.contains("not found")
}

fn is_not_found_or_not_running(output: &RuntimeOutput) -> bool {
    let stderr = output.stderr_string_lossy().to_ascii_lowercase();
    is_not_found(output) || stderr.contains("is not running")
}

#[derive(Debug, Deserialize)]
struct DockerPsRow {
    #[serde(rename = "ID")]
    id: String,
    #[serde(rename = "Names")]
    names: String,
    #[serde(rename = "ImageID")]
    image_id: Option<String>,
    #[serde(rename = "State")]
    state: String,
    #[serde(default, rename = "Labels")]
    labels: String,
}

impl TryFrom<DockerPsRow> for UpContainerSummary {
    type Error = anyhow::Error;

    fn try_from(row: DockerPsRow) -> Result<Self> {
        let config_hash = row
            .labels
            .split(',')
            .filter_map(|entry| entry.split_once('='))
            .find_map(|(key, value)| (key == "decune.config_hash").then(|| value.to_owned()));
        Ok(Self {
            id: row.id,
            name: row.names,
            image_id: row.image_id,
            config_hash,
            mounts: None,
            running: row.state == "running",
        })
    }
}

pub(crate) trait DockerMountCliExt {
    fn to_cli_mount(&self) -> String;
}

impl DockerMountCliExt for DockerMountSpec {
    fn to_cli_mount(&self) -> String {
        let mut fields = vec![
            format!("type={}", mount_type_value(self.mount_type)),
            format!("target={}", self.target),
        ];
        if let Some(source) = &self.source {
            fields.push(format!("source={source}"));
        }
        if self.read_only {
            fields.push("readonly".to_owned());
        }
        if let Some(consistency) = &self.consistency {
            fields.push(format!("consistency={consistency}"));
        }
        if let Some(bind_options) = &self.bind_options {
            if let Some(propagation) = bind_options.propagation {
                fields.push(format!("bind-propagation={}", propagation.as_str()));
            }
            if bind_options.create_mountpoint == Some(true) {
                fields.push("bind-create".to_owned());
            }
        }
        if let Some(volume_options) = &self.volume_options {
            if volume_options.no_copy == Some(true) {
                fields.push("volume-nocopy".to_owned());
            }
            if let Some(subpath) = &volume_options.subpath {
                fields.push(format!("volume-subpath={subpath}"));
            }
            if let Some(labels) = &volume_options.labels {
                for (key, value) in labels {
                    fields.push(format!("volume-label={key}={value}"));
                }
            }
            if let Some(driver_config) = &volume_options.driver_config {
                if let Some(name) = &driver_config.name {
                    fields.push(format!("volume-driver={name}"));
                }
                if let Some(options) = &driver_config.options {
                    for (key, value) in options {
                        fields.push(format!("volume-opt={key}={value}"));
                    }
                }
            }
        }
        fields.join(",")
    }
}

fn mount_type_value(mount_type: MountType) -> &'static str {
    match mount_type {
        MountType::Bind => "bind",
        MountType::Volume => "volume",
        MountType::Tmpfs => "tmpfs",
    }
}

pub(crate) trait DockerPublishCliExt {
    fn to_cli_publish(&self) -> String;
}

impl DockerPublishCliExt for DockerPublishPort {
    fn to_cli_publish(&self) -> String {
        let protocol = match self.protocol {
            PortProtocol::Tcp => "tcp",
            PortProtocol::Udp => "udp",
        };
        let mut value = String::new();
        if let Some(host_ip) = &self.host_ip {
            value.push_str(host_ip);
            value.push(':');
        }
        if let Some(host) = self.host {
            value.push_str(&host.to_string());
            value.push(':');
        }
        value.push_str(&self.container.to_string());
        value.push('/');
        value.push_str(protocol);
        value
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::Path};

    use crate::{
        config::types::{MountType, PortProtocol},
        docker::{
            container::{ContainerCreateSpec, ContainerHostConfig},
            mounts::DockerMountSpec,
            ports::DockerPublishPort,
        },
        runtime::docker_cli::{DockerBuildCliInput, docker_build_command, docker_create_command},
    };

    #[test]
    fn docker_build_command_uses_argv_for_dockerfile_plan() {
        let command = docker_build_command(&DockerBuildCliInput {
            image_tag: "decune/test:hash",
            dockerfile: Path::new("/work/Dockerfile"),
            context_dir: Path::new("/work"),
            labels: &BTreeMap::from([("decune.managed".to_owned(), "true".to_owned())]),
            build_args: &BTreeMap::from([("VARIANT".to_owned(), "bookworm".to_owned())]),
            target: Some("dev"),
            cache_from: &["type=registry,ref=example/cache:latest".to_owned()],
            no_cache: true,
            pull: true,
        });

        assert_eq!(command.program(), "docker");
        assert!(command.args_vec().contains(&"build".to_owned()));
        assert!(command.args_vec().contains(&"--build-arg".to_owned()));
        assert!(!command.sanitized_display().contains("sh -c"));
    }

    #[test]
    fn docker_create_command_maps_container_plan_to_argv() {
        let spec = ContainerCreateSpec {
            image: "alpine:3.20".to_owned(),
            name: "decune-test".to_owned(),
            entrypoint: Some(vec!["/bin/sh".to_owned()]),
            command: Some(vec!["-c".to_owned(), "sleep 1".to_owned()]),
            labels: BTreeMap::from([("decune.managed".to_owned(), "true".to_owned())]),
            env: BTreeMap::from([("WORKSPACE".to_owned(), "/workspaces/project".to_owned())]),
            working_dir: Some("/workspaces/project".to_owned()),
            user: Some("vscode".to_owned()),
            mounts: vec![DockerMountSpec {
                source: Some("/host/project".to_owned()),
                target: "/workspaces/project".to_owned(),
                mount_type: MountType::Bind,
                read_only: true,
                consistency: None,
                bind_options: None,
                volume_options: None,
            }],
            publish_ports: vec![DockerPublishPort {
                container: 8080,
                host: Some(18080),
                host_ip: Some("127.0.0.1".to_owned()),
                protocol: PortProtocol::Tcp,
            }],
            host_config: ContainerHostConfig {
                init: true,
                privileged: false,
                cap_add: vec!["SYS_PTRACE".to_owned()],
                security_opt: Vec::new(),
                extra_hosts: Vec::new(),
                dns: Vec::new(),
                dns_search: Vec::new(),
            },
        };

        let command = docker_create_command(&spec);

        assert_eq!(command.program(), "docker");
        assert_eq!(command.args_vec()[0], "create");
        assert!(command.args_vec().contains(&"--mount".to_owned()));
        assert!(command.args_vec().contains(&"--publish".to_owned()));
        assert!(!command.sanitized_display().contains("sh -c docker"));
    }
}
