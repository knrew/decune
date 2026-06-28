use std::path::Path;

use anyhow::Result;

use crate::{
    config::resolved::ResolvedConfig,
    state::{LifecycleCompletion, LifecycleState},
};

use super::{
    command::{run_host_lifecycle_command, run_lifecycle_stage},
    context::PreparedLifecycleRunContext,
    hooks::{run_hook_stage, run_hook_stage_without_container},
    sequence::{CREATION_LIFECYCLE_STAGES, after_hook_stage, has_pending_creation_lifecycle},
    types::{HookStage, LifecycleRunPath, LifecycleStage},
};

pub(crate) fn run_host_initialize_lifecycle(
    config: &ResolvedConfig,
    workspace_root: &Path,
) -> Result<()> {
    run_hook_stage_without_container(config, workspace_root, HookStage::BeforeInitialize)?;
    run_host_lifecycle_command(config, workspace_root, LifecycleStage::Initialize)?;
    run_hook_stage_without_container(config, workspace_root, HookStage::AfterInitialize)?;

    Ok(())
}

pub(crate) async fn run_container_start_lifecycle(
    path: LifecycleRunPath,
    context: &PreparedLifecycleRunContext<'_>,
    state: &mut LifecycleState,
    mut save_state: impl FnMut(&LifecycleState) -> Result<()>,
) -> Result<()> {
    let pending_creation = has_pending_creation_lifecycle(*state);
    match path {
        LifecycleRunPath::New | LifecycleRunPath::Started => {
            run_pending_creation_lifecycle(context, state, &mut save_state).await?;
            run_container_stage(
                context,
                HookStage::BeforePostStart,
                LifecycleStage::PostStart,
            )
            .await?;
        }
        LifecycleRunPath::Running => {
            run_pending_creation_lifecycle(context, state, &mut save_state).await?;
            if pending_creation {
                run_container_stage(
                    context,
                    HookStage::BeforePostStart,
                    LifecycleStage::PostStart,
                )
                .await?;
            } else {
                crate::ui::skipped(LifecycleStage::PostStart.property_name());
            }
        }
    }

    Ok(())
}

async fn run_pending_creation_lifecycle(
    context: &PreparedLifecycleRunContext<'_>,
    state: &mut LifecycleState,
    save_state: &mut impl FnMut(&LifecycleState) -> Result<()>,
) -> Result<()> {
    for stage in CREATION_LIFECYCLE_STAGES {
        run_container_creation_stage(
            context,
            state,
            stage.completion,
            stage.before_hook,
            stage.lifecycle_stage,
            save_state,
        )
        .await?;
    }

    Ok(())
}

async fn run_container_creation_stage(
    context: &PreparedLifecycleRunContext<'_>,
    state: &mut LifecycleState,
    completion: LifecycleCompletion,
    before_hook: HookStage,
    lifecycle_stage: LifecycleStage,
    save_state: &mut impl FnMut(&LifecycleState) -> Result<()>,
) -> Result<()> {
    if state.is_completed(completion) {
        crate::ui::skipped(lifecycle_stage.property_name());
        return Ok(());
    }

    if !state.is_command_completed(completion) {
        run_hook_stage(context, before_hook).await?;
        run_lifecycle_stage(context, lifecycle_stage).await?;
        state.mark_command_completed(completion);
        save_state(state)?;
    }
    if !state.is_after_hook_completed(completion) {
        run_hook_stage(context, after_hook_stage(before_hook)?).await?;
        state.mark_after_hook_completed(completion);
        save_state(state)?;
    }

    Ok(())
}

pub(crate) async fn run_attach_lifecycle(context: &PreparedLifecycleRunContext<'_>) -> Result<()> {
    run_container_stage(
        context,
        HookStage::BeforePostAttach,
        LifecycleStage::PostAttach,
    )
    .await?;

    Ok(())
}

async fn run_container_stage(
    context: &PreparedLifecycleRunContext<'_>,
    before_hook: HookStage,
    lifecycle_stage: LifecycleStage,
) -> Result<()> {
    run_hook_stage(context, before_hook).await?;
    run_lifecycle_stage(context, lifecycle_stage).await?;
    run_hook_stage(context, after_hook_stage(before_hook)?).await?;

    Ok(())
}
