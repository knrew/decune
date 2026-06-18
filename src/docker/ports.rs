use std::{
    collections::{BTreeMap, BTreeSet},
    net::{IpAddr, TcpListener},
};

use anyhow::{Context, Result, bail};

use crate::config::{
    resolved::{ResolvedAutoPorts, ResolvedPort, ResolvedPortAttributes, ResolvedPublishPort},
    types::{DEFAULT_PORT_HOST_IP, OnAutoForward, PortProtocol},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DockerPublishPort {
    pub(crate) container: u16,
    pub(crate) host: Option<u16>,
    pub(crate) host_ip: Option<String>,
    pub(crate) protocol: PortProtocol,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedForwardPort {
    pub(crate) service: Option<String>,
    pub(crate) container: u16,
    pub(crate) host: u16,
    pub(crate) host_ip: String,
    pub(crate) protocol: PortProtocol,
    pub(crate) require_local: bool,
    pub(crate) label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedAutoForwardPort {
    pub(crate) port: ResolvedForwardPort,
    pub(crate) on_auto_forward: OnAutoForward,
}

pub(crate) fn resolve_forward_ports(ports: &[ResolvedPort]) -> Result<Vec<ResolvedForwardPort>> {
    resolve_forward_ports_with(ports, host_port_available)
}

pub(crate) fn resolve_forward_ports_with<F>(
    ports: &[ResolvedPort],
    mut host_port_available: F,
) -> Result<Vec<ResolvedForwardPort>>
where
    F: FnMut(&str, u16) -> Result<bool>,
{
    let mut resolved = Vec::new();

    for port in ports {
        let start_host = port.host.unwrap_or(port.container);
        let host = resolve_host_port(port, start_host, &resolved, &mut host_port_available)?;

        resolved.push(ResolvedForwardPort {
            service: port.service.clone(),
            container: port.container,
            host,
            host_ip: port.host_ip.clone(),
            protocol: port.protocol,
            require_local: port.require_local,
            label: port.label.clone(),
        });
    }

    Ok(resolved)
}

pub(crate) fn resolve_auto_forward_ports(
    detected_ports: impl IntoIterator<Item = u16>,
    existing_forward_ports: &[ResolvedForwardPort],
    publish_ports: &[ResolvedPublishPort],
    auto_ports: &ResolvedAutoPorts,
    port_attributes: &BTreeMap<String, ResolvedPortAttributes>,
    other_ports_attributes: Option<&ResolvedPortAttributes>,
) -> Result<Vec<ResolvedAutoForwardPort>> {
    resolve_auto_forward_ports_with(
        detected_ports,
        existing_forward_ports,
        publish_ports,
        auto_ports,
        port_attributes,
        other_ports_attributes,
        host_port_available,
    )
}

pub(crate) fn resolve_auto_forward_ports_with<F>(
    detected_ports: impl IntoIterator<Item = u16>,
    existing_forward_ports: &[ResolvedForwardPort],
    publish_ports: &[ResolvedPublishPort],
    auto_ports: &ResolvedAutoPorts,
    port_attributes: &BTreeMap<String, ResolvedPortAttributes>,
    other_ports_attributes: Option<&ResolvedPortAttributes>,
    mut host_port_available: F,
) -> Result<Vec<ResolvedAutoForwardPort>>
where
    F: FnMut(&str, u16) -> Result<bool>,
{
    if !auto_ports.enabled {
        return Ok(Vec::new());
    }

    let manual_containers = existing_forward_ports
        .iter()
        .map(|port| port.container)
        .collect::<BTreeSet<_>>();
    let published_tcp_containers = publish_ports
        .iter()
        .filter(|port| port.protocol == PortProtocol::Tcp)
        .map(|port| port.container)
        .collect::<BTreeSet<_>>();
    let ignored = auto_ports.ignore.iter().copied().collect::<BTreeSet<_>>();
    let mut resolved = existing_forward_ports.to_vec();
    let mut additions = Vec::new();

    for container in detected_ports.into_iter().collect::<BTreeSet<_>>() {
        if container < auto_ports.min
            || container >= auto_ports.max
            || ignored.contains(&container)
            || manual_containers.contains(&container)
            || published_tcp_containers.contains(&container)
        {
            continue;
        }

        let attributes = port_attributes
            .get(&container.to_string())
            .or(other_ports_attributes);
        let on_auto_forward = attributes
            .and_then(|attributes| attributes.on_auto_forward)
            .unwrap_or(auto_ports.on_auto_forward);
        if on_auto_forward == OnAutoForward::Ignore {
            continue;
        }

        let port = ResolvedPort {
            enabled: true,
            service: None,
            container,
            host: Some(container),
            host_ip: DEFAULT_PORT_HOST_IP.to_owned(),
            protocol: PortProtocol::Tcp,
            require_local: attributes
                .and_then(|attributes| attributes.require_local_port)
                .unwrap_or(false),
            label: attributes.and_then(|attributes| attributes.label.clone()),
        };
        let host = resolve_host_port(&port, container, &resolved, &mut host_port_available)?;
        let port = ResolvedForwardPort {
            service: None,
            container,
            host,
            host_ip: DEFAULT_PORT_HOST_IP.to_owned(),
            protocol: PortProtocol::Tcp,
            require_local: port.require_local,
            label: port.label,
        };
        resolved.push(port.clone());
        additions.push(ResolvedAutoForwardPort {
            port,
            on_auto_forward,
        });
    }

    Ok(additions)
}

fn resolve_host_port<F>(
    port: &ResolvedPort,
    start_host: u16,
    resolved: &[ResolvedForwardPort],
    host_port_available: &mut F,
) -> Result<u16>
where
    F: FnMut(&str, u16) -> Result<bool>,
{
    let mut candidate = start_host;

    loop {
        let available = !reserved_host_port_conflicts(resolved, &port.host_ip, candidate)
            && host_port_available(&port.host_ip, candidate)?;

        if available {
            return Ok(candidate);
        }

        if port.require_local {
            bail!(
                "Manual port {}:{} is already in use; choose another host port or disable require_local",
                port.host_ip,
                start_host
            );
        }

        candidate = candidate.checked_add(1).with_context(|| {
            format!(
                "No available host port found for manual port {}:{}",
                port.host_ip, start_host
            )
        })?;
    }
}

fn reserved_host_port_conflicts(
    resolved: &[ResolvedForwardPort],
    candidate_host_ip: &str,
    candidate_host: u16,
) -> bool {
    resolved.iter().any(|existing| {
        existing.host == candidate_host
            && host_ip_bindings_conflict(&existing.host_ip, candidate_host_ip)
    })
}

fn host_ip_bindings_conflict(left: &str, right: &str) -> bool {
    left == right || host_ip_is_wildcard(left) || host_ip_is_wildcard(right)
}

fn host_ip_is_wildcard(value: &str) -> bool {
    value
        .parse::<IpAddr>()
        .is_ok_and(|address| address.is_unspecified())
}

fn host_port_available(host_ip: &str, host_port: u16) -> Result<bool> {
    match TcpListener::bind((host_ip, host_port)) {
        Ok(listener) => {
            drop(listener);
            Ok(true)
        }
        Err(error) if is_addr_in_use(&error) => Ok(false),
        Err(error) => Err(error)
            .with_context(|| format!("check host port availability for {host_ip}:{host_port}")),
    }
}

fn is_addr_in_use(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::AddrInUse
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{layer::LayerPort, types::DEFAULT_PORT_HOST_IP};

    #[test]
    fn resolves_unspecified_host_port_from_container_port() {
        let ports = vec![manual_port(3000, None, DEFAULT_PORT_HOST_IP, false)];

        let resolved = resolve_forward_ports_with(&ports, |_, _| Ok(true)).unwrap();

        assert_eq!(
            resolved,
            vec![ResolvedForwardPort {
                service: None,
                container: 3000,
                host: 3000,
                host_ip: DEFAULT_PORT_HOST_IP.to_owned(),
                protocol: PortProtocol::Tcp,
                require_local: false,
                label: None,
            }]
        );
    }

    #[test]
    fn falls_back_to_next_available_host_port_when_not_required() {
        let ports = vec![manual_port(3000, Some(3000), DEFAULT_PORT_HOST_IP, false)];

        let resolved =
            resolve_forward_ports_with(&ports, |_, port| Ok(!matches!(port, 3000 | 3001))).unwrap();

        assert_eq!(resolved[0].host, 3002);
    }

    #[test]
    fn require_local_rejects_occupied_host_port() {
        let ports = vec![manual_port(3000, Some(3000), DEFAULT_PORT_HOST_IP, true)];

        let error = resolve_forward_ports_with(&ports, |_, _| Ok(false)).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Manual port 127.0.0.1:3000 is already in use")
        );
    }

    #[test]
    fn resolver_keeps_host_ip_reservations_separate_when_bindable_together() {
        let ports = vec![
            manual_port(3000, Some(3000), DEFAULT_PORT_HOST_IP, false),
            manual_port(3001, Some(3000), "127.0.0.2", false),
        ];

        let resolved = resolve_forward_ports_with(&ports, |_, _| Ok(true)).unwrap();

        assert_eq!(resolved[0].host, 3000);
        assert_eq!(resolved[1].host, 3000);
    }

    #[test]
    fn resolver_treats_wildcard_and_loopback_same_port_as_conflicting() {
        let ports = vec![
            manual_port(3000, Some(3000), "0.0.0.0", false),
            manual_port(3001, Some(3000), DEFAULT_PORT_HOST_IP, false),
        ];

        let resolved = resolve_forward_ports_with(&ports, |_, _| Ok(true)).unwrap();

        assert_eq!(resolved[0].host, 3000);
        assert_eq!(resolved[1].host, 3001);
    }

    #[test]
    fn resolver_treats_ipv6_wildcard_and_loopback_same_port_as_conflicting() {
        let ports = vec![
            manual_port(3000, Some(3000), "::", false),
            manual_port(3001, Some(3000), "::1", false),
        ];

        let resolved = resolve_forward_ports_with(&ports, |_, _| Ok(true)).unwrap();

        assert_eq!(resolved[0].host, 3000);
        assert_eq!(resolved[1].host, 3001);
    }

    #[test]
    fn resolver_treats_ipv6_loopback_and_wildcard_same_port_as_conflicting() {
        let ports = vec![
            manual_port(3000, Some(3000), "::1", false),
            manual_port(3001, Some(3000), "::", false),
        ];

        let resolved = resolve_forward_ports_with(&ports, |_, _| Ok(true)).unwrap();

        assert_eq!(resolved[0].host, 3000);
        assert_eq!(resolved[1].host, 3001);
    }

    #[test]
    fn resolver_rejects_required_ipv6_wildcard_conflict() {
        let ports = vec![
            manual_port(3000, Some(3000), "::", false),
            manual_port(3001, Some(3000), "::1", true),
        ];

        let error = resolve_forward_ports_with(&ports, |_, _| Ok(true)).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Manual port ::1:3000 is already in use")
        );
    }

    #[test]
    fn auto_forward_excludes_ignored_manual_published_and_attribute_ignored_ports() {
        let auto = ResolvedAutoPorts {
            enabled: true,
            min: 2000,
            max: 7000,
            ignore: vec![3001],
            on_auto_forward: OnAutoForward::Notify,
        };
        let manual = vec![forward_port(3002, 3002)];
        let publish = vec![ResolvedPublishPort {
            container: 3003,
            host: Some(3003),
            host_ip: None,
            protocol: PortProtocol::Tcp,
        }];
        let attributes = BTreeMap::from([(
            "3004".to_owned(),
            ResolvedPortAttributes {
                label: None,
                on_auto_forward: Some(OnAutoForward::Ignore),
                require_local_port: None,
                ..ResolvedPortAttributes::default()
            },
        )]);

        let resolved = resolve_auto_forward_ports_with(
            [1023, 2000, 3001, 3002, 3003, 3004, 6999, 7000],
            &manual,
            &publish,
            &auto,
            &attributes,
            None,
            |_, _| Ok(true),
        )
        .unwrap();

        assert_eq!(
            resolved
                .into_iter()
                .map(|port| port.port.container)
                .collect::<Vec<_>>(),
            vec![2000, 6999]
        );
    }

    #[test]
    fn auto_forward_excludes_only_tcp_published_ports() {
        let auto = ResolvedAutoPorts {
            enabled: true,
            min: 1024,
            max: 32768,
            ignore: Vec::new(),
            on_auto_forward: OnAutoForward::Notify,
        };
        let publish = vec![
            ResolvedPublishPort {
                container: 3000,
                host: Some(3000),
                host_ip: None,
                protocol: PortProtocol::Tcp,
            },
            ResolvedPublishPort {
                container: 3001,
                host: Some(3001),
                host_ip: None,
                protocol: PortProtocol::Udp,
            },
        ];

        let resolved = resolve_auto_forward_ports_with(
            [3000, 3001],
            &[],
            &publish,
            &auto,
            &BTreeMap::new(),
            None,
            |_, _| Ok(true),
        )
        .unwrap();

        assert_eq!(
            resolved
                .into_iter()
                .map(|port| port.port.container)
                .collect::<Vec<_>>(),
            vec![3001]
        );
    }

    #[test]
    fn auto_forward_uses_attributes_and_falls_back_host_ports() {
        let auto = ResolvedAutoPorts {
            enabled: true,
            min: 1024,
            max: 32768,
            ignore: Vec::new(),
            on_auto_forward: OnAutoForward::Notify,
        };
        let attributes = BTreeMap::from([(
            "4321".to_owned(),
            ResolvedPortAttributes {
                label: Some("web".to_owned()),
                on_auto_forward: Some(OnAutoForward::Silent),
                require_local_port: Some(false),
                ..ResolvedPortAttributes::default()
            },
        )]);

        let resolved = resolve_auto_forward_ports_with(
            [4321],
            &[forward_port(4321, 1234)],
            &[],
            &auto,
            &attributes,
            None,
            |_, port| Ok(port != 4321),
        )
        .unwrap();

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].port.container, 4321);
        assert_eq!(resolved[0].port.host, 4322);
        assert_eq!(resolved[0].port.label.as_deref(), Some("web"));
        assert_eq!(resolved[0].on_auto_forward, OnAutoForward::Silent);
    }

    #[test]
    fn auto_forward_respects_other_ports_ignore_default() {
        let auto = ResolvedAutoPorts {
            enabled: true,
            min: 1024,
            max: 32768,
            ignore: Vec::new(),
            on_auto_forward: OnAutoForward::Notify,
        };
        let other = ResolvedPortAttributes {
            label: None,
            on_auto_forward: Some(OnAutoForward::Ignore),
            require_local_port: None,
            ..ResolvedPortAttributes::default()
        };

        let resolved = resolve_auto_forward_ports_with(
            [4321],
            &[],
            &[],
            &auto,
            &BTreeMap::new(),
            Some(&other),
            |_, _| Ok(true),
        )
        .unwrap();

        assert!(resolved.is_empty());
    }

    fn manual_port(
        container: u16,
        host: Option<u16>,
        host_ip: &str,
        require_local: bool,
    ) -> LayerPort {
        LayerPort {
            enabled: true,
            service: None,
            container,
            host,
            host_ip: host_ip.to_owned(),
            protocol: PortProtocol::Tcp,
            require_local,
            label: None,
        }
    }

    fn forward_port(host: u16, container: u16) -> ResolvedForwardPort {
        ResolvedForwardPort {
            service: None,
            container,
            host,
            host_ip: DEFAULT_PORT_HOST_IP.to_owned(),
            protocol: PortProtocol::Tcp,
            require_local: false,
            label: None,
        }
    }
}
