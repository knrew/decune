use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

use crate::error;

#[derive(Debug, Parser)]
#[command(
    name = "decune",
    version,
    about = "Run dev containers from the command line."
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Create or start a dev container and attach to a shell.
    Up(WorkspaceArg),
    /// Recreate a dev container.
    Rebuild(WorkspaceArg),
    /// Stop a managed dev container.
    Down(WorkspaceArg),
    /// Remove managed dev container resources.
    Clean(WorkspaceArg),
}

#[derive(Debug, Args)]
struct WorkspaceArg {
    /// Workspace directory.
    #[arg(default_value = ".", value_name = "WORKSPACE")]
    workspace: PathBuf,
}

pub(crate) async fn run() -> Result<()> {
    let cli = Cli::parse();
    run_cli(cli).await
}

async fn run_cli(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Up(args) => not_implemented("up", args),
        Commands::Rebuild(args) => not_implemented("rebuild", args),
        Commands::Down(args) => not_implemented("down", args),
        Commands::Clean(args) => not_implemented("clean", args),
    }
}

fn not_implemented(command: &str, args: WorkspaceArg) -> Result<()> {
    let _workspace = args.workspace;
    Err(error::command_not_implemented(command))
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::Cli;

    #[test]
    fn clap_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
