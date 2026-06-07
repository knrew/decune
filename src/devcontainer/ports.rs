use std::collections::BTreeMap;

use anyhow::{Result, anyhow};
use serde::Deserialize;

use crate::config::{
    layer::{LayerForwardPort, LayerPort, LayerPortAttributes, LayerPublishPort},
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
    let attribute_keys = attribute_keys_for_port(parsed.container, port);
    let port_attributes = attributes_for_keys(attributes, &attribute_keys);

    Ok(LayerForwardPort {
        port: LayerPort {
            enabled: true,
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

fn attribute_keys_for_port(container_port: u16, original: &DevcontainerPort) -> Vec<String> {
    let container_key = container_port.to_string();

    match original {
        DevcontainerPort::Number(_) => vec![container_key],
        DevcontainerPort::String(value) if value == &container_key => vec![container_key],
        DevcontainerPort::String(value) => vec![container_key, value.clone()],
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
    container: u16,
    host: Option<u16>,
    host_ip: Option<String>,
    protocol: PortProtocol,
}

fn parse_forwarding_port(port: &DevcontainerPort) -> Result<ParsedPort> {
    match port {
        DevcontainerPort::Number(container) => Ok(ParsedPort {
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
    let segments = value.split(':').collect::<Vec<_>>();

    match segments.as_slice() {
        [host, container] if is_local_forwarding_host(host) => Ok(ParsedPort {
            container: parse_u16_port(container, "container port")?,
            host: None,
            host_ip: Some(DEFAULT_PORT_HOST_IP.to_owned()),
            protocol,
        }),
        [host, container] if is_numeric_port_candidate(host) => {
            let _ = parse_u16_port(container, "container port")?;
            Err(anyhow!(
                "Invalid devcontainer forwardPorts entry: {original}. Use a numeric JSON value for a current-container port; host-port mappings are not supported in forwardPorts"
            ))
        }
        [host, container] => {
            let _ = parse_u16_port(container, "container port")?;
            Err(anyhow!(
                "Unsupported devcontainer forwardPorts host: {host}. Docker Compose service forwarding is not supported"
            ))
        }
        [container] => {
            let _ = parse_u16_port(container, "container port")?;
            Err(anyhow!(
                "Invalid devcontainer forwardPorts entry: {original}. Use a numeric JSON value or localhost:<port>"
            ))
        }
        _ => Err(anyhow!(
            "Invalid devcontainer forwardPorts entry: {original}"
        )),
    }
}

fn parse_publish_port_string(value: &str) -> Result<ParsedPort> {
    let (value, protocol) = parse_publish_port_protocol(value)?;
    let segments = value.split(':').collect::<Vec<_>>();

    match segments.as_slice() {
        [container] => Ok(ParsedPort {
            container: parse_u16_port(container, "container port")?,
            host: None,
            host_ip: None,
            protocol,
        }),
        [left, container] if is_numeric_port_candidate(left) => Ok(ParsedPort {
            container: parse_u16_port(container, "container port")?,
            host: Some(parse_u16_port(left, "host port")?),
            host_ip: None,
            protocol,
        }),
        [host_ip, container] => Ok(ParsedPort {
            container: parse_u16_port(container, "container port")?,
            host: None,
            host_ip: Some(normalize_host_ip(host_ip)?),
            protocol,
        }),
        [host_ip, host, container] => Ok(ParsedPort {
            container: parse_u16_port(container, "container port")?,
            host: Some(parse_u16_port(host, "host port")?),
            host_ip: Some(normalize_host_ip(host_ip)?),
            protocol,
        }),
        _ => Err(anyhow!("Invalid devcontainer port specification: {value}")),
    }
}

fn parse_forwarding_port_protocol(value: &str) -> Result<(&str, PortProtocol)> {
    match value.split_once('/') {
        None => Ok((value, PortProtocol::Tcp)),
        Some((port, "tcp")) => Ok((port, PortProtocol::Tcp)),
        Some((_, protocol)) => Err(anyhow!(
            "Unsupported devcontainer port protocol: {protocol}"
        )),
    }
}

fn parse_publish_port_protocol(value: &str) -> Result<(&str, PortProtocol)> {
    match value.split_once('/') {
        None => Ok((value, PortProtocol::Tcp)),
        Some((port, "tcp")) => Ok((port, PortProtocol::Tcp)),
        Some((port, "udp")) => Ok((port, PortProtocol::Udp)),
        Some((_, protocol)) => Err(anyhow!(
            "Unsupported devcontainer port protocol: {protocol}"
        )),
    }
}

fn parse_u16_port(value: &str, label: &str) -> Result<u16> {
    value
        .parse()
        .map_err(|error| anyhow!("Invalid {label} in devcontainer port {value}: {error}"))
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
                .contains("Unsupported devcontainer port protocol")
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
    fn string_numeric_forward_port_is_rejected() {
        let error = forwarding_port_to_layer(
            &DevcontainerPort::String("3000".to_owned()),
            &empty_attributes(),
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Use a numeric JSON value or localhost:<port>")
        );
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
    fn compose_service_forward_port_host_is_rejected() {
        let error = forwarding_port_to_layer(
            &DevcontainerPort::String("db:5432".to_owned()),
            &empty_attributes(),
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Docker Compose service forwarding is not supported")
        );
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
    fn app_port_accepts_udp_protocol_for_docker_publish() {
        let port = publish_port_to_layer(&DevcontainerPort::String(
            "127.0.0.1:5353:53/udp".to_owned(),
        ))
        .unwrap();

        assert_eq!(
            port,
            LayerPublishPort {
                container: 53,
                host: Some(5353),
                host_ip: Some(DEFAULT_PORT_HOST_IP.to_owned()),
                protocol: PortProtocol::Udp,
            }
        );
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
