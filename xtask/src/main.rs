#![allow(
    clippy::multiple_crate_versions,
    reason = "Temporary allow while strict clippy policy is introduced; code fixes will follow separately."
)]

mod cli;
mod command;
mod container_tools;
mod dist;
mod hash;
mod install;
mod paths;
mod release;
mod test_runner;

use anyhow::Result;
use clap::Parser;

use crate::{
    cli::{Args, XtaskCommand},
    container_tools::{
        build_container_tools, check_container_tools, resolve_container_tools_bundle_arg,
    },
    dist::{checksum, dist, release_manifest},
    install::install,
    paths::{resolve_dist_dir, workspace_root},
    release::release_preflight,
    test_runner::{compose_integration, workspace_test},
};

fn main() -> Result<()> {
    let args = Args::parse();
    let workspace = workspace_root()?;
    match args.command {
        XtaskCommand::BuildContainerTools { out, locked } => {
            let out = resolve_container_tools_bundle_arg(&workspace, out.as_deref());
            build_container_tools(&workspace, &out, locked)
        }
        XtaskCommand::CheckContainerTools { dir } => {
            let dir = resolve_container_tools_bundle_arg(&workspace, dir.as_deref());
            check_container_tools(&dir)?;
            Ok(())
        }
        XtaskCommand::ComposeIntegration { release } => compose_integration(&workspace, release),
        XtaskCommand::WorkspaceTest { release } => workspace_test(&workspace, release),
        XtaskCommand::Install {
            locked,
            force,
            root,
        } => install(&workspace, locked, force, root.as_deref()),
        XtaskCommand::Dist {
            target,
            version,
            locked,
            dist_dir,
            container_tools_dir,
        } => dist(
            &workspace,
            &target,
            &version,
            locked,
            dist_dir.as_deref(),
            container_tools_dir.as_deref(),
        ),
        XtaskCommand::Checksum { dist_dir, version } => checksum(
            &resolve_dist_dir(&workspace, dist_dir.as_deref()),
            version.as_deref(),
        ),
        XtaskCommand::ReleaseManifest { dist_dir, version } => {
            release_manifest(&resolve_dist_dir(&workspace, dist_dir.as_deref()), &version)
        }
        XtaskCommand::ReleasePreflight { tag, version } => {
            release_preflight(&workspace, &tag, &version)
        }
    }
}
