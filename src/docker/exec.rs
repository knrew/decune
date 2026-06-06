use std::{collections::BTreeMap, pin::Pin, time::Duration};

use anyhow::{Context, Result, bail};
use bollard::{
    Docker,
    container::LogOutput,
    exec::{CreateExecOptions, StartExecOptions, StartExecResults},
    models::ExecInspectResponse,
    query_parameters::ResizeExecOptionsBuilder,
};
use futures_util::TryStreamExt;
use tokio::{
    io::{AsyncReadExt, AsyncWrite, AsyncWriteExt},
    time::sleep,
};

use crate::{
    config::resolved::ResolvedUserEnvProbe,
    docker::client::DockerClient,
    terminal::{self, RawTerminalGuard},
};

#[allow(dead_code)]
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

const EXEC_EXIT_CODE_RETRY_LIMIT: usize = 100;
const EXEC_EXIT_CODE_RETRY_DELAY: Duration = Duration::from_millis(50);

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
    output.exit_code = inspect_exec_exit_code(client, &exec_id, container).await?;

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

#[allow(dead_code)]
pub(crate) async fn exec_attach_stdio(
    client: &DockerClient,
    container: &str,
    spec: &ExecCommandSpec,
) -> Result<i64> {
    let attached = exec_attach(client, container, spec).await?;

    run_attached_exec_stdio(client, container, spec, attached).await
}

pub(crate) async fn exec_detached(
    client: &DockerClient,
    container: &str,
    spec: &ExecCommandSpec,
) -> Result<String> {
    validate_exec_spec(spec)?;

    let options = CreateExecOptions {
        attach_stdin: Some(false),
        attach_stdout: Some(false),
        attach_stderr: Some(false),
        tty: Some(false),
        env: non_empty_vec(env_entries(&spec.env)),
        cmd: Some(spec.command.clone()),
        user: spec.user.clone(),
        working_dir: spec.working_dir.clone(),
        ..Default::default()
    };
    let response = client
        .raw()
        .create_exec(container, options)
        .await
        .with_context(|| {
            format!("Failed to create detached Docker exec in container: {container}")
        })?;
    let start_options = StartExecOptions {
        detach: true,
        tty: false,
        output_capacity: None,
    };
    client
        .raw()
        .start_exec(&response.id, Some(start_options))
        .await
        .with_context(|| {
            format!("Failed to start detached Docker exec in container: {container}")
        })?;

    Ok(response.id)
}

pub(crate) async fn run_attached_exec_stdio(
    client: &DockerClient,
    container: &str,
    spec: &ExecCommandSpec,
    attached: AttachedExec,
) -> Result<i64> {
    let exec_id = attached.id.clone();
    let StartExecResults::Attached { output, input } = attached.results else {
        bail!("Docker exec did not attach stdio in container: {container}");
    };

    let _raw_terminal = spec
        .tty
        .then(RawTerminalGuard::enter_stdin_if_tty)
        .transpose()?;
    if spec.tty {
        resize_exec_to_terminal(client.raw(), &exec_id).await?;
    }

    let input_task = tokio::spawn(copy_stdin_to_exec(input));
    let resize_task = spec
        .tty
        .then(|| spawn_resize_loop(client.raw().clone(), exec_id.clone()));

    let stream_result = stream_exec_output(container, output).await;
    input_task.abort();
    if let Some(resize_task) = resize_task {
        resize_task.abort();
    }
    stream_result?;

    inspect_exec_exit_code(client, &exec_id, container).await
}

pub(crate) async fn resolve_exec_env(
    client: &DockerClient,
    container: &str,
    user: &str,
    user_shell: Option<&str>,
    remote_env: &BTreeMap<String, String>,
    user_env_probe: Option<ResolvedUserEnvProbe>,
) -> Result<BTreeMap<String, String>> {
    let Some(command) = user_env_probe_command(
        user_env_probe.unwrap_or(ResolvedUserEnvProbe::None),
        user_shell,
    ) else {
        return Ok(remote_env.clone());
    };

    let output = exec_capture(
        client,
        container,
        &ExecCommandSpec {
            command,
            user: Some(user.to_owned()),
            working_dir: None,
            env: BTreeMap::new(),
            tty: false,
        },
    )
    .await
    .with_context(|| format!("Failed to probe user environment in container: {container}"))?;
    let stdout = String::from_utf8(output.stdout).with_context(|| {
        format!("User environment probe returned non-UTF-8 output in container: {container}")
    })?;
    let probe_env = parse_env_probe_output(&stdout)?;

    Ok(merge_probe_env(probe_env, remote_env))
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

pub(crate) async fn inspect_exec(
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

async fn inspect_exec_exit_code(
    client: &DockerClient,
    exec_id: &str,
    container: &str,
) -> Result<i64> {
    for attempt in 0..EXEC_EXIT_CODE_RETRY_LIMIT {
        let inspect = inspect_exec(client, exec_id, container).await?;
        if let Some(exit_code) = inspect.exit_code {
            return Ok(exit_code);
        }

        // Docker の attach stream 完了直後は exit_code がまだ反映されないことがある．
        if attempt + 1 < EXEC_EXIT_CODE_RETRY_LIMIT {
            sleep(EXEC_EXIT_CODE_RETRY_DELAY).await;
        }
    }

    bail!("Failed to read Docker exec exit code")
}

async fn copy_stdin_to_exec(mut input: Pin<Box<dyn AsyncWrite + Send>>) -> Result<()> {
    let mut stdin = tokio::io::stdin();
    let mut buffer = [0_u8; 8192];

    loop {
        let read = stdin
            .read(&mut buffer)
            .await
            .context("Failed to read from stdin")?;
        if read == 0 {
            input
                .shutdown()
                .await
                .context("Failed to close Docker exec stdin")?;
            return Ok(());
        }

        input
            .write_all(&buffer[..read])
            .await
            .context("Failed to write to Docker exec stdin")?;
    }
}

async fn stream_exec_output(
    container: &str,
    mut output: Pin<
        Box<
            dyn futures_util::Stream<Item = std::result::Result<LogOutput, bollard::errors::Error>>
                + Send,
        >,
    >,
) -> Result<()> {
    let mut stdout = tokio::io::stdout();
    let mut stderr = tokio::io::stderr();

    while let Some(log_output) = output
        .try_next()
        .await
        .with_context(|| format!("Failed to read Docker exec stream in container: {container}"))?
    {
        match log_output {
            LogOutput::StdOut { message } | LogOutput::Console { message } => {
                stdout
                    .write_all(&message)
                    .await
                    .context("Failed to write Docker exec stdout")?;
                stdout.flush().await.context("Failed to flush stdout")?;
            }
            LogOutput::StdErr { message } => {
                stderr
                    .write_all(&message)
                    .await
                    .context("Failed to write Docker exec stderr")?;
                stderr.flush().await.context("Failed to flush stderr")?;
            }
            LogOutput::StdIn { .. } => {}
        }
    }

    Ok(())
}

async fn resize_exec_to_terminal(docker: &Docker, exec_id: &str) -> Result<()> {
    let Some(size) = terminal::current_size() else {
        return Ok(());
    };

    match docker
        .resize_exec(
            exec_id,
            ResizeExecOptionsBuilder::default()
                .h(size.height)
                .w(size.width)
                .build(),
        )
        .await
    {
        Ok(()) => Ok(()),
        Err(_) if exec_has_finished(docker, exec_id).await.unwrap_or(false) => Ok(()),
        Err(error) => Err(error).context("Failed to resize Docker exec TTY"),
    }
}

async fn exec_has_finished(docker: &Docker, exec_id: &str) -> Result<bool> {
    let inspect = docker
        .inspect_exec(exec_id)
        .await
        .context("Failed to inspect Docker exec after TTY resize failure")?;

    Ok(exec_inspect_has_finished(&inspect))
}

fn exec_inspect_has_finished(inspect: &ExecInspectResponse) -> bool {
    match inspect.running {
        Some(true) => false,
        Some(false) => true,
        None => inspect.exit_code.is_some(),
    }
}

fn spawn_resize_loop(docker: Docker, exec_id: String) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        watch_terminal_resize(docker, exec_id).await;
    })
}

#[cfg(unix)]
async fn watch_terminal_resize(docker: Docker, exec_id: String) {
    let Ok(mut signal) =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
    else {
        return;
    };

    while signal.recv().await.is_some() {
        if let Some(size) = terminal::current_size() {
            let _ = docker
                .resize_exec(
                    &exec_id,
                    ResizeExecOptionsBuilder::default()
                        .h(size.height)
                        .w(size.width)
                        .build(),
                )
                .await;
        }
    }
}

#[cfg(not(unix))]
async fn watch_terminal_resize(_docker: Docker, _exec_id: String) {}

#[allow(dead_code)]
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

pub(crate) fn ensure_success_output(
    container: &str,
    command: &[String],
    output: &ExecOutput,
) -> Result<()> {
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

pub(crate) fn user_env_probe_command(
    probe: ResolvedUserEnvProbe,
    user_shell: Option<&str>,
) -> Option<Vec<String>> {
    let flags = match probe {
        ResolvedUserEnvProbe::None => return None,
        ResolvedUserEnvProbe::LoginShell => "-lc",
        ResolvedUserEnvProbe::InteractiveShell => "-ic",
        ResolvedUserEnvProbe::LoginInteractiveShell => "-lic",
    };
    let shell = user_shell
        .map(str::trim)
        .filter(|shell| !shell.is_empty())
        .unwrap_or("/bin/sh");

    Some(vec![shell.to_owned(), flags.to_owned(), "env".to_owned()])
}

pub(crate) fn parse_env_probe_output(output: &str) -> Result<BTreeMap<String, String>> {
    let mut env = BTreeMap::new();

    for line in output.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.is_empty() {
            continue;
        }
        env.insert(key.to_owned(), value.to_owned());
    }

    Ok(env)
}

pub(crate) fn merge_probe_env(
    mut probe_env: BTreeMap<String, String>,
    remote_env: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    probe_env.extend(remote_env.clone());
    probe_env
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
        exec_attach, exec_capture, exec_capture_output, exec_inspect_has_finished, merge_probe_env,
        parse_env_probe_output, start_exec_options, user_env_probe_command,
    };
    use crate::config::resolved::ResolvedUserEnvProbe;
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
    fn exec_inspect_has_finished_respects_running_state_before_exit_code() {
        let stopped_without_exit_code = ExecInspectResponse {
            running: Some(false),
            exit_code: None,
            ..Default::default()
        };
        let exited_without_running_state = ExecInspectResponse {
            running: None,
            exit_code: Some(0),
            ..Default::default()
        };
        let running = ExecInspectResponse {
            running: Some(true),
            exit_code: None,
            ..Default::default()
        };
        let running_with_exit_code = ExecInspectResponse {
            running: Some(true),
            exit_code: Some(0),
            ..Default::default()
        };

        assert!(exec_inspect_has_finished(&stopped_without_exit_code));
        assert!(exec_inspect_has_finished(&exited_without_running_state));
        assert!(!exec_inspect_has_finished(&running));
        assert!(!exec_inspect_has_finished(&running_with_exit_code));
    }

    #[test]
    fn user_env_probe_none_has_no_command() {
        assert_eq!(
            user_env_probe_command(ResolvedUserEnvProbe::None, None),
            None
        );
    }

    #[test]
    fn user_env_probe_modes_map_to_shell_flags() {
        assert_eq!(
            user_env_probe_command(ResolvedUserEnvProbe::LoginShell, None),
            Some(vec![
                "/bin/sh".to_owned(),
                "-lc".to_owned(),
                "env".to_owned()
            ])
        );
        assert_eq!(
            user_env_probe_command(ResolvedUserEnvProbe::InteractiveShell, None),
            Some(vec![
                "/bin/sh".to_owned(),
                "-ic".to_owned(),
                "env".to_owned()
            ])
        );
        assert_eq!(
            user_env_probe_command(ResolvedUserEnvProbe::LoginInteractiveShell, None),
            Some(vec![
                "/bin/sh".to_owned(),
                "-lic".to_owned(),
                "env".to_owned()
            ])
        );
    }

    #[test]
    fn user_env_probe_uses_remote_user_login_shell() {
        assert_eq!(
            user_env_probe_command(ResolvedUserEnvProbe::LoginShell, Some("/bin/bash")),
            Some(vec![
                "/bin/bash".to_owned(),
                "-lc".to_owned(),
                "env".to_owned()
            ])
        );
    }

    #[test]
    fn parses_env_probe_output_as_key_value_lines() {
        let output = parse_env_probe_output(
            "PATH=/usr/local/bin:/usr/bin\nEMPTY=\nNO_EQUALS\nSHELL=/bin/sh\n",
        )
        .unwrap();

        assert_eq!(
            output.get("PATH").map(String::as_str),
            Some("/usr/local/bin:/usr/bin")
        );
        assert_eq!(output.get("EMPTY").map(String::as_str), Some(""));
        assert_eq!(output.get("SHELL").map(String::as_str), Some("/bin/sh"));
        assert!(!output.contains_key("NO_EQUALS"));
    }

    #[test]
    fn remote_env_overrides_probe_env() {
        let merged = merge_probe_env(
            BTreeMap::from([
                ("PATH".to_owned(), "/usr/bin".to_owned()),
                ("FROM_PROBE".to_owned(), "yes".to_owned()),
            ]),
            &BTreeMap::from([
                ("PATH".to_owned(), "/workspace/bin".to_owned()),
                ("FROM_REMOTE".to_owned(), "yes".to_owned()),
            ]),
        );

        assert_eq!(
            merged.get("PATH").map(String::as_str),
            Some("/workspace/bin")
        );
        assert_eq!(merged.get("FROM_PROBE").map(String::as_str), Some("yes"));
        assert_eq!(merged.get("FROM_REMOTE").map(String::as_str), Some("yes"));
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
    fn exec_capture_returns_stdout() {
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
    fn exec_capture_applies_command_context() {
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
    fn exec_capture_preserves_tty_console_output() {
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
    fn exec_capture_output_returns_non_zero_exit() {
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
    fn exec_capture_returns_error_for_non_zero_exit() {
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
    fn exec_attach_streams_stdout() {
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
}
