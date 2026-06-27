use serde_json::Value as JsonValue;

use crate::{
    config::types::PortProtocol,
    docker::ports::ResolvedForwardPort,
    runtime::{
        compose_cli::ComposeConfigModel,
        compose_ports::{
            ComposePortEntry, ComposePublishedPortPlan, ComposePublishedPortPlanningInput,
            classify_compose_published_ports, compose_published_port_planning_input,
        },
    },
};

use super::planning::plan_compose_published_ports_with;

pub(super) fn model(value: JsonValue) -> ComposeConfigModel {
    serde_json::from_value(value).unwrap()
}

pub(super) fn entries(value: JsonValue) -> Vec<ComposePortEntry> {
    classify_compose_published_ports(&model(value))
}

pub(super) fn planning_input(
    value: JsonValue,
    primary_service: &str,
    selected_services: &[String],
) -> ComposePublishedPortPlanningInput {
    let model = model(value);
    let entries = classify_compose_published_ports(&model);
    compose_published_port_planning_input(&model, &entries, primary_service, selected_services)
}

pub(super) fn plan_with_availability(
    input: &ComposePublishedPortPlanningInput,
    unavailable_ports: &[u16],
) -> ComposePublishedPortPlan {
    plan_compose_published_ports_with(input, true, &[], &[], |_, port| {
        Ok(!unavailable_ports.contains(&port))
    })
    .unwrap()
}

pub(super) fn forward_port(host_ip: &str, host: u16, container: u16) -> ResolvedForwardPort {
    ResolvedForwardPort {
        service: None,
        container,
        requested_host: host,
        host,
        host_ip: host_ip.to_owned(),
        protocol: PortProtocol::Tcp,
        require_local: false,
        label: None,
    }
}
