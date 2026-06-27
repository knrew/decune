mod classify;
pub(in crate::runtime) mod diagnostics;
mod endpoint;
mod input;
mod overrides;
mod planning;
mod startup_failure;
#[cfg(test)]
mod test_support;
mod types;

pub(crate) use classify::classify_compose_published_ports;
pub(crate) use diagnostics::{
    ComposePublishedPortDiagnostic, compose_published_port_invalid_config_error,
    validate_compose_published_port_diagnostics,
};
pub(crate) use endpoint::{compose_port_protocol_name, compose_published_port_endpoint_display};
pub(crate) use input::{ComposeActiveServiceSet, compose_published_port_planning_input};
pub(crate) use overrides::{ComposePublishedPortOverride, compose_published_port_override};
pub(crate) use planning::{
    compose_published_port_plan_has_relocations, compose_published_port_runtime_plan,
    plan_compose_published_ports_with_existing_project,
};
pub(crate) use startup_failure::classify_compose_published_port_startup_failure;
pub(crate) use types::{
    ComposePortEligibility, ComposePortEntry, ComposePortHostIp, ComposePortProtocol,
    ComposePortSyntax, ComposePublishedHostPort, ComposePublishedPortAllocationReason,
    ComposePublishedPortEndpoint, ComposePublishedPortHostIpKind, ComposePublishedPortPlan,
    ComposePublishedPortPlanEntry, ComposePublishedPortPlanEntryType,
    ComposePublishedPortPlanSource, ComposePublishedPortPlanningInput,
    ComposePublishedPortReservation, ComposePublishedPortStartupDiagnostics,
};
