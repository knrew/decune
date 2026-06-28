use crate::runtime::compose_ports::{
    ComposePortEntry, ComposePortHostIp, ComposePortProtocol, ComposePublishedHostPort,
    ComposePublishedPortEndpoint, ComposePublishedPortHostIpKind,
};

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
    match endpoint.host_ip_kind {
        ComposePublishedPortHostIpKind::Omitted => {
            format!("<host_ip omitted>:{}", endpoint.host_port)
        }
        ComposePublishedPortHostIpKind::Explicit => format!(
            "{}:{}",
            endpoint.host_ip_value.as_deref().unwrap_or(""),
            endpoint.host_port
        ),
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

pub(super) fn endpoint_for_entry(entry: &ComposePortEntry) -> ComposePublishedPortEndpoint {
    let (host_ip_kind, host_ip_value) = match &entry.host_ip {
        ComposePortHostIp::Omitted => (ComposePublishedPortHostIpKind::Omitted, None),
        ComposePortHostIp::Explicit(value) => (
            ComposePublishedPortHostIpKind::Explicit,
            Some(value.clone()),
        ),
        ComposePortHostIp::Invalid(_) => {
            unreachable!("eligible Compose published port entry has valid host_ip")
        }
    };
    ComposePublishedPortEndpoint {
        host_ip_kind,
        host_ip_value,
        host_port: requested_host_port(entry)
            .expect("eligible Compose published port entry has published host port"),
    }
}

pub(super) fn reservation_host_ip(endpoint: &ComposePublishedPortEndpoint) -> &str {
    match endpoint.host_ip_kind {
        ComposePublishedPortHostIpKind::Omitted => OMITTED_HOST_IP_RESERVATION,
        ComposePublishedPortHostIpKind::Explicit => endpoint
            .host_ip_value
            .as_deref()
            .expect("explicit Compose published port endpoint has host_ip value"),
    }
}
