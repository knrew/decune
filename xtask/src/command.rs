use std::{
    collections::BTreeMap,
    ffi::OsString,
    io::{self, Read, Write},
    path::PathBuf,
    process::{Command, ExitStatus, Stdio},
    thread,
};

use anyhow::{Context, Result, anyhow, bail};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChildCommand {
    pub(crate) program: String,
    pub(crate) current_dir: Option<PathBuf>,
    pub(crate) args: Vec<String>,
    pub(crate) env: BTreeMap<String, OsString>,
}

impl ChildCommand {
    pub(crate) fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            current_dir: None,
            args: Vec::new(),
            env: BTreeMap::new(),
        }
    }

    pub(crate) fn current_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.current_dir = Some(path.into());
        self
    }

    pub(crate) fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub(crate) fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub(crate) fn env(mut self, key: impl Into<String>, value: impl Into<OsString>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    fn into_command(self) -> Command {
        let mut command = Command::new(&self.program);
        if let Some(current_dir) = self.current_dir {
            command.current_dir(current_dir);
        }
        command.args(self.args);
        for (key, value) in self.env {
            command.env(key, value);
        }
        command
    }
}

pub(crate) fn cargo_command_with_container_tools(
    workspace: &std::path::Path,
    bundle_dir: &std::path::Path,
) -> ChildCommand {
    ChildCommand::new("cargo")
        .current_dir(workspace)
        .env("DECUNE_CONTAINER_TOOLS_BUNDLE", "required")
        .env("DECUNE_CONTAINER_TOOLS_BUNDLE_DIR", bundle_dir.as_os_str())
}

pub(crate) fn run_command_spec(command: ChildCommand, context: &str) -> Result<()> {
    run_command(command.into_command(), context)
}

pub(crate) fn run_command_spec_streaming(command: ChildCommand, context: &str) -> Result<()> {
    run_command_streaming(command.into_command(), context)
}

pub(crate) fn run_command(mut command: Command, context: &str) -> Result<()> {
    let output = command
        .output()
        .with_context(|| format!("{context}: failed to spawn command"))?;
    if output.status.success() {
        return Ok(());
    }
    bail!(
        "{}.\nstdout:\n{}\nstderr:\n{}",
        context,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

pub(crate) fn run_command_streaming(mut command: Command, context: &str) -> Result<()> {
    let status = command
        .status()
        .with_context(|| format!("{context}: failed to spawn command"))?;
    if status.success() {
        return Ok(());
    }
    bail!("{context}: command exited with {status}")
}

pub(crate) struct StreamedCommandOutput {
    pub(crate) status: ExitStatus,
    pub(crate) stderr: Vec<u8>,
}

pub(crate) fn stream_command_stderr(
    mut command: Command,
    context: &str,
) -> Result<StreamedCommandOutput> {
    let mut child = command
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("{context}: failed to spawn command"))?;
    let child_stderr = child
        .stderr
        .take()
        .with_context(|| format!("{context}: failed to capture command stderr"))?;
    let stderr_thread = thread::spawn(move || {
        let stderr = io::stderr();
        stream_and_capture(child_stderr, stderr.lock())
    });

    let status = child.wait();
    let stderr = stderr_thread
        .join()
        .map_err(|panic_payload| {
            let message = panic_payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| panic_payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("unknown panic payload");
            anyhow!("{context}: stderr forwarding thread panicked: {message}")
        })?
        .with_context(|| format!("{context}: failed to stream command stderr"))?;

    Ok(StreamedCommandOutput {
        status: status.with_context(|| format!("{context}: failed to wait for command"))?,
        stderr,
    })
}

fn stream_and_capture<R, W>(mut reader: R, mut writer: W) -> io::Result<Vec<u8>>
where
    R: Read,
    W: Write,
{
    let mut captured = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        writer.write_all(&buffer[..read])?;
        writer.flush()?;
        captured.extend_from_slice(&buffer[..read]);
    }
    Ok(captured)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_and_capture_forwards_and_retains_bytes() {
        let input = b"first line\nsecond line\n";
        let mut forwarded = Vec::new();

        let captured = stream_and_capture(input.as_slice(), &mut forwarded).unwrap();

        assert_eq!(forwarded, input);
        assert_eq!(captured, input);
    }

    #[test]
    fn streaming_command_reports_context_and_exit_status() {
        let mut command = Command::new("sh");
        command.args(["-c", "exit 7"]);

        let error = run_command_streaming(command, "Test command failed").unwrap_err();
        let message = error.to_string();

        assert!(message.contains("Test command failed"));
        assert!(message.contains('7'));
    }

    #[test]
    fn stream_command_stderr_captures_stderr_on_failure() {
        let mut command = Command::new("sh");
        command.args(["-c", "echo err message >&2; exit 3"]);

        let output = stream_command_stderr(command, "test context").unwrap();

        assert!(!output.status.success());
        assert_eq!(output.status.code(), Some(3));
        assert!(String::from_utf8_lossy(&output.stderr).contains("err message"));
    }

    #[test]
    fn stream_command_stderr_captures_stderr_on_success() {
        let mut command = Command::new("sh");
        command.args(["-c", "echo ok >&2"]);

        let output = stream_command_stderr(command, "test context").unwrap();

        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("ok"));
    }
}
