use std::{collections::BTreeMap, future::Future, pin::Pin, process::Stdio, time::Duration};

#[cfg(test)]
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use tokio::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeCommand {
    program: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    redactions: RedactionRules,
    timeout: Option<Duration>,
}

impl RuntimeCommand {
    pub(crate) fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
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

    #[allow(dead_code)]
    pub(crate) fn redact_value(mut self, value: impl Into<String>) -> Self {
        self.redactions.push_value(value);
        self
    }

    #[allow(dead_code)]
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
    #[allow(dead_code)]
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

    fn run_status<'a>(
        &'a self,
        command: RuntimeCommand,
        stdio: RuntimeStdio,
    ) -> Pin<Box<dyn Future<Output = Result<i32>> + Send + 'a>>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeStdio {
    #[allow(dead_code)]
    Capture,
    Inherit,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct TokioRuntimeCommand;

impl RuntimeCommandRunner for TokioRuntimeCommand {
    fn run_capture<'a>(
        &'a self,
        command: RuntimeCommand,
    ) -> Pin<Box<dyn Future<Output = Result<RuntimeOutput>> + Send + 'a>> {
        Box::pin(async move {
            let mut process = Command::new(command.program());
            process.args(command.args_vec());
            process.envs(command.envs());
            let output = if let Some(timeout) = command.timeout_duration() {
                tokio::time::timeout(timeout, process.output())
                    .await
                    .with_context(|| {
                        format!("Command timed out: {}", command.sanitized_display())
                    })??
            } else {
                process.output().await.with_context(|| {
                    format!("Failed to run command: {}", command.sanitized_display())
                })?
            };

            Ok(RuntimeOutput {
                stdout: output.stdout,
                stderr: output.stderr,
                exit_code: output.status.code().unwrap_or(1),
            })
        })
    }

    fn run_status<'a>(
        &'a self,
        command: RuntimeCommand,
        stdio: RuntimeStdio,
    ) -> Pin<Box<dyn Future<Output = Result<i32>> + Send + 'a>> {
        Box::pin(async move {
            let mut process = Command::new(command.program());
            process.args(command.args_vec());
            process.envs(command.envs());
            match stdio {
                RuntimeStdio::Capture => {
                    process
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null());
                }
                RuntimeStdio::Inherit => {
                    process
                        .stdin(Stdio::inherit())
                        .stdout(Stdio::inherit())
                        .stderr(Stdio::inherit());
                }
            }
            let status = process.status().await.with_context(|| {
                format!("Failed to run command: {}", command.sanitized_display())
            })?;
            Ok(status.code().unwrap_or(1))
        })
    }
}

#[cfg(test)]
#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub(crate) struct FakeRuntimeCommand {
    responses: Arc<std::sync::Mutex<Vec<Result<RuntimeOutput, String>>>>,
    commands: Arc<std::sync::Mutex<Vec<RuntimeCommand>>>,
}

#[cfg(test)]
impl FakeRuntimeCommand {
    #[allow(dead_code)]
    pub(crate) fn new(responses: Vec<Result<RuntimeOutput, String>>) -> Self {
        Self {
            responses: Arc::new(std::sync::Mutex::new(responses)),
            commands: Arc::default(),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn commands(&self) -> Vec<RuntimeCommand> {
        self.commands.lock().unwrap().clone()
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
}
