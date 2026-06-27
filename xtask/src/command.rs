use std::{collections::BTreeMap, ffi::OsString, path::PathBuf, process::Command};

use anyhow::{Context, Result, bail};

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
