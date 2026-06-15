use anyhow::{Result, bail};

use crate::{
    config::resolved::ResolvedDevcontainerSource,
    runtime::compose_cli::{ComposeIntrospector, ComposePsContainer},
    up::types::UpPlan,
};

pub(in crate::up) struct UpExecTarget {
    pub(in crate::up) id: String,
    pub(in crate::up) display_name: String,
}

pub(in crate::up) async fn resolve_up_exec_target(
    plan: &UpPlan,
    fallback_container: &str,
) -> Result<UpExecTarget> {
    let Some(compose_project) = &plan.compose_project else {
        return Ok(UpExecTarget {
            id: fallback_container.to_owned(),
            display_name: fallback_container.to_owned(),
        });
    };
    let Some(ResolvedDevcontainerSource::Compose(compose)) = &plan.config.devcontainer.source
    else {
        bail!("Docker Compose devcontainer source is missing for exec target resolution");
    };

    let container = ComposeIntrospector::default()
        .resolve_service_container(
            &compose_project.command_plan_with_generated_override(),
            &compose.service,
        )
        .await?;

    Ok(exec_target_from_compose_container(container))
}

fn exec_target_from_compose_container(container: ComposePsContainer) -> UpExecTarget {
    UpExecTarget {
        display_name: container.name.unwrap_or_else(|| container.id.clone()),
        id: container.id,
    }
}
