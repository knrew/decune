use std::collections::BTreeMap;

use anyhow::{Result, anyhow};
use serde::Deserialize;

use crate::config::{
    layer::{LayerForwardPort, LayerPort, LayerPortAttributes, LayerPublishPort},
    ports::{PortSpecSegments, split_port_spec},
    types::{DEFAULT_PORT_HOST_IP, OnAutoForward as ConfigOnAutoForward, PortProtocol},
};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum OnAutoForward {
    Notify,
    Silent,
    Ignore,
    OpenBrowser,
    OpenBrowserOnce,
    OpenPreview,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DevcontainerPortAttributes {
    pub(crate) label: Option<String>,
    pub(crate) on_auto_forward: Option<OnAutoForward>,
    pub(crate) require_local_port: Option<bool>,
    #[serde(rename = "protocol")]
    pub(crate) unsupported_protocol: Option<String>,
    #[serde(rename = "elevateIfNeeded")]
    pub(crate) unsupported_elevate_if_needed: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub(crate) enum DevcontainerPort {
    Number(u16),
    String(String),
}

pub(crate) fn forwarding_port_to_layer(
    port: &DevcontainerPort,
    attributes: &BTreeMap<String, DevcontainerPortAttributes>,
) -> Result<LayerForwardPort> {
    let parsed = parse_forwarding_port(port)?;
    let attribute_keys = attribute_keys_for_port(parsed.service.as_deref(), parsed.container, port);
    let port_attributes = attributes_for_keys(attributes, &attribute_keys);

    Ok(LayerForwardPort {
        port: LayerPort {
            enabled: true,
            service: parsed.service,
            container: parsed.container,
            host: parsed.host,
            host_ip: parsed
                .host_ip
                .unwrap_or_else(|| DEFAULT_PORT_HOST_IP.to_owned()),
            protocol: parsed.protocol,
            require_local: port_attributes
                .and_then(|attributes| attributes.require_local_port)
                .unwrap_or(false),
            label: port_attributes.and_then(|attributes| attributes.label.clone()),
        },
        attribute_keys,
    })
}

pub(crate) fn publish_port_to_layer(port: &DevcontainerPort) -> Result<LayerPublishPort> {
    let parsed = parse_publish_port(port)?;

    Ok(LayerPublishPort {
        container: parsed.container,
        host: parsed.host,
        host_ip: parsed.host_ip,
        protocol: parsed.protocol,
    })
}

pub(crate) fn port_attributes_to_layer(
    attributes: &DevcontainerPortAttributes,
) -> LayerPortAttributes {
    LayerPortAttributes {
        label: attributes.label.clone(),
        on_auto_forward: attributes
            .on_auto_forward
            .as_ref()
            .map(on_auto_forward_to_config),
        require_local_port: attributes.require_local_port,
        unsupported_protocol: attributes.unsupported_protocol.clone(),
        unsupported_elevate_if_needed: attributes.unsupported_elevate_if_needed,
    }
}

fn attribute_keys_for_port(
    service: Option<&str>,
    container_port: u16,
    original: &DevcontainerPort,
) -> Vec<String> {
    let container_key = container_port.to_string();
    let service_key = service.map(|service| format!("{service}:{container_key}"));

    match original {
        DevcontainerPort::Number(_) => vec![container_key],
        DevcontainerPort::String(value) if value == &container_key => vec![container_key],
        DevcontainerPort::String(value) => {
            let mut keys = Vec::new();
            if let Some(service_key) = service_key {
                keys.push(service_key);
            }
            keys.push(container_key);
            if !keys.iter().any(|key| key == value) {
                keys.push(value.clone());
            }
            keys
        }
    }
}

fn attributes_for_keys<'a>(
    attributes: &'a BTreeMap<String, DevcontainerPortAttributes>,
    keys: &[String],
) -> Option<&'a DevcontainerPortAttributes> {
    keys.iter().find_map(|key| attributes.get(key))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedPort {
    service: Option<String>,
    container: u16,
    host: Option<u16>,
    host_ip: Option<String>,
    protocol: PortProtocol,
}

fn parse_forwarding_port(port: &DevcontainerPort) -> Result<ParsedPort> {
    match port {
        DevcontainerPort::Number(container) => Ok(ParsedPort {
            service: None,
            container: *container,
            host: None,
            host_ip: Some(DEFAULT_PORT_HOST_IP.to_owned()),
            protocol: PortProtocol::Tcp,
        }),
        DevcontainerPort::String(value) => parse_forwarding_port_string(value),
    }
}

fn parse_publish_port(port: &DevcontainerPort) -> Result<ParsedPort> {
    match port {
        DevcontainerPort::Number(container) => Ok(ParsedPort {
            service: None,
            container: *container,
            host: Some(*container),
            host_ip: None,
            protocol: PortProtocol::Tcp,
        }),
        DevcontainerPort::String(value) => parse_publish_port_string(value),
    }
}

fn parse_forwarding_port_string(original: &str) -> Result<ParsedPort> {
    let (value, protocol) = parse_forwarding_port_protocol(original)?;

    match split_port_spec(value)
        .map_err(|error| anyhow!("Invalid devcontainer forwardPorts entry: {original}. {error}"))?
    {
        PortSpecSegments::Two {
            left: host_ip,
            container,
            bracketed_host_ip: true,
        } => Ok(ParsedPort {
            service: None,
            container: parse_u16_port(container, "container port")?,
            host: None,
            host_ip: Some(normalize_host_ip(host_ip)?),
            protocol,
        }),
        PortSpecSegments::Two {
            left: host,
            container,
            bracketed_host_ip: false,
        } if is_local_forwarding_host(host) => Ok(ParsedPort {
            service: None,
            container: parse_u16_port(container, "container port")?,
            host: None,
            host_ip: Some(DEFAULT_PORT_HOST_IP.to_owned()),
            protocol,
        }),
        PortSpecSegments::Two {
            left: host,
            container,
            bracketed_host_ip: false,
        } if is_numeric_port_candidate(host) => {
            let _ = parse_u16_port(container, "container port")?;
            Err(anyhow!(
                "Invalid devcontainer forwardPorts entry: {original}. Use a numeric JSON value for a current-container port; host-port mappings are not supported in forwardPorts"
            ))
        }
        PortSpecSegments::Two {
            left: service,
            container,
            bracketed_host_ip: false,
        } => Ok(ParsedPort {
            service: Some(parse_compose_service_name(service, original)?),
            container: parse_u16_port(container, "container port")?,
            host: None,
            host_ip: Some(DEFAULT_PORT_HOST_IP.to_owned()),
            protocol,
        }),
        PortSpecSegments::One { container } => Ok(ParsedPort {
            service: None,
            container: parse_u16_port(container, "container port")?,
            host: None,
            host_ip: Some(DEFAULT_PORT_HOST_IP.to_owned()),
            protocol,
        }),
        PortSpecSegments::Three { .. } => Err(anyhow!(
            "Invalid devcontainer forwardPorts entry: {original}. host-port mappings are not supported in forwardPorts"
        )),
    }
}

fn parse_publish_port_string(value: &str) -> Result<ParsedPort> {
    let (value, protocol) = parse_publish_port_protocol(value)?;

    match split_port_spec(value)
        .map_err(|error| anyhow!("Invalid devcontainer port specification: {value}. {error}"))?
    {
        PortSpecSegments::One { container } => Ok(ParsedPort {
            service: None,
            container: parse_u16_port(container, "container port")?,
            host: None,
            host_ip: None,
            protocol,
        }),
        PortSpecSegments::Two {
            left,
            container,
            bracketed_host_ip: false,
        } if is_numeric_port_candidate(left) => Ok(ParsedPort {
            service: None,
            container: parse_u16_port(container, "container port")?,
            host: Some(parse_u16_port(left, "host port")?),
            host_ip: None,
            protocol,
        }),
        PortSpecSegments::Two {
            left: host_ip,
            container,
            ..
        } => Ok(ParsedPort {
            service: None,
            container: parse_u16_port(container, "container port")?,
            host: None,
            host_ip: Some(normalize_host_ip(host_ip)?),
            protocol,
        }),
        PortSpecSegments::Three {
            host_ip,
            host,
            container,
            ..
        } => Ok(ParsedPort {
            service: None,
            container: parse_u16_port(container, "container port")?,
            host: Some(parse_u16_port(host, "host port")?),
            host_ip: Some(normalize_host_ip(host_ip)?),
            protocol,
        }),
    }
}

fn parse_forwarding_port_protocol(value: &str) -> Result<(&str, PortProtocol)> {
    match value.split_once('/') {
        None => Ok((value, PortProtocol::Tcp)),
        Some((port, "tcp")) => Ok((port, PortProtocol::Tcp)),
        Some((_, protocol)) => Err(anyhow!(
            "Unsupported devcontainer port protocol: {protocol}. decune supports tcp only"
        )),
    }
}

fn parse_publish_port_protocol(value: &str) -> Result<(&str, PortProtocol)> {
    match value.split_once('/') {
        None => Ok((value, PortProtocol::Tcp)),
        Some((port, "tcp")) => Ok((port, PortProtocol::Tcp)),
        Some((_, protocol)) => Err(anyhow!(
            "Unsupported devcontainer port protocol: {protocol}. decune supports tcp only"
        )),
    }
}

fn parse_u16_port(value: &str, label: &str) -> Result<u16> {
    value
        .parse()
        .map_err(|error| anyhow!("Invalid {label} in devcontainer port {value}: {error}"))
}

fn parse_compose_service_name(value: &str, original: &str) -> Result<String> {
    if value.is_empty() {
        return Err(anyhow!(
            "Invalid devcontainer forwardPorts entry: {original}. Compose service name must not be empty"
        ));
    }

    Ok(value.to_owned())
}

fn is_numeric_port_candidate(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|character| character.is_ascii_digit())
}

fn is_local_forwarding_host(value: &str) -> bool {
    matches!(value, "localhost")
}

fn normalize_host_ip(value: &str) -> Result<String> {
    match value {
        "" => Err(anyhow!("Devcontainer port host IP must not be empty")),
        "localhost" => Ok(DEFAULT_PORT_HOST_IP.to_owned()),
        value => Ok(value.to_owned()),
    }
}

fn on_auto_forward_to_config(value: &OnAutoForward) -> ConfigOnAutoForward {
    match value {
        OnAutoForward::Notify => ConfigOnAutoForward::Notify,
        OnAutoForward::Silent => ConfigOnAutoForward::Silent,
        OnAutoForward::Ignore => ConfigOnAutoForward::Ignore,
        OnAutoForward::OpenBrowser
        | OnAutoForward::OpenBrowserOnce
        | OnAutoForward::OpenPreview => ConfigOnAutoForward::Notify,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::config::types::{DEFAULT_PORT_HOST_IP, OnAutoForward as ConfigOnAutoForward};

    use super::*;

    #[test]
    fn standard_on_auto_forward_values_are_accepted() {
        for (input, expected) in [
            ("notify", ConfigOnAutoForward::Notify),
            ("silent", ConfigOnAutoForward::Silent),
            ("ignore", ConfigOnAutoForward::Ignore),
            ("openBrowser", ConfigOnAutoForward::Notify),
            ("openBrowserOnce", ConfigOnAutoForward::Notify),
            ("openPreview", ConfigOnAutoForward::Notify),
        ] {
            let attributes: DevcontainerPortAttributes =
                serde_json::from_value(json!({"onAutoForward": input})).unwrap();
            let layer = port_attributes_to_layer(&attributes);

            assert_eq!(layer.on_auto_forward, Some(expected));
        }
    }

    #[test]
    fn unsupported_port_attribute_fields_are_parsed_for_warnings() {
        let attributes: DevcontainerPortAttributes = serde_json::from_value(json!({
            "label": "web",
            "protocol": "https",
            "elevateIfNeeded": true
        }))
        .unwrap();

        assert_eq!(attributes.label.as_deref(), Some("web"));
        assert_eq!(attributes.unsupported_protocol.as_deref(), Some("https"));
        assert_eq!(attributes.unsupported_elevate_if_needed, Some(true));
    }

    #[test]
    fn unsupported_port_protocol_is_rejected() {
        let error = forwarding_port_to_layer(
            &DevcontainerPort::String("3000/udp".to_owned()),
            &empty_attributes(),
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Unsupported devcontainer port protocol: udp. decune supports tcp only")
        );
    }

    #[test]
    fn string_forward_port_requires_localhost_host() {
        let port = forwarding_port_to_layer(
            &DevcontainerPort::String("localhost:5432".to_owned()),
            &empty_attributes(),
        )
        .unwrap();

        assert_eq!(port.port.host_ip, DEFAULT_PORT_HOST_IP);
        assert_eq!(port.port.container, 5432);
        assert_eq!(port.port.host, None);
    }

    #[test]
    fn string_numeric_forward_port_targets_primary_service() {
        let port = forwarding_port_to_layer(
            &DevcontainerPort::String("3000".to_owned()),
            &empty_attributes(),
        )
        .unwrap();

        assert_eq!(port.port.service, None);
        assert_eq!(port.port.container, 3000);
        assert_eq!(port.port.host, None);
        assert_eq!(port.port.host_ip, DEFAULT_PORT_HOST_IP);
    }

    #[test]
    fn publish_style_forward_port_mapping_is_rejected() {
        let error = forwarding_port_to_layer(
            &DevcontainerPort::String("3000:3000".to_owned()),
            &empty_attributes(),
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("host-port mappings are not supported")
        );
    }

    #[test]
    fn compose_service_forward_port_is_preserved() {
        let port = forwarding_port_to_layer(
            &DevcontainerPort::String("db:5432".to_owned()),
            &empty_attributes(),
        )
        .unwrap();

        assert_eq!(port.port.service.as_deref(), Some("db"));
        assert_eq!(port.port.container, 5432);
        assert_eq!(port.port.host, None);
        assert_eq!(port.port.host_ip, DEFAULT_PORT_HOST_IP);
    }

    #[test]
    fn bracketed_ipv6_forward_port_sets_host_ip() {
        let port = forwarding_port_to_layer(
            &DevcontainerPort::String("[::1]:5432".to_owned()),
            &empty_attributes(),
        )
        .unwrap();

        assert_eq!(port.port.service, None);
        assert_eq!(port.port.container, 5432);
        assert_eq!(port.port.host, None);
        assert_eq!(port.port.host_ip, "::1");
    }

    #[test]
    fn three_segment_forward_port_mapping_is_rejected() {
        let error = forwarding_port_to_layer(
            &DevcontainerPort::String("127.0.0.1:5433:5432".to_owned()),
            &empty_attributes(),
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Invalid devcontainer forwardPorts entry")
        );
    }

    #[test]
    fn bracketed_ipv6_forward_port_mapping_is_rejected() {
        let error = forwarding_port_to_layer(
            &DevcontainerPort::String("[::1]:5433:5432".to_owned()),
            &empty_attributes(),
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("host-port mappings are not supported")
        );
    }

    #[test]
    fn invalid_port_number_is_rejected() {
        let error =
            publish_port_to_layer(&DevcontainerPort::String("99999".to_owned())).unwrap_err();

        assert!(error.to_string().contains("Invalid container port"));
    }

    #[test]
    fn numeric_app_port_publishes_the_same_host_port() {
        let port = publish_port_to_layer(&DevcontainerPort::Number(8080)).unwrap();

        assert_eq!(
            port,
            LayerPublishPort {
                container: 8080,
                host: Some(8080),
                host_ip: None,
                protocol: PortProtocol::Tcp,
            }
        );
    }

    #[test]
    fn string_app_port_without_host_uses_ephemeral_host_port() {
        let port = publish_port_to_layer(&DevcontainerPort::String("8080".to_owned())).unwrap();

        assert_eq!(
            port,
            LayerPublishPort {
                container: 8080,
                host: None,
                host_ip: None,
                protocol: PortProtocol::Tcp,
            }
        );
    }

    #[test]
    fn app_port_rejects_udp_protocol_for_docker_publish() {
        let error = publish_port_to_layer(&DevcontainerPort::String(
            "127.0.0.1:5353:53/udp".to_owned(),
        ))
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Unsupported devcontainer port protocol: udp. decune supports tcp only")
        );
    }

    #[test]
    fn app_port_accepts_bracketed_ipv6_publish_forms() {
        let host_and_container =
            publish_port_to_layer(&DevcontainerPort::String("[::1]:8080:3000".to_owned())).unwrap();
        let container_only =
            publish_port_to_layer(&DevcontainerPort::String("[::1]:3000".to_owned())).unwrap();
        let with_protocol = publish_port_to_layer(&DevcontainerPort::String(
            "[2001:db8::1]:8080:3000/tcp".to_owned(),
        ))
        .unwrap();

        assert_eq!(
            host_and_container,
            LayerPublishPort {
                container: 3000,
                host: Some(8080),
                host_ip: Some("::1".to_owned()),
                protocol: PortProtocol::Tcp,
            }
        );
        assert_eq!(
            container_only,
            LayerPublishPort {
                container: 3000,
                host: None,
                host_ip: Some("::1".to_owned()),
                protocol: PortProtocol::Tcp,
            }
        );
        assert_eq!(
            with_protocol,
            LayerPublishPort {
                container: 3000,
                host: Some(8080),
                host_ip: Some("2001:db8::1".to_owned()),
                protocol: PortProtocol::Tcp,
            }
        );
    }

    #[test]
    fn app_port_preserves_ipv4_localhost_and_numeric_host_forms() {
        let ipv4 =
            publish_port_to_layer(&DevcontainerPort::String("127.0.0.1:8080:3000".to_owned()))
                .unwrap();
        let localhost =
            publish_port_to_layer(&DevcontainerPort::String("localhost:3000".to_owned())).unwrap();
        let numeric_host =
            publish_port_to_layer(&DevcontainerPort::String("8080:3000".to_owned())).unwrap();

        assert_eq!(ipv4.host_ip.as_deref(), Some(DEFAULT_PORT_HOST_IP));
        assert_eq!(ipv4.host, Some(8080));
        assert_eq!(ipv4.container, 3000);
        assert_eq!(localhost.host_ip.as_deref(), Some(DEFAULT_PORT_HOST_IP));
        assert_eq!(localhost.host, None);
        assert_eq!(localhost.container, 3000);
        assert_eq!(numeric_host.host_ip, None);
        assert_eq!(numeric_host.host, Some(8080));
        assert_eq!(numeric_host.container, 3000);
    }

    #[test]
    fn malformed_ipv6_app_ports_are_rejected() {
        for value in [
            "::1:8080:3000",
            "[::1:8080:3000",
            "[]:3000",
            "[::1]",
            "[::1]:8080:3000:extra",
            "[::1]:abc:3000",
        ] {
            assert!(
                publish_port_to_layer(&DevcontainerPort::String(value.to_owned())).is_err(),
                "{value}"
            );
        }
    }

    #[test]
    fn numeric_host_port_outside_u16_is_rejected() {
        let error =
            publish_port_to_layer(&DevcontainerPort::String("99999:3000".to_owned())).unwrap_err();

        assert!(error.to_string().contains("Invalid host port"));
    }

    fn empty_attributes() -> BTreeMap<String, DevcontainerPortAttributes> {
        BTreeMap::new()
    }
}
