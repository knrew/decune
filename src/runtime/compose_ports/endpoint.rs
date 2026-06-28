use crate::runtime::compose_ports::{
    ComposePortEntry, ComposePortHostIp, ComposePortProtocol, ComposePublishedHostPort,
    ComposePublishedPortEndpoint, ComposePublishedPortHostIp,
};

use super::planning::ComposePublishedPortPlanError;

const OMITTED_HOST_IP_RESERVATION: &str = "0.0.0.0";

pub(super) const fn protocol_order(protocol: &ComposePortProtocol) -> u8 {
    match protocol {
        ComposePortProtocol::Tcp => 0,
        ComposePortProtocol::Udp => 1,
        ComposePortProtocol::Other(_) => 2,
        ComposePortProtocol::Invalid(_) => 3,
    }
}

pub(crate) fn compose_port_protocol_name(protocol: &ComposePortProtocol) -> &str {
    match protocol {
        ComposePortProtocol::Tcp => "tcp",
        ComposePortProtocol::Udp => "udp",
        ComposePortProtocol::Other(value) | ComposePortProtocol::Invalid(value) => value,
    }
}

pub(crate) fn compose_published_port_endpoint_display(
    endpoint: &ComposePublishedPortEndpoint,
) -> String {
    match &endpoint.host_ip {
        ComposePublishedPortHostIp::Omitted => {
            format!("<host_ip omitted>:{}", endpoint.host_port)
        }
        ComposePublishedPortHostIp::Explicit(value) => format!("{}:{}", value, endpoint.host_port),
    }
}

pub(super) const fn requested_host_port(entry: &ComposePortEntry) -> Option<u16> {
    match entry.published_host_port {
        ComposePublishedHostPort::Single(port) => Some(port),
        ComposePublishedHostPort::None
        | ComposePublishedHostPort::Range(_)
        | ComposePublishedHostPort::Invalid(_) => None,
    }
}

pub(super) fn host_ip_display_value(host_ip: &ComposePortHostIp) -> String {
    match host_ip {
        ComposePortHostIp::Omitted => String::new(),
        ComposePortHostIp::Explicit(value) | ComposePortHostIp::Invalid(value) => value.clone(),
    }
}

pub(super) fn endpoint_for_entry(
    entry: &ComposePortEntry,
) -> std::result::Result<ComposePublishedPortEndpoint, ComposePublishedPortPlanError> {
    let host_ip = match &entry.host_ip {
        ComposePortHostIp::Omitted => ComposePublishedPortHostIp::Omitted,
        ComposePortHostIp::Explicit(value) => ComposePublishedPortHostIp::Explicit(value.clone()),
        ComposePortHostIp::Invalid(value) => {
            return Err(ComposePublishedPortPlanError::InconsistentEntry {
                service: entry.service.clone(),
                port_entry_index: entry.entry_index,
                detail: format!("eligible fixed TCP entry has invalid host_ip `{value}`"),
            });
        }
    };
    let host_port = requested_host_port(entry).ok_or_else(|| {
        ComposePublishedPortPlanError::InconsistentEntry {
            service: entry.service.clone(),
            port_entry_index: entry.entry_index,
            detail: "eligible fixed TCP entry is missing a single published host port".to_owned(),
        }
    })?;
    Ok(ComposePublishedPortEndpoint { host_ip, host_port })
}

pub(super) fn target_port_for_entry(
    entry: &ComposePortEntry,
) -> std::result::Result<u16, ComposePublishedPortPlanError> {
    entry
        .target_port
        .ok_or_else(|| ComposePublishedPortPlanError::InconsistentEntry {
            service: entry.service.clone(),
            port_entry_index: entry.entry_index,
            detail: "eligible fixed TCP entry is missing a target port".to_owned(),
        })
}

pub(super) fn reservation_host_ip(endpoint: &ComposePublishedPortEndpoint) -> &str {
    match &endpoint.host_ip {
        ComposePublishedPortHostIp::Omitted => OMITTED_HOST_IP_RESERVATION,
        ComposePublishedPortHostIp::Explicit(value) => value,
    }
}
