use std::collections::BTreeMap;

use anyhow::{Result, bail};

use crate::{config::resolved::ResolvedUserEnvProbe, docker::client::DockerClient, ui};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExecCommandSpec {
    pub(crate) command: Vec<String>,
    pub(crate) user: Option<String>,
    pub(crate) working_dir: Option<String>,
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) redactions: Vec<String>,
    pub(crate) tty: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExecOutput {
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    pub(crate) exit_code: i64,
}

/// Runs exec to completion and returns captured output after requiring a zero exit code.
pub(crate) async fn exec_capture(
    client: &DockerClient,
    container: &str,
    spec: &ExecCommandSpec,
) -> Result<ExecOutput> {
    let output = exec_capture_output(client, container, spec).await?;

    ensure_success_output_with_redactions(container, &spec.command, &spec.redactions, &output)?;

    Ok(output)
}

/// Runs exec to completion and returns captured output with its exit code.
pub(crate) async fn exec_capture_output(
    client: &DockerClient,
    container: &str,
    spec: &ExecCommandSpec,
) -> Result<ExecOutput> {
    validate_exec_spec(spec)?;

    client.cli().exec_capture(container, spec).await
}

/// Runs exec attached to this process stdio and returns the command exit status.
pub(crate) async fn exec_attach_stdio(
    client: &DockerClient,
    container: &str,
    spec: &ExecCommandSpec,
) -> Result<i64> {
    validate_exec_spec(spec)?;

    client.cli().exec_attached_status(container, spec).await
}

/// Starts exec in detached mode and returns only whether Docker accepted the start request.
pub(crate) async fn exec_detached(
    client: &DockerClient,
    container: &str,
    spec: &ExecCommandSpec,
) -> Result<()> {
    validate_exec_spec(spec)?;

    client.cli().exec_detached(container, spec).await
}

pub(crate) async fn run_attached_exec_stdio(
    client: &DockerClient,
    container: &str,
    spec: &ExecCommandSpec,
) -> Result<i64> {
    exec_attach_stdio(client, container, spec).await
}

pub(crate) async fn resolve_exec_env(
    client: &DockerClient,
    container: &str,
    user: &str,
    user_shell: Option<&str>,
    remote_env: &BTreeMap<String, String>,
    user_env_probe: Option<ResolvedUserEnvProbe>,
) -> Result<BTreeMap<String, String>> {
    let Some(command) =
        user_env_probe_command(effective_user_env_probe(user_env_probe), user_shell)
    else {
        return Ok(remote_env.clone());
    };

    let output = match exec_capture(
        client,
        container,
        &ExecCommandSpec {
            command,
            user: Some(user.to_owned()),
            working_dir: None,
            env: BTreeMap::new(),
            redactions: Vec::new(),
            tty: false,
        },
    )
    .await
    {
        Ok(output) => output,
        Err(error) => {
            ui::warn(&format!(
                "User environment probe failed in container {container}; continuing without probed environment: {error:#}"
            ));
            return Ok(remote_env.clone());
        }
    };
    let stdout = match String::from_utf8(output.stdout) {
        Ok(stdout) => stdout,
        Err(error) => {
            ui::warn(&format!(
                "User environment probe returned non-UTF-8 output in container {container}; continuing without probed environment: {error}"
            ));
            return Ok(remote_env.clone());
        }
    };
    let probe_env = match parse_env_probe_output(&stdout) {
        Ok(probe_env) => probe_env,
        Err(error) => {
            ui::warn(&format!(
                "User environment probe output could not be parsed in container {container}; continuing without probed environment: {error:#}"
            ));
            return Ok(remote_env.clone());
        }
    };

    Ok(merge_probe_env(probe_env, remote_env))
}

pub(crate) fn effective_user_env_probe(
    user_env_probe: Option<ResolvedUserEnvProbe>,
) -> ResolvedUserEnvProbe {
    user_env_probe.unwrap_or(ResolvedUserEnvProbe::LoginInteractiveShell)
}

pub(crate) fn ensure_success_output(
    container: &str,
    command: &[String],
    output: &ExecOutput,
) -> Result<()> {
    ensure_success_exit_code(container, command, &[], output.exit_code, output)
}

fn ensure_success_output_with_redactions(
    container: &str,
    command: &[String],
    redactions: &[String],
    output: &ExecOutput,
) -> Result<()> {
    ensure_success_exit_code(container, command, redactions, output.exit_code, output)
}

fn ensure_success_exit_code(
    container: &str,
    command: &[String],
    redactions: &[String],
    exit_code: i64,
    output: &ExecOutput,
) -> Result<()> {
    if exit_code == 0 {
        return Ok(());
    }

    bail!(
        "Docker exec failed in container {container}: command `{}` exited with exit code {exit_code}. stdout tail: `{}` stderr tail: `{}`",
        redact_values(&command_display(command), redactions),
        redact_values(&output_tail(&output.stdout), redactions),
        redact_values(&output_tail(&output.stderr), redactions),
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

fn command_display(command: &[String]) -> String {
    command.join(" ")
}

fn output_tail(output: &[u8]) -> String {
    const MAX_TAIL_BYTES: usize = 4096;

    let start = output.len().saturating_sub(MAX_TAIL_BYTES);
    String::from_utf8_lossy(&output[start..]).trim().to_owned()
}

fn redact_values(value: &str, redactions: &[String]) -> String {
    redactions
        .iter()
        .filter(|secret| !secret.is_empty())
        .fold(value.to_owned(), |redacted, secret| {
            redacted.replace(secret, "[REDACTED]")
        })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::config::resolved::ResolvedUserEnvProbe;

    use super::{
        ExecOutput, ensure_success_output, merge_probe_env, parse_env_probe_output,
        user_env_probe_command,
    };

    #[test]
    fn user_env_probe_command_uses_requested_shell_flags() {
        assert_eq!(
            user_env_probe_command(ResolvedUserEnvProbe::LoginShell, Some("/bin/bash")).unwrap(),
            vec!["/bin/bash", "-lc", "env"]
        );
        assert_eq!(
            user_env_probe_command(ResolvedUserEnvProbe::InteractiveShell, None).unwrap(),
            vec!["/bin/sh", "-ic", "env"]
        );
        assert_eq!(
            user_env_probe_command(ResolvedUserEnvProbe::None, Some("/bin/zsh")),
            None
        );
    }

    #[test]
    fn parse_env_probe_output_keeps_values_with_equals() {
        let env = parse_env_probe_output("PATH=/usr/bin\nTOKEN=a=b\nINVALID\n").unwrap();

        assert_eq!(env.get("PATH").map(String::as_str), Some("/usr/bin"));
        assert_eq!(env.get("TOKEN").map(String::as_str), Some("a=b"));
        assert!(!env.contains_key("INVALID"));
    }

    #[test]
    fn merge_probe_env_lets_remote_env_override_probe() {
        let probe = BTreeMap::from([
            ("PATH".to_owned(), "/usr/bin".to_owned()),
            ("LANG".to_owned(), "C".to_owned()),
        ]);
        let remote = BTreeMap::from([("LANG".to_owned(), "en_US.UTF-8".to_owned())]);

        let merged = merge_probe_env(probe, &remote);

        assert_eq!(merged.get("PATH").map(String::as_str), Some("/usr/bin"));
        assert_eq!(merged.get("LANG").map(String::as_str), Some("en_US.UTF-8"));
    }

    #[test]
    fn failed_exec_output_includes_command_and_tails() {
        let output = ExecOutput {
            stdout: b"ok".to_vec(),
            stderr: b"bad".to_vec(),
            exit_code: 2,
        };
        let error = ensure_success_output("container", &["false".to_owned()], &output).unwrap_err();

        assert!(error.to_string().contains("Docker exec failed"));
        assert!(error.to_string().contains("false"));
        assert!(error.to_string().contains("bad"));
    }

    #[test]
    fn failed_exec_output_redacts_secret_values() {
        let output = ExecOutput {
            stdout: b"stdout secret-token".to_vec(),
            stderr: b"stderr secret-token".to_vec(),
            exit_code: 2,
        };
        let command = vec![
            "/bin/sh".to_owned(),
            "-lc".to_owned(),
            "echo secret-token".to_owned(),
        ];
        let error = super::ensure_success_output_with_redactions(
            "container",
            &command,
            &["secret-token".to_owned()],
            &output,
        )
        .unwrap_err();
        let message = error.to_string();

        assert!(!message.contains("secret-token"));
        assert!(message.contains("[REDACTED]"));
    }
}
