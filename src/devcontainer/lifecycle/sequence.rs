use anyhow::{Result, bail};

use crate::state::{LifecycleCompletion, LifecycleState};

use super::types::{HookStage, LifecycleStage};
#[cfg(test)]
use super::types::{LifecycleRunPath, LifecycleStep};

pub(in crate::devcontainer::lifecycle) struct CreationLifecycleStage {
    pub(in crate::devcontainer::lifecycle) completion: LifecycleCompletion,
    pub(in crate::devcontainer::lifecycle) before_hook: HookStage,
    pub(in crate::devcontainer::lifecycle) lifecycle_stage: LifecycleStage,
}

pub(in crate::devcontainer::lifecycle) const CREATION_LIFECYCLE_STAGES:
    &[CreationLifecycleStage] = &[
    CreationLifecycleStage {
        completion: LifecycleCompletion::OnCreate,
        before_hook: HookStage::BeforeOnCreate,
        lifecycle_stage: LifecycleStage::OnCreate,
    },
    CreationLifecycleStage {
        completion: LifecycleCompletion::UpdateContent,
        before_hook: HookStage::BeforeUpdateContent,
        lifecycle_stage: LifecycleStage::UpdateContent,
    },
    CreationLifecycleStage {
        completion: LifecycleCompletion::PostCreate,
        before_hook: HookStage::BeforePostCreate,
        lifecycle_stage: LifecycleStage::PostCreate,
    },
];

pub(in crate::devcontainer::lifecycle) fn has_pending_creation_lifecycle(
    state: LifecycleState,
) -> bool {
    CREATION_LIFECYCLE_STAGES
        .iter()
        .any(|stage| !state.is_completed(stage.completion))
}

#[cfg(test)]
pub(crate) fn lifecycle_plan(path: LifecycleRunPath) -> Vec<LifecycleStep> {
    let mut plan = container_start_lifecycle_plan(path);
    plan.extend(attach_lifecycle_plan());
    plan
}
#[cfg(test)]
pub(crate) fn container_start_lifecycle_plan(path: LifecycleRunPath) -> Vec<LifecycleStep> {
    let state = match path {
        LifecycleRunPath::New => LifecycleState::default(),
        LifecycleRunPath::Started | LifecycleRunPath::Running => LifecycleState::all_completed(),
    };
    container_start_lifecycle_plan_with_state(path, &state)
}

#[cfg(test)]
pub(crate) fn container_start_lifecycle_plan_with_state(
    path: LifecycleRunPath,
    state: &LifecycleState,
) -> Vec<LifecycleStep> {
    let mut steps = match path {
        LifecycleRunPath::New => {
            vec![
                LifecycleStep::Hooks(HookStage::BeforeInitialize),
                LifecycleStep::Lifecycle(LifecycleStage::Initialize),
                LifecycleStep::Hooks(HookStage::AfterInitialize),
                LifecycleStep::ImagePreparation,
                LifecycleStep::ContainerCreate,
                LifecycleStep::ContainerStart,
                LifecycleStep::HostDaemonStart,
                LifecycleStep::DecuneSetup,
            ]
        }
        LifecycleRunPath::Started => vec![
            LifecycleStep::Hooks(HookStage::BeforeInitialize),
            LifecycleStep::Lifecycle(LifecycleStage::Initialize),
            LifecycleStep::Hooks(HookStage::AfterInitialize),
            LifecycleStep::ContainerStart,
            LifecycleStep::HostDaemonStart,
            LifecycleStep::DecuneSetup,
        ],
        LifecycleRunPath::Running => vec![
            LifecycleStep::Hooks(HookStage::BeforeInitialize),
            LifecycleStep::Lifecycle(LifecycleStage::Initialize),
            LifecycleStep::Hooks(HookStage::AfterInitialize),
            LifecycleStep::HostDaemonStart,
            LifecycleStep::DecuneSetup,
        ],
    };

    push_pending_creation_lifecycle_steps(&mut steps, *state);
    if path != LifecycleRunPath::Running || has_pending_creation_lifecycle(*state) {
        steps.extend([
            LifecycleStep::Hooks(HookStage::BeforePostStart),
            LifecycleStep::Lifecycle(LifecycleStage::PostStart),
            LifecycleStep::Hooks(HookStage::AfterPostStart),
        ]);
    }

    steps
}

#[cfg(test)]
fn push_pending_creation_lifecycle_steps(steps: &mut Vec<LifecycleStep>, state: LifecycleState) {
    for stage in CREATION_LIFECYCLE_STAGES {
        push_creation_lifecycle_steps(
            steps,
            state,
            stage.completion,
            stage.before_hook,
            stage.lifecycle_stage,
        );
    }
}

#[cfg(test)]
fn push_creation_lifecycle_steps(
    steps: &mut Vec<LifecycleStep>,
    state: LifecycleState,
    completion: LifecycleCompletion,
    before_hook: HookStage,
    lifecycle_stage: LifecycleStage,
) {
    if state.is_completed(completion) {
        return;
    }

    if !state.is_command_completed(completion) {
        steps.extend([
            LifecycleStep::Hooks(before_hook),
            LifecycleStep::Lifecycle(lifecycle_stage),
        ]);
    }
    if !state.is_after_hook_completed(completion) {
        steps.push(LifecycleStep::Hooks(
            after_hook_stage(before_hook).expect("creation hook has after hook"),
        ));
    }
}
#[cfg(test)]
pub(crate) fn attach_lifecycle_plan() -> Vec<LifecycleStep> {
    vec![
        LifecycleStep::PortForwardingStart,
        LifecycleStep::Hooks(HookStage::BeforePostAttach),
        LifecycleStep::Lifecycle(LifecycleStage::PostAttach),
        LifecycleStep::Hooks(HookStage::AfterPostAttach),
        LifecycleStep::ShellAttach,
    ]
}

pub(in crate::devcontainer::lifecycle) fn after_hook_stage(
    before_hook: HookStage,
) -> Result<HookStage> {
    match before_hook {
        HookStage::BeforeOnCreate => Ok(HookStage::AfterOnCreate),
        HookStage::BeforeUpdateContent => Ok(HookStage::AfterUpdateContent),
        HookStage::BeforePostCreate => Ok(HookStage::AfterPostCreate),
        HookStage::BeforePostStart => Ok(HookStage::AfterPostStart),
        HookStage::BeforePostAttach => Ok(HookStage::AfterPostAttach),
        HookStage::BeforeInitialize | HookStage::AfterInitialize => {
            bail!("Hook stage does not have a container after hook")
        }
        HookStage::AfterOnCreate
        | HookStage::AfterUpdateContent
        | HookStage::AfterPostCreate
        | HookStage::AfterPostStart
        | HookStage::AfterPostAttach => bail!("Hook stage is already an after hook"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_plan_for_new_container_matches_documented_order() {
        assert_eq!(
            lifecycle_plan(LifecycleRunPath::New),
            vec![
                LifecycleStep::Hooks(HookStage::BeforeInitialize),
                LifecycleStep::Lifecycle(LifecycleStage::Initialize),
                LifecycleStep::Hooks(HookStage::AfterInitialize),
                LifecycleStep::ImagePreparation,
                LifecycleStep::ContainerCreate,
                LifecycleStep::ContainerStart,
                LifecycleStep::HostDaemonStart,
                LifecycleStep::DecuneSetup,
                LifecycleStep::Hooks(HookStage::BeforeOnCreate),
                LifecycleStep::Lifecycle(LifecycleStage::OnCreate),
                LifecycleStep::Hooks(HookStage::AfterOnCreate),
                LifecycleStep::Hooks(HookStage::BeforeUpdateContent),
                LifecycleStep::Lifecycle(LifecycleStage::UpdateContent),
                LifecycleStep::Hooks(HookStage::AfterUpdateContent),
                LifecycleStep::Hooks(HookStage::BeforePostCreate),
                LifecycleStep::Lifecycle(LifecycleStage::PostCreate),
                LifecycleStep::Hooks(HookStage::AfterPostCreate),
                LifecycleStep::Hooks(HookStage::BeforePostStart),
                LifecycleStep::Lifecycle(LifecycleStage::PostStart),
                LifecycleStep::Hooks(HookStage::AfterPostStart),
                LifecycleStep::PortForwardingStart,
                LifecycleStep::Hooks(HookStage::BeforePostAttach),
                LifecycleStep::Lifecycle(LifecycleStage::PostAttach),
                LifecycleStep::Hooks(HookStage::AfterPostAttach),
                LifecycleStep::ShellAttach,
            ]
        );
    }

    #[test]
    fn lifecycle_plan_for_existing_paths_matches_documented_order() {
        assert_eq!(
            lifecycle_plan(LifecycleRunPath::Started),
            vec![
                LifecycleStep::Hooks(HookStage::BeforeInitialize),
                LifecycleStep::Lifecycle(LifecycleStage::Initialize),
                LifecycleStep::Hooks(HookStage::AfterInitialize),
                LifecycleStep::ContainerStart,
                LifecycleStep::HostDaemonStart,
                LifecycleStep::DecuneSetup,
                LifecycleStep::Hooks(HookStage::BeforePostStart),
                LifecycleStep::Lifecycle(LifecycleStage::PostStart),
                LifecycleStep::Hooks(HookStage::AfterPostStart),
                LifecycleStep::PortForwardingStart,
                LifecycleStep::Hooks(HookStage::BeforePostAttach),
                LifecycleStep::Lifecycle(LifecycleStage::PostAttach),
                LifecycleStep::Hooks(HookStage::AfterPostAttach),
                LifecycleStep::ShellAttach,
            ]
        );
        assert_eq!(
            lifecycle_plan(LifecycleRunPath::Running),
            vec![
                LifecycleStep::Hooks(HookStage::BeforeInitialize),
                LifecycleStep::Lifecycle(LifecycleStage::Initialize),
                LifecycleStep::Hooks(HookStage::AfterInitialize),
                LifecycleStep::HostDaemonStart,
                LifecycleStep::DecuneSetup,
                LifecycleStep::PortForwardingStart,
                LifecycleStep::Hooks(HookStage::BeforePostAttach),
                LifecycleStep::Lifecycle(LifecycleStage::PostAttach),
                LifecycleStep::Hooks(HookStage::AfterPostAttach),
                LifecycleStep::ShellAttach,
            ]
        );
    }

    #[test]
    fn lifecycle_start_phase_plans_exclude_attach_steps() {
        assert_eq!(
            container_start_lifecycle_plan(LifecycleRunPath::New),
            vec![
                LifecycleStep::Hooks(HookStage::BeforeInitialize),
                LifecycleStep::Lifecycle(LifecycleStage::Initialize),
                LifecycleStep::Hooks(HookStage::AfterInitialize),
                LifecycleStep::ImagePreparation,
                LifecycleStep::ContainerCreate,
                LifecycleStep::ContainerStart,
                LifecycleStep::HostDaemonStart,
                LifecycleStep::DecuneSetup,
                LifecycleStep::Hooks(HookStage::BeforeOnCreate),
                LifecycleStep::Lifecycle(LifecycleStage::OnCreate),
                LifecycleStep::Hooks(HookStage::AfterOnCreate),
                LifecycleStep::Hooks(HookStage::BeforeUpdateContent),
                LifecycleStep::Lifecycle(LifecycleStage::UpdateContent),
                LifecycleStep::Hooks(HookStage::AfterUpdateContent),
                LifecycleStep::Hooks(HookStage::BeforePostCreate),
                LifecycleStep::Lifecycle(LifecycleStage::PostCreate),
                LifecycleStep::Hooks(HookStage::AfterPostCreate),
                LifecycleStep::Hooks(HookStage::BeforePostStart),
                LifecycleStep::Lifecycle(LifecycleStage::PostStart),
                LifecycleStep::Hooks(HookStage::AfterPostStart),
            ]
        );
        assert_eq!(
            container_start_lifecycle_plan(LifecycleRunPath::Started),
            vec![
                LifecycleStep::Hooks(HookStage::BeforeInitialize),
                LifecycleStep::Lifecycle(LifecycleStage::Initialize),
                LifecycleStep::Hooks(HookStage::AfterInitialize),
                LifecycleStep::ContainerStart,
                LifecycleStep::HostDaemonStart,
                LifecycleStep::DecuneSetup,
                LifecycleStep::Hooks(HookStage::BeforePostStart),
                LifecycleStep::Lifecycle(LifecycleStage::PostStart),
                LifecycleStep::Hooks(HookStage::AfterPostStart),
            ]
        );
        assert_eq!(
            container_start_lifecycle_plan(LifecycleRunPath::Running),
            vec![
                LifecycleStep::Hooks(HookStage::BeforeInitialize),
                LifecycleStep::Lifecycle(LifecycleStage::Initialize),
                LifecycleStep::Hooks(HookStage::AfterInitialize),
                LifecycleStep::HostDaemonStart,
                LifecycleStep::DecuneSetup,
            ]
        );
    }

    #[test]
    fn lifecycle_start_phase_plan_skips_completed_creation_stages_from_state() {
        assert_eq!(
            container_start_lifecycle_plan_with_state(
                LifecycleRunPath::New,
                &crate::state::LifecycleState {
                    on_create_completed: true,
                    after_on_create_completed: true,
                    update_content_completed: false,
                    after_update_content_completed: false,
                    post_create_completed: true,
                    after_post_create_completed: true,
                },
            ),
            vec![
                LifecycleStep::Hooks(HookStage::BeforeInitialize),
                LifecycleStep::Lifecycle(LifecycleStage::Initialize),
                LifecycleStep::Hooks(HookStage::AfterInitialize),
                LifecycleStep::ImagePreparation,
                LifecycleStep::ContainerCreate,
                LifecycleStep::ContainerStart,
                LifecycleStep::HostDaemonStart,
                LifecycleStep::DecuneSetup,
                LifecycleStep::Hooks(HookStage::BeforeUpdateContent),
                LifecycleStep::Lifecycle(LifecycleStage::UpdateContent),
                LifecycleStep::Hooks(HookStage::AfterUpdateContent),
                LifecycleStep::Hooks(HookStage::BeforePostStart),
                LifecycleStep::Lifecycle(LifecycleStage::PostStart),
                LifecycleStep::Hooks(HookStage::AfterPostStart),
            ]
        );
    }

    #[test]
    fn lifecycle_start_phase_plan_resumes_pending_after_hook_without_command() {
        assert_eq!(
            container_start_lifecycle_plan_with_state(
                LifecycleRunPath::Running,
                &crate::state::LifecycleState {
                    on_create_completed: true,
                    after_on_create_completed: false,
                    update_content_completed: false,
                    after_update_content_completed: false,
                    post_create_completed: false,
                    after_post_create_completed: false,
                },
            ),
            vec![
                LifecycleStep::Hooks(HookStage::BeforeInitialize),
                LifecycleStep::Lifecycle(LifecycleStage::Initialize),
                LifecycleStep::Hooks(HookStage::AfterInitialize),
                LifecycleStep::HostDaemonStart,
                LifecycleStep::DecuneSetup,
                LifecycleStep::Hooks(HookStage::AfterOnCreate),
                LifecycleStep::Hooks(HookStage::BeforeUpdateContent),
                LifecycleStep::Lifecycle(LifecycleStage::UpdateContent),
                LifecycleStep::Hooks(HookStage::AfterUpdateContent),
                LifecycleStep::Hooks(HookStage::BeforePostCreate),
                LifecycleStep::Lifecycle(LifecycleStage::PostCreate),
                LifecycleStep::Hooks(HookStage::AfterPostCreate),
                LifecycleStep::Hooks(HookStage::BeforePostStart),
                LifecycleStep::Lifecycle(LifecycleStage::PostStart),
                LifecycleStep::Hooks(HookStage::AfterPostStart),
            ]
        );
    }

    #[test]
    fn lifecycle_start_phase_plan_resumes_pending_creation_stages_when_running() {
        assert_eq!(
            container_start_lifecycle_plan_with_state(
                LifecycleRunPath::Running,
                &crate::state::LifecycleState {
                    on_create_completed: true,
                    after_on_create_completed: true,
                    update_content_completed: false,
                    after_update_content_completed: false,
                    post_create_completed: false,
                    after_post_create_completed: false,
                },
            ),
            vec![
                LifecycleStep::Hooks(HookStage::BeforeInitialize),
                LifecycleStep::Lifecycle(LifecycleStage::Initialize),
                LifecycleStep::Hooks(HookStage::AfterInitialize),
                LifecycleStep::HostDaemonStart,
                LifecycleStep::DecuneSetup,
                LifecycleStep::Hooks(HookStage::BeforeUpdateContent),
                LifecycleStep::Lifecycle(LifecycleStage::UpdateContent),
                LifecycleStep::Hooks(HookStage::AfterUpdateContent),
                LifecycleStep::Hooks(HookStage::BeforePostCreate),
                LifecycleStep::Lifecycle(LifecycleStage::PostCreate),
                LifecycleStep::Hooks(HookStage::AfterPostCreate),
                LifecycleStep::Hooks(HookStage::BeforePostStart),
                LifecycleStep::Lifecycle(LifecycleStage::PostStart),
                LifecycleStep::Hooks(HookStage::AfterPostStart),
            ]
        );
    }

    #[test]
    fn lifecycle_start_phase_plan_resumes_pending_creation_stages_when_starting_stopped() {
        assert_eq!(
            container_start_lifecycle_plan_with_state(
                LifecycleRunPath::Started,
                &crate::state::LifecycleState {
                    on_create_completed: true,
                    after_on_create_completed: true,
                    update_content_completed: true,
                    after_update_content_completed: true,
                    post_create_completed: false,
                    after_post_create_completed: false,
                },
            ),
            vec![
                LifecycleStep::Hooks(HookStage::BeforeInitialize),
                LifecycleStep::Lifecycle(LifecycleStage::Initialize),
                LifecycleStep::Hooks(HookStage::AfterInitialize),
                LifecycleStep::ContainerStart,
                LifecycleStep::HostDaemonStart,
                LifecycleStep::DecuneSetup,
                LifecycleStep::Hooks(HookStage::BeforePostCreate),
                LifecycleStep::Lifecycle(LifecycleStage::PostCreate),
                LifecycleStep::Hooks(HookStage::AfterPostCreate),
                LifecycleStep::Hooks(HookStage::BeforePostStart),
                LifecycleStep::Lifecycle(LifecycleStage::PostStart),
                LifecycleStep::Hooks(HookStage::AfterPostStart),
            ]
        );
    }

    #[test]
    fn lifecycle_attach_phase_plan_contains_forwarding_post_attach_and_shell_boundary() {
        assert_eq!(
            attach_lifecycle_plan(),
            vec![
                LifecycleStep::PortForwardingStart,
                LifecycleStep::Hooks(HookStage::BeforePostAttach),
                LifecycleStep::Lifecycle(LifecycleStage::PostAttach),
                LifecycleStep::Hooks(HookStage::AfterPostAttach),
                LifecycleStep::ShellAttach,
            ]
        );
    }
}
