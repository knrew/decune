use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::{
    command::{ChildCommand, cargo_command_with_container_tools, run_command_spec},
    container_tools::{
        BuildOutputMode, default_xtask_container_tools_bundle_dir, prepare_container_tools_bundle,
    },
};

pub(crate) fn install(
    workspace: &Path,
    locked: bool,
    force: bool,
    root: Option<&Path>,
) -> Result<()> {
    let plan = install_plan(workspace, locked, force, root);
    prepare_container_tools_bundle(
        workspace,
        &plan.bundle_dir,
        plan.bundle_locked,
        BuildOutputMode::Captured,
    )?;

    run_command_spec(
        plan.command,
        "Failed to install decune from local source checkout",
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InstallPlan {
    bundle_dir: PathBuf,
    bundle_locked: bool,
    command: ChildCommand,
}

fn install_plan(workspace: &Path, locked: bool, force: bool, root: Option<&Path>) -> InstallPlan {
    let bundle_dir = default_xtask_container_tools_bundle_dir(workspace);
    let command = install_cargo_command(workspace, locked, force, root, &bundle_dir);
    InstallPlan {
        bundle_dir,
        bundle_locked: locked,
        command,
    }
}

fn install_cargo_command(
    workspace: &Path,
    locked: bool,
    force: bool,
    root: Option<&Path>,
    bundle_dir: &Path,
) -> ChildCommand {
    let mut command = cargo_command_with_container_tools(workspace, bundle_dir).args([
        "install",
        "--path",
        ".",
        "--profile",
        "dist",
        "--bin",
        "decune",
    ]);
    if locked {
        command = command.arg("--locked");
    }
    if force {
        command = command.arg("--force");
    }
    if let Some(root) = root {
        command = command
            .arg("--root")
            .arg(root.to_string_lossy().into_owned());
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_cargo_command_installs_local_checkout_with_required_bundle() {
        let workspace = Path::new("/workspace/decune");
        let root = Path::new("/workspace/decune/target/install-smoke");
        let bundle_dir = default_xtask_container_tools_bundle_dir(workspace);

        let command = install_cargo_command(workspace, true, true, Some(root), &bundle_dir);

        assert_eq!(command.program, "cargo");
        assert_eq!(command.current_dir.as_deref(), Some(workspace));
        assert_eq!(
            command.args,
            [
                "install",
                "--path",
                ".",
                "--profile",
                "dist",
                "--bin",
                "decune",
                "--locked",
                "--force",
                "--root",
                "/workspace/decune/target/install-smoke",
            ]
        );
        assert_eq!(
            command.env.get("DECUNE_CONTAINER_TOOLS_BUNDLE"),
            Some(&std::ffi::OsString::from("required"))
        );
        assert_eq!(
            command.env.get("DECUNE_CONTAINER_TOOLS_BUNDLE_DIR"),
            Some(&bundle_dir.as_os_str().to_owned())
        );
    }

    #[test]
    fn install_plan_uses_same_locked_mode_for_bundle_build_and_install_command() {
        let workspace = Path::new("/workspace/decune");

        let unlocked = install_plan(workspace, false, false, None);

        assert!(!unlocked.bundle_locked);
        assert_eq!(
            unlocked.bundle_dir,
            PathBuf::from("/workspace/decune/target/decune-xtask/container-tools-bundle")
        );
        assert_eq!(
            unlocked
                .command
                .env
                .get("DECUNE_CONTAINER_TOOLS_BUNDLE_DIR"),
            Some(&unlocked.bundle_dir.as_os_str().to_owned())
        );
        assert!(!unlocked.command.args.iter().any(|arg| arg == "--locked"));

        let locked = install_plan(workspace, true, false, None);

        assert!(locked.bundle_locked);
        assert_eq!(
            locked.bundle_dir,
            PathBuf::from("/workspace/decune/target/decune-xtask/container-tools-bundle")
        );
        assert_eq!(
            locked.command.env.get("DECUNE_CONTAINER_TOOLS_BUNDLE_DIR"),
            Some(&locked.bundle_dir.as_os_str().to_owned())
        );
        assert!(locked.command.args.iter().any(|arg| arg == "--locked"));
    }
}
