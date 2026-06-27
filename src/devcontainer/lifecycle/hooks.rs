use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::{
    config::{
        resolved::{ResolvedConfig, ResolvedHook},
        types::HookLocation,
    },
    docker::user::ResolvedRemoteUser,
};

use super::{
    command::{hook_command_argv, run_container_process, run_host_process},
    context::PreparedLifecycleRunContext,
    types::HookStage,
};

pub(in crate::devcontainer::lifecycle) fn run_hook_stage_without_container(
    config: &ResolvedConfig,
    workspace_root: &Path,
    stage: HookStage,
) -> Result<()> {
    let hooks = hooks_for_stage(config, stage);
    if !hooks.is_empty() {
        crate::ui::status("Running", &format!("{} hook", stage.property_name()));
    }
    for hook in hooks {
        let location = hook.location.unwrap_or_else(|| stage.default_location());
        if location != HookLocation::Host {
            bail!(
                "Hook {} must run on host before container creation",
                stage.property_name()
            );
        }
        run_host_hook(workspace_root, stage, hook)?;
    }

    Ok(())
}

pub(in crate::devcontainer::lifecycle) async fn run_hook_stage(
    context: &PreparedLifecycleRunContext<'_>,
    stage: HookStage,
) -> Result<()> {
    let hooks = hooks_for_stage(context.config, stage);
    if !hooks.is_empty() {
        crate::ui::status("Running", &format!("{} hook", stage.property_name()));
    }
    for hook in hooks {
        match hook.location.unwrap_or_else(|| stage.default_location()) {
            HookLocation::Host => run_host_hook(context.workspace_root, stage, hook)?,
            HookLocation::Container => run_container_hook(context, stage, hook).await?,
        }
    }

    Ok(())
}

fn hooks_for_stage(config: &ResolvedConfig, stage: HookStage) -> &[ResolvedHook] {
    match stage {
        HookStage::BeforeInitialize => &config.hooks.before_initialize,
        HookStage::AfterInitialize => &config.hooks.after_initialize,
        HookStage::BeforeOnCreate => &config.hooks.before_on_create,
        HookStage::AfterOnCreate => &config.hooks.after_on_create,
        HookStage::BeforeUpdateContent => &config.hooks.before_update_content,
        HookStage::AfterUpdateContent => &config.hooks.after_update_content,
        HookStage::BeforePostCreate => &config.hooks.before_post_create,
        HookStage::AfterPostCreate => &config.hooks.after_post_create,
        HookStage::BeforePostStart => &config.hooks.before_post_start,
        HookStage::AfterPostStart => &config.hooks.after_post_start,
        HookStage::BeforePostAttach => &config.hooks.before_post_attach,
        HookStage::AfterPostAttach => &config.hooks.after_post_attach,
    }
}

fn run_host_hook(workspace_root: &Path, stage: HookStage, hook: &ResolvedHook) -> Result<()> {
    if hook.user.is_some() {
        bail!(
            "Host hook {} must not specify a container user",
            stage.property_name()
        );
    }

    let argv = hook_command_argv(&hook.command, hook.shell);
    let workdir = host_hook_workdir(workspace_root, hook);
    run_host_process(stage.property_name(), &argv, &workdir)
}

async fn run_container_hook(
    context: &PreparedLifecycleRunContext<'_>,
    stage: HookStage,
    hook: &ResolvedHook,
) -> Result<()> {
    let argv = hook_command_argv(&hook.command, hook.shell);
    let user = hook_user(&context.remote_user, hook);
    let workdir = hook
        .workdir
        .clone()
        .unwrap_or_else(|| context.workspace_folder.to_owned());

    run_container_process(context, stage.property_name(), argv, Some((user, workdir))).await
}

pub(in crate::devcontainer::lifecycle) fn host_hook_workdir(
    workspace_root: &Path,
    hook: &ResolvedHook,
) -> PathBuf {
    let Some(workdir) = &hook.workdir else {
        return workspace_root.to_path_buf();
    };
    let path = PathBuf::from(workdir);
    if path.is_absolute() {
        path
    } else {
        workspace_root.join(path)
    }
}

fn hook_user(remote_user: &ResolvedRemoteUser, hook: &ResolvedHook) -> String {
    match hook.user.as_deref() {
        None | Some("remote") => remote_user.user.clone(),
        Some("root") => "root".to_owned(),
        Some(user) => user.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::config::{resolved::ResolvedHook, types::Command};

    #[test]
    fn host_hook_workdir_defaults_to_workspace_and_resolves_relative_paths() {
        let workspace_root = Path::new("/workspace/project");
        let default_hook = ResolvedHook {
            command: Command::Shell("true".to_owned()),
            location: None,
            user: None,
            shell: true,
            workdir: None,
        };
        let relative_hook = ResolvedHook {
            workdir: Some("scripts".to_owned()),
            ..default_hook.clone()
        };
        let absolute_hook = ResolvedHook {
            workdir: Some("/tmp".to_owned()),
            ..default_hook.clone()
        };

        assert_eq!(
            host_hook_workdir(workspace_root, &default_hook),
            workspace_root
        );
        assert_eq!(
            host_hook_workdir(workspace_root, &relative_hook),
            PathBuf::from("/workspace/project/scripts")
        );
        assert_eq!(
            host_hook_workdir(workspace_root, &absolute_hook),
            PathBuf::from("/tmp")
        );
    }
}
