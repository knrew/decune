use std::{collections::BTreeMap, path::Path, sync::Arc};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::{
    config::types::{MountType, PortProtocol},
    docker::{
        container::{ContainerCreateSpec, ContainerInspect},
        image::{DockerImageInspect, DockerImageSummary},
        mounts::DockerMountSpec,
        ports::DockerPublishPort,
    },
    runtime::command::{
        RuntimeCommand, RuntimeCommandRunner, RuntimeOutput, RuntimeStdio, TokioRuntimeCommand,
        ensure_success,
    },
    terminal,
    up::{UpContainerSummary, UpMountSummary},
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
        let output = self
            .runner
            .run_capture_with_stdin(command.clone(), input.context_tar.to_vec())
            .await?;
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
        self.list_workspace_containers_with_filters(workspace_id, &[])
            .await
    }

    pub(crate) async fn list_compose_service_containers(
        &self,
        workspace_id: &str,
        project_name: &str,
        service: &str,
    ) -> Result<Vec<UpContainerSummary>> {
        self.list_workspace_containers_with_filters(
            workspace_id,
            &[
                format!("label=com.docker.compose.project={project_name}"),
                format!("label=com.docker.compose.service={service}"),
            ],
        )
        .await
    }

    pub(crate) async fn list_compose_project_containers(
        &self,
        workspace_id: &str,
        project_name: &str,
    ) -> Result<Vec<UpContainerSummary>> {
        self.list_workspace_containers_with_filters(
            workspace_id,
            &[format!("label=com.docker.compose.project={project_name}")],
        )
        .await
    }

    async fn list_workspace_containers_with_filters(
        &self,
        workspace_id: &str,
        extra_filters: &[String],
    ) -> Result<Vec<UpContainerSummary>> {
        let mut command = docker_cmd(["ps", "--all"])
            .arg("--filter")
            .arg("label=decune.managed=true")
            .arg("--filter")
            .arg(format!("label=decune.workspace_id={workspace_id}"));
        for filter in extra_filters {
            command = command.arg("--filter").arg(filter);
        }
        command = command.arg("--format").arg("json");
        let values: Vec<DockerPsRow> = self
            .run_json_lines_command("list Docker containers", workspace_id, command)
            .await?;
        let ids = values.into_iter().map(|row| row.id).collect::<Vec<_>>();
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        let command = docker_cmd(["container", "inspect"]).args(ids.iter().map(String::as_str));
        let containers: Vec<ContainerInspect> = self
            .run_json_command("inspect Docker containers", workspace_id, command)
            .await?;
        containers
            .into_iter()
            .map(up_container_summary_from_inspect)
            .collect()
    }

    pub(crate) async fn exec_capture(
        &self,
        container: &str,
        spec: &crate::docker::exec::ExecCommandSpec,
    ) -> Result<crate::docker::exec::ExecOutput> {
        let command = docker_exec_command(container, spec, false, false, false);
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
        let command = docker_exec_command(container, spec, false, false, true);
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
        let command = docker_exec_command(
            container,
            spec,
            true,
            spec.tty && terminal::stdin_is_tty(),
            false,
        );
        let status = self
            .runner
            .run_status(command, RuntimeStdio::Inherit)
            .await?;
        Ok(i64::from(status))
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
    pub(crate) context_tar: &'a [u8],
    pub(crate) labels: &'a BTreeMap<String, String>,
    pub(crate) build_args: &'a BTreeMap<String, String>,
    pub(crate) target: Option<&'a str>,
    pub(crate) cache_from: &'a [String],
    pub(crate) no_cache: bool,
    pub(crate) pull: bool,
}

pub(crate) fn docker_build_command(input: &DockerBuildCliInput<'_>) -> RuntimeCommand {
    let dockerfile = input.dockerfile.to_string_lossy().into_owned();
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
        command = command
            .env(key.clone(), value.clone())
            .arg("--build-arg")
            .arg(key.as_str());
    }
    if let Some(target) = input.target {
        command = command.arg("--target").arg(target);
    }
    for cache in input.cache_from {
        command = command.arg("--cache-from").arg(cache);
    }
    command.arg("-")
}

pub(crate) fn docker_create_command(spec: &ContainerCreateSpec) -> RuntimeCommand {
    let mut command = docker_cmd(["create", "--name", &spec.name]);
    for (key, value) in &spec.labels {
        command = command.arg("--label").arg(format!("{key}={value}"));
    }
    for (key, value) in &spec.env {
        command = command
            .env(key.clone(), value.clone())
            .arg("--env")
            .arg(key.as_str());
    }
    if let Some(working_dir) = &spec.working_dir {
        command = command.arg("--workdir").arg(working_dir);
    }
    if let Some(user) = &spec.user {
        command = command.arg("--user").arg(user);
    }
    let mut entrypoint_args = Vec::new();
    if let Some((entrypoint, args)) = spec
        .entrypoint
        .as_ref()
        .and_then(|entrypoint| entrypoint.split_first())
    {
        command = command.arg("--entrypoint").arg(entrypoint);
        entrypoint_args.extend(args.iter().cloned());
    }
    command = add_host_config_args(command, spec);
    for mount in &spec.mounts {
        command = command.arg("--mount").arg(mount.to_cli_mount());
    }
    for publish in &spec.publish_ports {
        command = command.arg("--publish").arg(publish.to_cli_publish());
    }
    command = command.arg(&spec.image);
    command = command.args(entrypoint_args);
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
    tty: bool,
    detached: bool,
) -> RuntimeCommand {
    let mut command = docker_cmd(["exec"]);
    if interactive {
        command = command.arg("--interactive");
    }
    if tty {
        command = command.arg("--tty");
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
        command = command
            .env(key.clone(), value.clone())
            .arg("--env")
            .arg(key.as_str());
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
}

fn up_container_summary_from_inspect(container: ContainerInspect) -> Result<UpContainerSummary> {
    let id = container
        .id
        .context("Docker container inspect output was missing Id")?;
    let name = container
        .name
        .map(|name| name.trim_start_matches('/').to_owned())
        .unwrap_or_else(|| id.clone());
    let labels = container
        .config
        .as_ref()
        .and_then(|config| config.labels.as_ref());
    let config_hash = labels
        .and_then(|labels| labels.get("decune.config_hash"))
        .cloned();
    let config_file = labels
        .and_then(|labels| labels.get("devcontainer.config_file"))
        .cloned();
    let mounts = container.mounts.map(|mounts| {
        mounts
            .into_iter()
            .filter_map(up_mount_summary_from_inspect)
            .collect()
    });
    let running = container
        .state
        .as_ref()
        .and_then(|state| state.running)
        .unwrap_or(false);

    Ok(UpContainerSummary {
        id,
        name,
        image_id: container.image,
        config_hash,
        config_file,
        mounts,
        running,
    })
}

fn up_mount_summary_from_inspect(
    mount: crate::docker::container::ContainerMount,
) -> Option<UpMountSummary> {
    let mount_type = mount_type_from_summary(mount.typ.as_deref())?;
    Some(UpMountSummary {
        source: mount.source,
        target: mount.destination?,
        mount_type,
        read_only: !mount.rw.unwrap_or(true),
    })
}

fn mount_type_from_summary(value: Option<&str>) -> Option<MountType> {
    match value {
        Some("bind") => Some(MountType::Bind),
        Some("volume") => Some(MountType::Volume),
        Some("tmpfs") => Some(MountType::Tmpfs),
        _ => None,
    }
}

pub(crate) trait DockerMountCliExt {
    fn to_cli_mount(&self) -> String;
}

impl DockerMountCliExt for DockerMountSpec {
    fn to_cli_mount(&self) -> String {
        let mut fields = vec![
            mount_field("type", mount_type_value(self.mount_type)),
            mount_field("target", &self.target),
        ];
        if let Some(source) = &self.source {
            fields.push(mount_field("source", source));
        }
        if self.read_only {
            fields.push("readonly".to_owned());
        }
        if let Some(consistency) = &self.consistency {
            fields.push(mount_field("consistency", consistency));
        }
        if let Some(bind_options) = &self.bind_options
            && let Some(propagation) = bind_options.propagation
        {
            fields.push(mount_field("bind-propagation", propagation.as_str()));
        }
        if let Some(volume_options) = &self.volume_options {
            if volume_options.no_copy == Some(true) {
                fields.push("volume-nocopy".to_owned());
            }
            if let Some(subpath) = &volume_options.subpath {
                fields.push(mount_field("volume-subpath", subpath));
            }
            if let Some(labels) = &volume_options.labels {
                for (key, value) in labels {
                    fields.push(mount_field("volume-label", &format!("{key}={value}")));
                }
            }
            if let Some(driver_config) = &volume_options.driver_config {
                if let Some(name) = &driver_config.name {
                    fields.push(mount_field("volume-driver", name));
                }
                if let Some(options) = &driver_config.options {
                    for (key, value) in options {
                        fields.push(mount_field("volume-opt", &format!("{key}={value}")));
                    }
                }
            }
        }
        fields.join(",")
    }
}

fn mount_field(key: &str, value: &str) -> String {
    quote_mount_csv_field(&format!("{key}={value}"))
}

fn quote_mount_csv_field(field: &str) -> String {
    if !field.contains([',', '"', '\n', '\r']) {
        return field.to_owned();
    }

    let mut quoted = String::with_capacity(field.len() + 2);
    quoted.push('"');
    for character in field.chars() {
        if character == '"' {
            quoted.push('"');
        }
        quoted.push(character);
    }
    quoted.push('"');
    quoted
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
        let mut value = match (&self.host_ip, self.host) {
            (None, None) => self.container.to_string(),
            (None, Some(host)) => format!("{host}:{}", self.container),
            (Some(host_ip), None) => {
                format!("{}::{}", docker_publish_host_ip(host_ip), self.container)
            }
            (Some(host_ip), Some(host)) => {
                format!(
                    "{}:{host}:{}",
                    docker_publish_host_ip(host_ip),
                    self.container
                )
            }
        };
        value.push('/');
        value.push_str(protocol);
        value
    }
}

fn docker_publish_host_ip(host_ip: &str) -> String {
    if host_ip.contains(':') && !(host_ip.starts_with('[') && host_ip.ends_with(']')) {
        format!("[{host_ip}]")
    } else {
        host_ip.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::Path, sync::Arc};

    use crate::{
        config::types::{MountType, PortProtocol},
        docker::{
            container::{ContainerCreateSpec, ContainerHostConfig},
            mounts::{DockerMountSpec, MountBindOptions, MountVolumeOptions},
            ports::DockerPublishPort,
        },
        runtime::{
            command::{FakeRuntimeCommand, RuntimeOutput},
            docker_cli::{
                DockerBuildCliInput, DockerCli, DockerMountCliExt, DockerPublishCliExt,
                docker_build_command, docker_create_command, docker_exec_command,
            },
        },
    };

    #[test]
    fn docker_build_command_uses_argv_for_dockerfile_plan() {
        let command = docker_build_command(&DockerBuildCliInput {
            image_tag: "decune/test:hash",
            dockerfile: Path::new("/work/Dockerfile"),
            context_tar: b"",
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
    fn docker_build_command_keeps_build_arg_values_out_of_argv() {
        let command = docker_build_command(&DockerBuildCliInput {
            image_tag: "decune/test:hash",
            dockerfile: Path::new("/work/Dockerfile"),
            context_tar: b"",
            labels: &BTreeMap::new(),
            build_args: &BTreeMap::from([("TOKEN".to_owned(), "test-secret".to_owned())]),
            target: None,
            cache_from: &[],
            no_cache: false,
            pull: false,
        });

        assert!(
            !command
                .args_vec()
                .iter()
                .any(|arg| arg.contains("test-secret"))
        );
        assert!(!command.sanitized_display().contains("test-secret"));
        assert!(
            command
                .args_vec()
                .windows(2)
                .any(|args| { args[0] == "--build-arg" && args[1] == "TOKEN" })
        );
        assert_eq!(
            command.env_value("TOKEN").map(String::as_str),
            Some("test-secret")
        );
    }

    #[test]
    fn docker_build_command_uses_stdin_context_arg_for_tar_context() {
        let command = docker_build_command(&DockerBuildCliInput {
            image_tag: "decune/test:hash",
            dockerfile: Path::new("docker/Dockerfile"),
            context_tar: b"tar-bytes",
            labels: &BTreeMap::new(),
            build_args: &BTreeMap::new(),
            target: None,
            cache_from: &[],
            no_cache: false,
            pull: false,
        });

        assert_eq!(arg_after(&command, "--file"), Some("docker/Dockerfile"));
        assert_eq!(command.args_vec().last().map(String::as_str), Some("-"));
    }

    #[test]
    fn docker_build_sends_tar_context_to_stdin() {
        let runner = FakeRuntimeCommand::new(vec![Ok(output(b"built\n"))]);
        let client = DockerCli::new(Arc::new(runner.clone()));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime
            .block_on(client.build(DockerBuildCliInput {
                image_tag: "decune/test:hash",
                dockerfile: Path::new("Dockerfile"),
                context_tar: b"tar-bytes",
                labels: &BTreeMap::new(),
                build_args: &BTreeMap::new(),
                target: None,
                cache_from: &[],
                no_cache: false,
                pull: false,
            }))
            .unwrap();

        assert_eq!(runner.stdin(), vec![Some(b"tar-bytes".to_vec())]);
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
        assert_eq!(arg_after(&command, "--entrypoint"), Some("/bin/sh"));
        assert_eq!(
            arg_after(&command, "--publish"),
            Some("127.0.0.1:18080:8080/tcp")
        );
        assert!(!command.sanitized_display().contains("sh -c docker"));
    }

    #[test]
    fn docker_create_command_keeps_env_values_out_of_argv() {
        let spec = ContainerCreateSpec {
            image: "alpine:3.20".to_owned(),
            name: "decune-test".to_owned(),
            entrypoint: None,
            command: None,
            labels: BTreeMap::new(),
            env: BTreeMap::from([("API_TOKEN".to_owned(), "test-secret".to_owned())]),
            working_dir: None,
            user: None,
            mounts: Vec::new(),
            publish_ports: Vec::new(),
            host_config: ContainerHostConfig::default(),
        };

        let command = docker_create_command(&spec);

        assert!(
            !command
                .args_vec()
                .iter()
                .any(|arg| arg.contains("test-secret"))
        );
        assert!(!command.sanitized_display().contains("test-secret"));
        assert!(
            command
                .args_vec()
                .windows(2)
                .any(|args| { args[0] == "--env" && args[1] == "API_TOKEN" })
        );
        assert_eq!(
            command.env_value("API_TOKEN").map(String::as_str),
            Some("test-secret")
        );
    }

    #[test]
    fn docker_create_command_maps_multi_element_entrypoint_to_cli_shape() {
        let spec = ContainerCreateSpec {
            image: "alpine:3.20".to_owned(),
            name: "decune-test".to_owned(),
            entrypoint: Some(vec!["/bin/sh".to_owned(), "-c".to_owned()]),
            command: Some(vec!["echo ok".to_owned()]),
            labels: BTreeMap::new(),
            env: BTreeMap::new(),
            working_dir: None,
            user: None,
            mounts: Vec::new(),
            publish_ports: Vec::new(),
            host_config: ContainerHostConfig::default(),
        };

        let command = docker_create_command(&spec);

        assert_eq!(arg_after(&command, "--entrypoint"), Some("/bin/sh"));
        assert_eq!(
            command.args_vec(),
            [
                "create",
                "--name",
                "decune-test",
                "--entrypoint",
                "/bin/sh",
                "alpine:3.20",
                "-c",
                "echo ok"
            ]
        );
    }

    #[test]
    fn docker_exec_command_keeps_interactive_and_tty_independent() {
        let spec = crate::docker::exec::ExecCommandSpec {
            command: vec!["/bin/sh".to_owned()],
            user: Some("vscode".to_owned()),
            working_dir: Some("/workspace".to_owned()),
            env: BTreeMap::from([("TERM".to_owned(), "xterm".to_owned())]),
            tty: true,
        };

        let non_tty_attached = docker_exec_command("container", &spec, true, false, false);
        assert!(
            non_tty_attached
                .args_vec()
                .contains(&"--interactive".to_owned())
        );
        assert!(!non_tty_attached.args_vec().contains(&"--tty".to_owned()));

        let tty_attached = docker_exec_command("container", &spec, true, true, false);
        assert!(
            tty_attached
                .args_vec()
                .contains(&"--interactive".to_owned())
        );
        assert!(tty_attached.args_vec().contains(&"--tty".to_owned()));

        let captured = docker_exec_command("container", &spec, false, false, false);
        assert!(!captured.args_vec().contains(&"--interactive".to_owned()));
        assert!(!captured.args_vec().contains(&"--tty".to_owned()));
    }

    #[test]
    fn docker_exec_command_keeps_env_values_out_of_argv() {
        let spec = crate::docker::exec::ExecCommandSpec {
            command: vec!["/run/decune/decune-forward-agent".to_owned()],
            user: Some("0".to_owned()),
            working_dir: None,
            env: BTreeMap::from([
                (
                    "DECUNE_FORWARD_AGENT_ALLOWED_PORTS".to_owned(),
                    "4321".to_owned(),
                ),
                (
                    "DECUNE_FORWARD_AGENT_SECRET".to_owned(),
                    "test-secret".to_owned(),
                ),
            ]),
            tty: false,
        };

        let command = docker_exec_command("container", &spec, false, false, true);

        assert!(
            !command
                .args_vec()
                .iter()
                .any(|arg| arg.contains("test-secret"))
        );
        assert!(
            command
                .args_vec()
                .windows(2)
                .any(|args| { args[0] == "--env" && args[1] == "DECUNE_FORWARD_AGENT_SECRET" })
        );
        assert_eq!(
            command
                .env_value("DECUNE_FORWARD_AGENT_SECRET")
                .map(String::as_str),
            Some("test-secret")
        );
    }

    #[test]
    fn docker_publish_format_handles_host_and_host_ip_combinations() {
        assert_eq!(
            DockerPublishPort {
                container: 8080,
                host: None,
                host_ip: None,
                protocol: PortProtocol::Tcp,
            }
            .to_cli_publish(),
            "8080/tcp"
        );
        assert_eq!(
            DockerPublishPort {
                container: 8080,
                host: Some(18080),
                host_ip: None,
                protocol: PortProtocol::Tcp,
            }
            .to_cli_publish(),
            "18080:8080/tcp"
        );
        assert_eq!(
            DockerPublishPort {
                container: 8080,
                host: None,
                host_ip: Some("127.0.0.1".to_owned()),
                protocol: PortProtocol::Tcp,
            }
            .to_cli_publish(),
            "127.0.0.1::8080/tcp"
        );
        assert_eq!(
            DockerPublishPort {
                container: 8080,
                host: Some(18080),
                host_ip: Some("::1".to_owned()),
                protocol: PortProtocol::Udp,
            }
            .to_cli_publish(),
            "[::1]:18080:8080/udp"
        );
    }

    #[test]
    fn docker_create_command_does_not_emit_unsupported_bind_create_field() {
        let spec = ContainerCreateSpec {
            image: "alpine:3.20".to_owned(),
            name: "decune-test".to_owned(),
            entrypoint: None,
            command: None,
            labels: BTreeMap::new(),
            env: BTreeMap::new(),
            working_dir: None,
            user: None,
            mounts: vec![DockerMountSpec {
                source: Some("/host/generated/cache".to_owned()),
                target: "/cache".to_owned(),
                mount_type: MountType::Bind,
                read_only: false,
                consistency: None,
                bind_options: Some(MountBindOptions {
                    create_mountpoint: Some(true),
                    ..MountBindOptions::default()
                }),
                volume_options: None,
            }],
            publish_ports: Vec::new(),
            host_config: ContainerHostConfig::default(),
        };

        let command = docker_create_command(&spec);

        let mount = command
            .args_vec()
            .windows(2)
            .find_map(|args| (args[0] == "--mount").then_some(args[1].as_str()))
            .expect("expected --mount argument");
        assert!(!mount.contains("bind-create"));
    }

    #[test]
    fn docker_mount_cli_format_quotes_csv_fields() {
        let mount = DockerMountSpec {
            source: Some(r#"/host/work,one/"quoted""#.to_owned()),
            target: "/workspaces/project".to_owned(),
            mount_type: MountType::Bind,
            read_only: true,
            consistency: Some("cached".to_owned()),
            bind_options: None,
            volume_options: None,
        };

        assert_eq!(
            mount.to_cli_mount(),
            r#"type=bind,target=/workspaces/project,"source=/host/work,one/""quoted""",readonly,consistency=cached"#
        );
    }

    #[test]
    fn docker_mount_cli_format_quotes_volume_option_fields() {
        let mount = DockerMountSpec {
            source: Some("project-cache".to_owned()),
            target: "/cache".to_owned(),
            mount_type: MountType::Volume,
            read_only: false,
            consistency: None,
            bind_options: None,
            volume_options: Some(MountVolumeOptions {
                subpath: Some("deps,with,commas".to_owned()),
                ..MountVolumeOptions::default()
            }),
        };

        assert_eq!(
            mount.to_cli_mount(),
            r#"type=volume,target=/cache,source=project-cache,"volume-subpath=deps,with,commas""#
        );
    }

    fn arg_after<'a>(
        command: &'a crate::runtime::command::RuntimeCommand,
        flag: &str,
    ) -> Option<&'a str> {
        command
            .args_vec()
            .windows(2)
            .find_map(|args| (args[0] == flag).then_some(args[1].as_str()))
    }

    #[test]
    fn list_workspace_containers_preserves_inspected_mounts_for_reuse() {
        let runner = FakeRuntimeCommand::new(vec![
            Ok(output(
                br#"[{
                    "Id": "container-id",
                    "Name": "/decune-project",
                    "Image": "sha256:image",
                    "Config": {
                        "Labels": {
                            "decune.config_hash": "hash123"
                        }
                    },
                    "State": {
                        "Running": true,
                        "ExitCode": 0,
                        "Pid": 1234
                    },
                    "Mounts": [{
                        "Type": "bind",
                        "Source": "/tmp/ssh-agent.sock",
                        "Destination": "/run/decune/ssh-agent.sock",
                        "RW": true
                    }]
                }]"#,
            )),
            Ok(output(
                br#"{"ID":"container-id","Names":"decune-project","ImageID":"sha256:image","State":"running","Labels":"decune.config_hash=hash123"}"#,
            )),
        ]);
        let client = DockerCli::new(Arc::new(runner.clone()));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let containers = runtime
            .block_on(client.list_workspace_containers("workspace123"))
            .unwrap();

        assert_eq!(containers.len(), 1);
        let container = &containers[0];
        assert_eq!(container.id, "container-id");
        assert_eq!(container.name, "decune-project");
        assert_eq!(container.config_hash.as_deref(), Some("hash123"));
        assert!(container.running);
        assert_eq!(container.mounts.as_ref().unwrap().len(), 1);
        let mount = &container.mounts.as_ref().unwrap()[0];
        assert_eq!(mount.source.as_deref(), Some("/tmp/ssh-agent.sock"));
        assert_eq!(mount.target, "/run/decune/ssh-agent.sock");
        assert_eq!(mount.mount_type, MountType::Bind);
        assert!(!mount.read_only);

        let commands = runner.commands();
        assert_eq!(commands[0].args_vec()[0], "ps");
        assert_eq!(
            commands[1].args_vec(),
            ["container", "inspect", "container-id"]
        );
    }

    fn output(stdout: &[u8]) -> RuntimeOutput {
        RuntimeOutput {
            stdout: stdout.to_vec(),
            stderr: Vec::new(),
            exit_code: 0,
        }
    }
}
