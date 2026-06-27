use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(author, version, about)]
pub(crate) struct Args {
    #[command(subcommand)]
    pub(crate) command: XtaskCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum XtaskCommand {
    BuildContainerTools {
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long)]
        locked: bool,
    },
    CheckContainerTools {
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    ComposeIntegration {
        #[arg(long)]
        release: bool,
    },
    WorkspaceTest {
        #[arg(long)]
        release: bool,
    },
    Install {
        #[arg(long)]
        locked: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        root: Option<PathBuf>,
    },
    Dist {
        #[arg(long)]
        target: String,
        #[arg(long)]
        version: String,
        #[arg(long)]
        locked: bool,
        #[arg(long)]
        dist_dir: Option<PathBuf>,
        #[arg(long)]
        container_tools_dir: Option<PathBuf>,
    },
    Checksum {
        #[arg(long)]
        dist_dir: Option<PathBuf>,
        #[arg(long)]
        version: Option<String>,
    },
    ReleaseManifest {
        #[arg(long)]
        dist_dir: Option<PathBuf>,
        #[arg(long)]
        version: String,
    },
    ReleasePreflight {
        #[arg(long)]
        tag: String,
        #[arg(long)]
        version: String,
    },
}
