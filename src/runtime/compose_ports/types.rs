use std::collections::BTreeMap;

use serde_json::Value as JsonValue;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposePortEntry {
    pub(crate) service: String,
    pub(crate) entry_index: usize,
    pub(crate) service_replica_count: u64,
    pub(crate) service_uses_host_network: bool,
    pub(crate) syntax: ComposePortSyntax,
    pub(crate) target_port: Option<u16>,
    pub(crate) published_host_port: ComposePublishedHostPort,
    pub(crate) host_ip: ComposePortHostIp,
    pub(crate) protocol: ComposePortProtocol,
    pub(crate) original_fields: BTreeMap<String, JsonValue>,
    pub(crate) eligibility: ComposePortEligibility,
    pub(crate) unsupported_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ComposePortSyntax {
    EffectiveObject,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ComposePublishedHostPort {
    Single(u16),
    None,
    Range(String),
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ComposePortHostIp {
    Omitted,
    Explicit(String),
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ComposePortProtocol {
    Tcp,
    Udp,
    Other(String),
    Invalid(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ComposePortEligibility {
    EligibleFixedTcp,
    UnsupportedContainerOnly,
    UnsupportedUdp,
    UnsupportedRange,
    UnsupportedInvalid,
    UnsupportedOther,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ComposePublishedPortStartupDiagnostics<'a> {
    pub(crate) input: &'a ComposePublishedPortPlanningInput,
    pub(crate) plan: &'a ComposePublishedPortPlan,
    pub(crate) relocation_enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposePublishedPortPlanningInput {
    pub(crate) services: crate::runtime::compose_ports::ComposeActiveServiceSet,
    pub(crate) port_entries: Vec<ComposePortEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ComposePublishedPortPlan {
    pub(crate) entries: Vec<ComposePublishedPortPlanEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposePublishedPortPlanEntry {
    pub(crate) service: String,
    pub(crate) port_entry_index: usize,
    pub(crate) source: ComposePublishedPortPlanSource,
    pub(crate) kind: ComposePublishedPortPlanEntryType,
    pub(crate) target_port: u16,
    pub(crate) protocol: ComposePortProtocol,
    pub(crate) requested: ComposePublishedPortEndpoint,
    pub(crate) planned: ComposePublishedPortEndpoint,
    pub(crate) relocated: bool,
    pub(crate) allocation_reason: ComposePublishedPortAllocationReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposePublishedPortReservation {
    pub(crate) service: String,
    pub(crate) target_port: u16,
    pub(crate) protocol: ComposePortProtocol,
    pub(crate) endpoint: ComposePublishedPortEndpoint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ComposePublishedPortPlanSource {
    Compose,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ComposePublishedPortPlanEntryType {
    Published,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposePublishedPortEndpoint {
    pub(crate) host_ip_kind: ComposePublishedPortHostIpKind,
    pub(crate) host_ip_value: Option<String>,
    pub(crate) host_port: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ComposePublishedPortHostIpKind {
    Omitted,
    Explicit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ComposePublishedPortAllocationReason {
    Available,
    Reserved,
    Unavailable,
}
