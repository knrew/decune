#![allow(dead_code)]

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use bollard::{
    container::LogOutput,
    exec::{CreateExecOptions, StartExecOptions, StartExecResults},
    models::ExecInspectResponse,
};
use futures_util::TryStreamExt;

use crate::docker::client::DockerClient;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExecAttachMode {
    Capture,
    Stream,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExecCommandSpec {
    pub(crate) command: Vec<String>,
    pub(crate) user: Option<String>,
    pub(crate) working_dir: Option<String>,
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) tty: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExecOutput {
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    pub(crate) exit_code: i64,
}

#[derive(Debug)]
pub(crate) struct AttachedExec {
    pub(crate) id: String,
    pub(crate) results: StartExecResults,
}

pub(crate) async fn exec_capture(
    client: &DockerClient,
    container: &str,
    spec: &ExecCommandSpec,
) -> Result<ExecOutput> {
    let output = exec_capture_output(client, container, spec).await?;

    ensure_success_output(container, &spec.command, &output)?;

    Ok(output)
}

pub(crate) async fn exec_capture_output(
    client: &DockerClient,
    container: &str,
    spec: &ExecCommandSpec,
) -> Result<ExecOutput> {
    validate_exec_spec(spec)?;

    let exec_id = create_exec(client, container, spec, ExecAttachMode::Capture).await?;
    let mut output = start_exec_and_capture_output(client, &exec_id, container, spec.tty).await?;
    let inspect = inspect_exec(client, &exec_id, container).await?;
    output.exit_code = inspect
        .exit_code
        .context("Failed to read Docker exec exit code")?;

    Ok(output)
}

pub(crate) async fn exec_attach(
    client: &DockerClient,
    container: &str,
    spec: &ExecCommandSpec,
) -> Result<AttachedExec> {
    validate_exec_spec(spec)?;

    let exec_id = create_exec(client, container, spec, ExecAttachMode::Stream).await?;
    let options = start_exec_options(spec.tty);
    let results = client
        .raw()
        .start_exec(&exec_id, Some(options))
        .await
        .with_context(|| format!("Failed to start Docker exec in container: {container}"))?;

    Ok(AttachedExec {
        id: exec_id,
        results,
    })
}

pub(crate) fn create_exec_options(
    spec: &ExecCommandSpec,
    mode: ExecAttachMode,
) -> CreateExecOptions<String> {
    CreateExecOptions {
        attach_stdin: Some(matches!(mode, ExecAttachMode::Stream)),
        attach_stdout: Some(true),
        attach_stderr: Some(true),
        tty: Some(spec.tty),
        env: non_empty_vec(env_entries(&spec.env)),
        cmd: Some(spec.command.clone()),
        user: spec.user.clone(),
        working_dir: spec.working_dir.clone(),
        ..Default::default()
    }
}

fn start_exec_options(tty: bool) -> StartExecOptions {
    StartExecOptions {
        detach: false,
        tty,
        output_capacity: None,
    }
}

async fn create_exec(
    client: &DockerClient,
    container: &str,
    spec: &ExecCommandSpec,
    mode: ExecAttachMode,
) -> Result<String> {
    let options = create_exec_options(spec, mode);
    let response = client
        .raw()
        .create_exec(container, options)
        .await
        .with_context(|| format!("Failed to create Docker exec in container: {container}"))?;

    Ok(response.id)
}

async fn start_exec_and_capture_output(
    client: &DockerClient,
    exec_id: &str,
    container: &str,
    tty: bool,
) -> Result<ExecOutput> {
    let options = start_exec_options(tty);
    let results = client
        .raw()
        .start_exec(exec_id, Some(options))
        .await
        .with_context(|| format!("Failed to start Docker exec in container: {container}"))?;

    let StartExecResults::Attached { mut output, .. } = results else {
        bail!("Docker exec did not attach output in container: {container}");
    };

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();

    while let Some(log_output) = output
        .try_next()
        .await
        .with_context(|| format!("Failed to read Docker exec output in container: {container}"))?
    {
        match log_output {
            LogOutput::StdOut { message } | LogOutput::Console { message } => {
                stdout.extend_from_slice(&message);
            }
            LogOutput::StdErr { message } => {
                stderr.extend_from_slice(&message);
            }
            LogOutput::StdIn { .. } => {}
        }
    }

    Ok(ExecOutput {
        stdout,
        stderr,
        exit_code: 0,
    })
}

async fn inspect_exec(
    client: &DockerClient,
    exec_id: &str,
    container: &str,
) -> Result<ExecInspectResponse> {
    client
        .raw()
        .inspect_exec(exec_id)
        .await
        .with_context(|| format!("Failed to inspect Docker exec in container: {container}"))
}

pub(crate) fn ensure_success_exit(
    container: &str,
    command: &[String],
    inspect: &ExecInspectResponse,
    output: &ExecOutput,
) -> Result<()> {
    let exit_code = inspect
        .exit_code
        .context("Failed to read Docker exec exit code")?;

    ensure_success_exit_code(container, command, exit_code, output)
}

fn ensure_success_output(container: &str, command: &[String], output: &ExecOutput) -> Result<()> {
    ensure_success_exit_code(container, command, output.exit_code, output)
}

fn ensure_success_exit_code(
    container: &str,
    command: &[String],
    exit_code: i64,
    output: &ExecOutput,
) -> Result<()> {
    if exit_code == 0 {
        return Ok(());
    }

    bail!(
        "Docker exec failed in container {container}: command `{}` exited with exit code {exit_code}. stdout tail: `{}` stderr tail: `{}`",
        command_display(command),
        output_tail(&output.stdout),
        output_tail(&output.stderr),
    );
}

fn validate_exec_spec(spec: &ExecCommandSpec) -> Result<()> {
    if spec.command.is_empty() {
        bail!("Docker exec command must not be empty");
    }

    Ok(())
}

fn env_entries(env: &BTreeMap<String, String>) -> Vec<String> {
    env.iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect()
}

fn non_empty_vec<T>(values: Vec<T>) -> Option<Vec<T>> {
    if values.is_empty() {
        None
    } else {
        Some(values)
    }
}

fn command_display(command: &[String]) -> String {
    command.join(" ")
}

fn output_tail(output: &[u8]) -> String {
    const MAX_TAIL_BYTES: usize = 4096;

    let start = output.len().saturating_sub(MAX_TAIL_BYTES);
    String::from_utf8_lossy(&output[start..]).trim().to_owned()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use anyhow::{Result, bail};
    use bollard::{
        container::LogOutput,
        exec::StartExecResults,
        models::{ContainerCreateBody, ExecInspectResponse},
        query_parameters::CreateContainerOptionsBuilder,
    };
    use futures_util::TryStreamExt;

    use super::{
        ExecAttachMode, ExecCommandSpec, ExecOutput, create_exec_options, ensure_success_exit,
        exec_attach, exec_capture, exec_capture_output, start_exec_options,
    };
    use crate::docker::{
        client::DockerClient,
        container::{remove_container, start_container},
        image::{PullPolicy, ensure_image},
    };

    #[test]
    fn create_exec_options_includes_command_context_and_capture_io() {
        let spec = ExecCommandSpec {
            command: vec![
                "/bin/sh".to_owned(),
                "-lc".to_owned(),
                "echo $GREETING".to_owned(),
            ],
            user: Some("vscode".to_owned()),
            working_dir: Some("/workspaces/project".to_owned()),
            env: BTreeMap::from([("GREETING".to_owned(), "hello".to_owned())]),
            tty: false,
        };

        let options = create_exec_options(&spec, ExecAttachMode::Capture);

        assert_eq!(options.attach_stdin, Some(false));
        assert_eq!(options.attach_stdout, Some(true));
        assert_eq!(options.attach_stderr, Some(true));
        assert_eq!(options.tty, Some(false));
        assert_eq!(options.user.as_deref(), Some("vscode"));
        assert_eq!(options.working_dir.as_deref(), Some("/workspaces/project"));
        assert_eq!(
            options.cmd,
            Some(vec![
                "/bin/sh".to_owned(),
                "-lc".to_owned(),
                "echo $GREETING".to_owned(),
            ])
        );
        assert_eq!(options.env, Some(vec!["GREETING=hello".to_owned()]));
    }

    #[test]
    fn create_exec_options_attaches_stdin_for_streaming_tty() {
        let spec = ExecCommandSpec {
            command: vec!["/bin/sh".to_owned()],
            user: None,
            working_dir: None,
            env: BTreeMap::new(),
            tty: true,
        };

        let options = create_exec_options(&spec, ExecAttachMode::Stream);

        assert_eq!(options.attach_stdin, Some(true));
        assert_eq!(options.attach_stdout, Some(true));
        assert_eq!(options.attach_stderr, Some(true));
        assert_eq!(options.tty, Some(true));
    }

    #[test]
    fn start_exec_options_preserves_capture_tty() {
        let options = start_exec_options(true);

        assert!(!options.detach);
        assert!(options.tty);
        assert_eq!(options.output_capacity, None);
    }

    #[test]
    fn ensure_success_exit_accepts_zero_exit_code() {
        let inspect = ExecInspectResponse {
            exit_code: Some(0),
            ..Default::default()
        };
        let output = ExecOutput {
            stdout: b"ok\n".to_vec(),
            stderr: Vec::new(),
            exit_code: 0,
        };

        ensure_success_exit("container-1", &["true".to_owned()], &inspect, &output).unwrap();
    }

    #[test]
    fn ensure_success_exit_reports_non_zero_with_output_tail() {
        let inspect = ExecInspectResponse {
            exit_code: Some(7),
            ..Default::default()
        };
        let output = ExecOutput {
            stdout: b"before\n".to_vec(),
            stderr: b"bad things happened\n".to_vec(),
            exit_code: 7,
        };

        let error = ensure_success_exit("container-1", &["false".to_owned()], &inspect, &output)
            .unwrap_err();
        let message = format!("{error:#}");

        assert!(message.contains("Docker exec failed in container container-1"));
        assert!(message.contains("exit code 7"));
        assert!(message.contains("false"));
        assert!(message.contains("bad things happened"));
    }

    #[test]
    fn exec_capture_returns_stdout_when_docker_tests_are_enabled() {
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
            let name = test_container_name("exec-capture-stdout");
            let result = async {
                create_running_exec_test_container(&client, &name).await?;

                let output = exec_capture(
                    &client,
                    &name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "echo hello".to_owned(),
                        ],
                        user: None,
                        working_dir: Some("/tmp".to_owned()),
                        env: BTreeMap::new(),
                        tty: false,
                    },
                )
                .await?;

                assert_eq!(output.exit_code, 0);
                assert_eq!(String::from_utf8(output.stdout).unwrap(), "hello\n");
                assert!(output.stderr.is_empty());

                Ok(())
            }
            .await;

            let cleanup = remove_container(&client, &name, true, true).await;
            result.and(cleanup).unwrap();
        });
    }

    #[test]
    fn exec_capture_applies_command_context_when_docker_tests_are_enabled() {
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
            let name = test_container_name("exec-capture-context");
            let result = async {
                create_running_exec_test_container(&client, &name).await?;

                let output = exec_capture(
                    &client,
                    &name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "printf '%s:%s:%s' \"$GREETING\" \"$(pwd)\" \"$(id -un)\"".to_owned(),
                        ],
                        user: Some("nobody".to_owned()),
                        working_dir: Some("/tmp".to_owned()),
                        env: BTreeMap::from([("GREETING".to_owned(), "hello".to_owned())]),
                        tty: false,
                    },
                )
                .await?;

                assert_eq!(output.exit_code, 0);
                assert_eq!(
                    String::from_utf8(output.stdout).unwrap(),
                    "hello:/tmp:nobody"
                );
                assert!(output.stderr.is_empty());

                Ok(())
            }
            .await;

            let cleanup = remove_container(&client, &name, true, true).await;
            result.and(cleanup).unwrap();
        });
    }

    #[test]
    fn exec_capture_preserves_tty_console_output_when_docker_tests_are_enabled() {
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
            let name = test_container_name("exec-capture-tty");
            let result = async {
                create_running_exec_test_container(&client, &name).await?;

                let output = exec_capture(
                    &client,
                    &name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "printf tty-output".to_owned(),
                        ],
                        user: None,
                        working_dir: None,
                        env: BTreeMap::new(),
                        tty: true,
                    },
                )
                .await?;

                assert_eq!(output.exit_code, 0);
                assert_eq!(String::from_utf8(output.stdout).unwrap(), "tty-output");
                assert!(output.stderr.is_empty());

                Ok(())
            }
            .await;

            let cleanup = remove_container(&client, &name, true, true).await;
            result.and(cleanup).unwrap();
        });
    }

    #[test]
    fn exec_capture_output_returns_non_zero_exit_when_docker_tests_are_enabled() {
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
            let name = test_container_name("exec-capture-output-nonzero");
            let result = async {
                create_running_exec_test_container(&client, &name).await?;

                let output = exec_capture_output(
                    &client,
                    &name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "echo failure >&2; exit 7".to_owned(),
                        ],
                        user: None,
                        working_dir: None,
                        env: BTreeMap::new(),
                        tty: false,
                    },
                )
                .await?;

                assert_eq!(output.exit_code, 7);
                assert!(output.stdout.is_empty());
                assert_eq!(String::from_utf8(output.stderr).unwrap(), "failure\n");

                Ok(())
            }
            .await;

            let cleanup = remove_container(&client, &name, true, true).await;
            result.and(cleanup).unwrap();
        });
    }

    #[test]
    fn exec_capture_returns_error_for_non_zero_exit_when_docker_tests_are_enabled() {
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
            let name = test_container_name("exec-capture-nonzero");
            let result = async {
                create_running_exec_test_container(&client, &name).await?;

                let error = exec_capture(
                    &client,
                    &name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "echo failure >&2; exit 7".to_owned(),
                        ],
                        user: None,
                        working_dir: None,
                        env: BTreeMap::new(),
                        tty: false,
                    },
                )
                .await
                .unwrap_err();
                let message = format!("{error:#}");

                assert!(message.contains("exit code 7"));
                assert!(message.contains("failure"));

                Ok(())
            }
            .await;

            let cleanup = remove_container(&client, &name, true, true).await;
            result.and(cleanup).unwrap();
        });
    }

    #[test]
    fn exec_attach_streams_stdout_when_docker_tests_are_enabled() {
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
            let name = test_container_name("exec-attach-stdout");
            let result = async {
                create_running_exec_test_container(&client, &name).await?;

                let attached = exec_attach(
                    &client,
                    &name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "echo attached".to_owned(),
                        ],
                        user: None,
                        working_dir: Some("/tmp".to_owned()),
                        env: BTreeMap::new(),
                        tty: false,
                    },
                )
                .await?;

                let StartExecResults::Attached { mut output, .. } = attached.results else {
                    bail!("Docker exec did not return an attached stream");
                };
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                while let Some(log_output) = output.try_next().await? {
                    match log_output {
                        LogOutput::StdOut { message } | LogOutput::Console { message } => {
                            stdout.extend_from_slice(&message);
                        }
                        LogOutput::StdErr { message } => stderr.extend_from_slice(&message),
                        LogOutput::StdIn { .. } => {}
                    }
                }

                let inspect = client.raw().inspect_exec(&attached.id).await?;
                assert_eq!(inspect.exit_code, Some(0));
                assert_eq!(String::from_utf8(stdout).unwrap(), "attached\n");
                assert!(stderr.is_empty());

                Ok(())
            }
            .await;

            let cleanup = remove_container(&client, &name, true, true).await;
            result.and(cleanup).unwrap();
        });
    }

    async fn create_running_exec_test_container(client: &DockerClient, name: &str) -> Result<()> {
        ensure_image(client, "alpine:3.20", PullPolicy::Missing).await?;
        remove_container(client, name, true, true).await?;

        let options = CreateContainerOptionsBuilder::default().name(name).build();
        let body = ContainerCreateBody {
            image: Some("alpine:3.20".to_owned()),
            cmd: Some(vec!["sleep".to_owned(), "60".to_owned()]),
            ..Default::default()
        };

        client.raw().create_container(Some(options), body).await?;
        start_container(client, name).await?;

        Ok(())
    }

    fn test_container_name(test_name: &str) -> String {
        format!("decune-test-{test_name}-{}", std::process::id())
    }

    fn docker_tests_enabled() -> bool {
        std::env::var_os("DECUNE_DOCKER_TESTS").is_some_and(|value| value == "1")
    }
}
