use std::{
    collections::BTreeMap,
    future::Future,
    io::ErrorKind,
    path::{Path, PathBuf},
    pin::Pin,
    process::{ExitStatus, Stdio},
    time::Duration,
};

#[cfg(test)]
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeCommand {
    program: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    current_dir: Option<PathBuf>,
    redactions: RedactionRules,
    timeout: Option<Duration>,
}

impl RuntimeCommand {
    pub(crate) fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            current_dir: None,
            redactions: RedactionRules::default(),
            timeout: None,
        }
    }

    pub(crate) fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub(crate) fn args(mut self, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub(crate) fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    pub(crate) fn current_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.current_dir = Some(dir.into());
        self
    }

    #[cfg(test)]
    pub(crate) fn redact_value(mut self, value: impl Into<String>) -> Self {
        self.redactions.push_value(value);
        self
    }

    pub(crate) fn redact_values(
        mut self,
        values: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        for value in values {
            self.redactions.push_value(value);
        }
        self
    }
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Runtime command timeouts are specified and covered by tests, but no production caller is wired yet."
        )
    )]
    pub(crate) fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub(crate) fn program(&self) -> &str {
        &self.program
    }

    pub(crate) fn args_vec(&self) -> &[String] {
        &self.args
    }

    pub(crate) fn envs(&self) -> &BTreeMap<String, String> {
        &self.env
    }

    pub(crate) fn current_dir_path(&self) -> Option<&Path> {
        self.current_dir.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn env_value(&self, key: &str) -> Option<&String> {
        self.env.get(key)
    }

    pub(crate) fn timeout_duration(&self) -> Option<Duration> {
        self.timeout
    }

    pub(crate) fn sanitized_display(&self) -> String {
        let mut parts = Vec::with_capacity(self.args.len() + 1);
        parts.push(self.redactions.redact(&self.program));
        parts.extend(self.args.iter().map(|arg| self.redactions.redact(arg)));
        parts.join(" ")
    }

    pub(crate) fn redact_output(&self, value: &str) -> String {
        self.redactions.redact(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct RedactionRules {
    values: Vec<String>,
}

impl RedactionRules {
    pub(crate) fn push_value(&mut self, value: impl Into<String>) {
        let value = value.into();
        if !value.is_empty() {
            self.values.push(value);
        }
    }

    pub(crate) fn redact(&self, value: &str) -> String {
        self.values
            .iter()
            .fold(value.to_owned(), |redacted, secret| {
                redacted.replace(secret, "[REDACTED]")
            })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeOutput {
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    pub(crate) exit_code: i32,
}

impl RuntimeOutput {
    pub(crate) fn stdout_string(&self) -> Result<String> {
        String::from_utf8(self.stdout.clone()).context("Command stdout was not valid UTF-8")
    }

    pub(crate) fn stderr_string_lossy(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

pub(crate) trait RuntimeCommandRunner: Send + Sync {
    fn run_capture<'a>(
        &'a self,
        command: RuntimeCommand,
    ) -> Pin<Box<dyn Future<Output = Result<RuntimeOutput>> + Send + 'a>>;

    fn run_capture_with_stdin<'a>(
        &'a self,
        command: RuntimeCommand,
        stdin: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = Result<RuntimeOutput>> + Send + 'a>>;

    fn run_status<'a>(
        &'a self,
        command: RuntimeCommand,
        stdio: RuntimeStdio,
    ) -> Pin<Box<dyn Future<Output = Result<i32>> + Send + 'a>>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeStdio {
    Inherit,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct TokioRuntimeCommand;

impl RuntimeCommandRunner for TokioRuntimeCommand {
    fn run_capture<'a>(
        &'a self,
        command: RuntimeCommand,
    ) -> Pin<Box<dyn Future<Output = Result<RuntimeOutput>> + Send + 'a>> {
        Box::pin(async move { run_capture_process(command, None).await })
    }

    fn run_capture_with_stdin<'a>(
        &'a self,
        command: RuntimeCommand,
        stdin: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = Result<RuntimeOutput>> + Send + 'a>> {
        Box::pin(async move { run_capture_process(command, Some(stdin)).await })
    }

    fn run_status<'a>(
        &'a self,
        command: RuntimeCommand,
        _stdio: RuntimeStdio,
    ) -> Pin<Box<dyn Future<Output = Result<i32>> + Send + 'a>> {
        Box::pin(async move {
            let command_display = command.sanitized_display();
            let mut process = Command::new(command.program());
            process.args(command.args_vec());
            process.envs(command.envs());
            if let Some(current_dir) = command.current_dir_path() {
                process.current_dir(current_dir);
            }
            process
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());
            let mut child = process
                .spawn()
                .with_context(|| format!("Failed to run command: {command_display}"))?;
            let status =
                wait_for_child(&mut child, command.timeout_duration(), &command_display).await?;
            Ok(status.code().unwrap_or(1))
        })
    }
}

async fn run_capture_process(
    command: RuntimeCommand,
    stdin: Option<Vec<u8>>,
) -> Result<RuntimeOutput> {
    let command_display = command.sanitized_display();
    let mut process = Command::new(command.program());
    process.args(command.args_vec());
    process.envs(command.envs());
    if let Some(current_dir) = command.current_dir_path() {
        process.current_dir(current_dir);
    }
    process
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = process
        .spawn()
        .with_context(|| format!("Failed to run command: {command_display}"))?;
    let mut tasks = CaptureTasks::spawn(&mut child, stdin, &command_display)?;
    let output = match command.timeout_duration() {
        Some(timeout) => {
            let result = tokio::time::timeout(
                timeout,
                wait_for_child_and_capture(&mut child, &mut tasks, &command_display),
            )
            .await;
            match result {
                Ok(output) => output?,
                Err(_) => {
                    kill_and_wait_child(&mut child, &command_display).await?;
                    tasks.abort_and_join().await;
                    return Err(timeout_error(&command_display));
                }
            }
        }
        None => wait_for_child_and_capture(&mut child, &mut tasks, &command_display).await?,
    };

    Ok(output)
}

async fn wait_for_child_and_capture(
    child: &mut Child,
    tasks: &mut CaptureTasks,
    command_display: &str,
) -> Result<RuntimeOutput> {
    let status = match wait_for_child(child, None, command_display).await {
        Ok(status) => status,
        Err(err) => {
            tasks.abort_and_join().await;
            return Err(err);
        }
    };
    let (stdout, stderr) = tasks.join(status.success(), command_display).await?;

    Ok(RuntimeOutput {
        stdout,
        stderr,
        exit_code: status.code().unwrap_or(1),
    })
}

async fn wait_for_child(
    child: &mut Child,
    timeout: Option<Duration>,
    command_display: &str,
) -> Result<ExitStatus> {
    let wait = child.wait();
    if let Some(timeout) = timeout {
        match tokio::time::timeout(timeout, wait).await {
            Ok(status) => {
                status.with_context(|| format!("Failed to run command: {command_display}"))
            }
            Err(_) => {
                kill_and_wait_child(child, command_display).await?;
                Err(timeout_error(command_display))
            }
        }
    } else {
        wait.await
            .with_context(|| format!("Failed to run command: {command_display}"))
    }
}

async fn kill_and_wait_child(child: &mut Child, command_display: &str) -> Result<()> {
    match child.kill().await {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::InvalidInput => {}
        Err(err) => {
            return Err(err)
                .with_context(|| format!("Failed to kill timed out command: {command_display}"));
        }
    }
    child
        .wait()
        .await
        .with_context(|| format!("Failed to reap timed out command: {command_display}"))?;
    Ok(())
}

fn timeout_error(command_display: &str) -> anyhow::Error {
    anyhow::anyhow!("Command timed out: {command_display}")
}

struct CaptureTasks {
    stdout: Option<JoinHandle<Result<Vec<u8>>>>,
    stderr: Option<JoinHandle<Result<Vec<u8>>>>,
    stdin: Option<JoinHandle<Result<()>>>,
}

impl CaptureTasks {
    fn spawn(child: &mut Child, stdin: Option<Vec<u8>>, command_display: &str) -> Result<Self> {
        let stdout = child
            .stdout
            .take()
            .with_context(|| format!("Failed to open command stdout: {command_display}"))?;
        let stderr = child
            .stderr
            .take()
            .with_context(|| format!("Failed to open command stderr: {command_display}"))?;
        let stdin = if let Some(stdin) = stdin {
            let child_stdin = child
                .stdin
                .take()
                .with_context(|| format!("Failed to open command stdin: {command_display}"))?;
            Some(spawn_stdin_task(
                child_stdin,
                stdin,
                command_display.to_owned(),
            ))
        } else {
            None
        };

        Ok(Self {
            stdout: Some(spawn_read_task(
                stdout,
                "stdout",
                command_display.to_owned(),
            )),
            stderr: Some(spawn_read_task(
                stderr,
                "stderr",
                command_display.to_owned(),
            )),
            stdin,
        })
    }

    async fn join(&mut self, success: bool, command_display: &str) -> Result<(Vec<u8>, Vec<u8>)> {
        let stdout = self
            .stdout
            .take()
            .with_context(|| format!("Command stdout task was already joined: {command_display}"))?
            .await
            .with_context(|| format!("Failed to join command stdout: {command_display}"))??;
        let stderr = self
            .stderr
            .take()
            .with_context(|| format!("Command stderr task was already joined: {command_display}"))?
            .await
            .with_context(|| format!("Failed to join command stderr: {command_display}"))??;

        if let Some(stdin) = self.stdin.take() {
            let write_result = stdin
                .await
                .with_context(|| format!("Failed to join command stdin: {command_display}"))?;
            if success {
                write_result?;
            }
        }

        Ok((stdout, stderr))
    }

    async fn abort_and_join(&mut self) {
        if let Some(stdout) = &self.stdout {
            stdout.abort();
        }
        if let Some(stderr) = &self.stderr {
            stderr.abort();
        }
        if let Some(stdin) = &self.stdin {
            stdin.abort();
        }

        if let Some(stdout) = self.stdout.take() {
            let _ = stdout.await;
        }
        if let Some(stderr) = self.stderr.take() {
            let _ = stderr.await;
        }
        if let Some(stdin) = self.stdin.take() {
            let _ = stdin.await;
        }
    }
}

fn spawn_read_task<R>(
    mut reader: R,
    stream_name: &'static str,
    command_display: String,
) -> JoinHandle<Result<Vec<u8>>>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut output = Vec::new();
        reader
            .read_to_end(&mut output)
            .await
            .with_context(|| format!("Failed to read command {stream_name}: {command_display}"))?;
        Ok(output)
    })
}

fn spawn_stdin_task(
    mut child_stdin: tokio::process::ChildStdin,
    stdin: Vec<u8>,
    command_display: String,
) -> JoinHandle<Result<()>> {
    tokio::spawn(async move {
        let result = child_stdin
            .write_all(&stdin)
            .await
            .with_context(|| format!("Failed to write command stdin: {command_display}"));
        drop(child_stdin);
        result
    })
}

#[cfg(test)]
#[derive(Debug, Clone, Default)]
pub(crate) struct FakeRuntimeCommand {
    responses: Arc<std::sync::Mutex<Vec<Result<RuntimeOutput, String>>>>,
    commands: Arc<std::sync::Mutex<Vec<RuntimeCommand>>>,
    stdin: Arc<std::sync::Mutex<Vec<Option<Vec<u8>>>>>,
}

#[cfg(test)]
impl FakeRuntimeCommand {
    pub(crate) fn new(responses: Vec<Result<RuntimeOutput, String>>) -> Self {
        Self {
            responses: Arc::new(std::sync::Mutex::new(responses)),
            commands: Arc::default(),
            stdin: Arc::default(),
        }
    }
    pub(crate) fn commands(&self) -> Vec<RuntimeCommand> {
        self.commands.lock().unwrap().clone()
    }
    pub(crate) fn stdin(&self) -> Vec<Option<Vec<u8>>> {
        self.stdin.lock().unwrap().clone()
    }
}

#[cfg(test)]
impl RuntimeCommandRunner for FakeRuntimeCommand {
    fn run_capture<'a>(
        &'a self,
        command: RuntimeCommand,
    ) -> Pin<Box<dyn Future<Output = Result<RuntimeOutput>> + Send + 'a>> {
        Box::pin(async move {
            self.commands.lock().unwrap().push(command);
            self.stdin.lock().unwrap().push(None);
            let response = self
                .responses
                .lock()
                .unwrap()
                .pop()
                .unwrap_or_else(|| Err("fake runtime command response missing".to_owned()));
            response.map_err(anyhow::Error::msg)
        })
    }

    fn run_capture_with_stdin<'a>(
        &'a self,
        command: RuntimeCommand,
        stdin: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = Result<RuntimeOutput>> + Send + 'a>> {
        Box::pin(async move {
            self.commands.lock().unwrap().push(command);
            self.stdin.lock().unwrap().push(Some(stdin));
            let response = self
                .responses
                .lock()
                .unwrap()
                .pop()
                .unwrap_or_else(|| Err("fake runtime command response missing".to_owned()));
            response.map_err(anyhow::Error::msg)
        })
    }

    fn run_status<'a>(
        &'a self,
        command: RuntimeCommand,
        _stdio: RuntimeStdio,
    ) -> Pin<Box<dyn Future<Output = Result<i32>> + Send + 'a>> {
        Box::pin(async move {
            self.commands.lock().unwrap().push(command);
            self.stdin.lock().unwrap().push(None);
            Ok(0)
        })
    }
}

pub(crate) fn ensure_success(
    action: &str,
    target: &str,
    command: &RuntimeCommand,
    output: &RuntimeOutput,
) -> Result<()> {
    if output.exit_code == 0 {
        return Ok(());
    }

    let stderr = command.redact_output(&output.stderr_string_lossy());
    bail!(
        "Failed to {action}: {target}. Command `{}` exited with status {}. stderr: {}",
        command.sanitized_display(),
        output.exit_code,
        stderr.trim()
    );
}

#[cfg(test)]
mod tests {
    use super::{RedactionRules, RuntimeCommand, RuntimeCommandRunner, TokioRuntimeCommand};

    use std::path::Path;

    #[cfg(unix)]
    use std::{
        fs,
        time::{Duration, Instant},
    };

    #[test]
    fn runtime_command_keeps_program_and_argv_without_shell_string() {
        let command = RuntimeCommand::new("docker").args(["inspect", "container"]);

        assert_eq!(command.program(), "docker");
        assert_eq!(command.args_vec(), ["inspect", "container"]);
        assert_eq!(command.sanitized_display(), "docker inspect container");
    }

    #[test]
    fn runtime_command_keeps_child_env_out_of_display() {
        let command = RuntimeCommand::new("docker")
            .args(["exec", "--env", "DECUNE_SECRET", "container", "env"])
            .env("DECUNE_SECRET", "secret-token");

        assert_eq!(
            command.env_value("DECUNE_SECRET").map(String::as_str),
            Some("secret-token")
        );
        assert!(!command.sanitized_display().contains("secret-token"));
    }

    #[test]
    fn runtime_command_records_current_dir_without_adding_it_to_display() {
        let command = RuntimeCommand::new("docker")
            .arg("ps")
            .current_dir("/workspace");

        assert_eq!(command.current_dir_path(), Some(Path::new("/workspace")));
        assert_eq!(command.sanitized_display(), "docker ps");
        assert!(!command.sanitized_display().contains("/workspace"));
    }

    #[cfg(unix)]
    #[test]
    fn runtime_command_runner_applies_current_dir_to_process() {
        let tempdir = tempfile::tempdir().unwrap();
        let command = RuntimeCommand::new("pwd").current_dir(tempdir.path());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let output = runtime
            .block_on(TokioRuntimeCommand.run_capture(command))
            .unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();

        assert_eq!(Path::new(stdout.trim()), tempdir.path());
    }

    #[test]
    fn runtime_command_runner_passes_child_env_to_process() {
        let command = RuntimeCommand::new("env").env("DECUNE_RUNTIME_TEST_ENV", "visible");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let output = runtime
            .block_on(TokioRuntimeCommand.run_capture(command))
            .unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();

        assert!(stdout.contains("DECUNE_RUNTIME_TEST_ENV=visible"));
    }

    #[test]
    fn redaction_rules_hide_secret_values_in_command_display_and_output() {
        let command = RuntimeCommand::new("docker")
            .args(["login", "--password-stdin"])
            .redact_value("secret-token");
        let mut rules = RedactionRules::default();
        rules.push_value("secret-token");

        assert_eq!(
            command.redact_output("token=secret-token"),
            "token=[REDACTED]"
        );
        assert_eq!(
            rules.redact("secret-token is hidden"),
            "[REDACTED] is hidden"
        );
    }
    #[cfg(unix)]
    #[test]
    fn runtime_command_timeout_kills_and_reaps_child_process() {
        let tempdir = tempfile::tempdir().unwrap();
        let pid_path = tempdir.path().join("sleep.pid");
        let command = sleeper_command(&pid_path).timeout(Duration::from_millis(50));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let err = runtime
            .block_on(TokioRuntimeCommand.run_capture(command))
            .unwrap_err();

        assert!(err.to_string().contains("Command timed out"));
        let pid = read_pid(&pid_path);
        assert!(!process_exists(pid), "process {pid} was still running");
    }

    #[cfg(unix)]
    #[test]
    fn runtime_command_timeout_cleans_up_stdin_writer_task_and_child() {
        let tempdir = tempfile::tempdir().unwrap();
        let pid_path = tempdir.path().join("sleep.pid");
        let command = sleeper_command(&pid_path).timeout(Duration::from_millis(50));
        let stdin = vec![b'x'; 16 * 1024 * 1024];
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let err = runtime
            .block_on(TokioRuntimeCommand.run_capture_with_stdin(command, stdin))
            .unwrap_err();

        assert!(err.to_string().contains("Command timed out"));
        let pid = read_pid(&pid_path);
        assert!(!process_exists(pid), "process {pid} was still running");
    }

    #[cfg(unix)]
    #[test]
    fn runtime_command_timeout_error_uses_sanitized_display() {
        let secret = "secret-token-008";
        let command = RuntimeCommand::new("sh")
            .args(["-c", "sleep 30"])
            .env("DECUNE_SECRET", secret)
            .timeout(Duration::from_millis(50));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let err = runtime
            .block_on(TokioRuntimeCommand.run_capture(command))
            .unwrap_err();
        let message = err.to_string();

        assert!(message.contains("Command timed out"));
        assert!(!message.contains(secret));
    }

    #[cfg(unix)]
    #[test]
    fn runtime_command_timeout_covers_capture_task_join_when_descendant_keeps_pipe_open() {
        let command = RuntimeCommand::new("sh")
            .args(["-c", "printf parent-done; sleep 1 &"])
            .timeout(Duration::from_millis(50));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let start = Instant::now();
        let err = runtime
            .block_on(TokioRuntimeCommand.run_capture(command))
            .unwrap_err();

        assert!(err.to_string().contains("Command timed out"));
        assert!(start.elapsed() < Duration::from_millis(500));
    }

    #[cfg(unix)]
    fn sleeper_command(pid_path: &Path) -> RuntimeCommand {
        RuntimeCommand::new("sh")
            .args([
                "-c",
                "printf '%s' \"$$\" > \"$DECUNE_PID_FILE\"; exec sleep 30",
            ])
            .env("DECUNE_PID_FILE", pid_path.display().to_string())
    }

    #[cfg(unix)]
    fn read_pid(pid_path: &Path) -> libc::pid_t {
        fs::read_to_string(pid_path)
            .unwrap()
            .parse::<libc::pid_t>()
            .unwrap()
    }

    #[cfg(unix)]
    fn process_exists(pid: libc::pid_t) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }
}
