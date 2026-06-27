mod adapter;
mod capabilities;
mod command_plan;
mod config;
mod introspector;
mod override_file;
mod project_plan;
mod ps;

pub(crate) use adapter::DockerComposeCli;
#[allow(unused_imports)]
pub(crate) use capabilities::ComposeCliCapabilities;
#[allow(unused_imports)]
pub(crate) use command_plan::{
    ComposeBuildOptions, ComposeCleanupPlan, ComposeCommandPlan, ComposeDownOptions,
    ComposeLifecyclePlan, ComposePullOptions, ComposeStopOptions, ComposeUpOptions,
};
#[allow(unused_imports)]
pub(crate) use config::{
    ComposeConfigModel, ComposeConfigOutput, ComposeConfigService, ComposePrimaryImage,
    ComposePrimaryImageResolver, ComposeServiceValidation,
};
pub(crate) use introspector::ComposeIntrospector;
#[allow(unused_imports)]
pub(crate) use override_file::{
    ComposeOverrideMount, ComposeOverridePatch, ComposeOverridePortEntry,
    ComposeOverrideServicePatch, write_compose_override,
};
pub(crate) use project_plan::ComposeProjectPlan;
#[allow(unused_imports)]
pub(crate) use ps::{ComposePsContainer, ComposePublishedPort, resolve_compose_container};

#[cfg(test)]
use command_plan::{
    compose_build_command, compose_down_command, compose_pull_command, compose_stop_command,
    compose_up_command,
};
#[cfg(test)]
use ps::parse_compose_ps_json;

#[cfg(test)]
mod tests;
