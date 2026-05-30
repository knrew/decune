use std::net::TcpListener;

use anyhow::{Context, Result, bail};

use crate::config::{resolved::ResolvedPort, types::PortProtocol};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DockerPublishPort {
    pub(crate) container: u16,
    pub(crate) host: Option<u16>,
    pub(crate) host_ip: Option<String>,
    pub(crate) protocol: PortProtocol,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedForwardPort {
    pub(crate) container: u16,
    pub(crate) host: u16,
    pub(crate) host_ip: String,
    pub(crate) protocol: PortProtocol,
    pub(crate) require_local: bool,
    pub(crate) label: Option<String>,
}

pub(crate) fn resolve_forward_ports(ports: &[ResolvedPort]) -> Result<Vec<ResolvedForwardPort>> {
    resolve_forward_ports_with(ports, host_port_available)
}

fn resolve_forward_ports_with<F>(
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
    left == right || left == "0.0.0.0" || right == "0.0.0.0"
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

impl DockerPublishPort {
    pub(crate) fn key(&self) -> String {
        format!("{}/{}", self.container, docker_protocol(self.protocol))
    }
}

fn docker_protocol(protocol: PortProtocol) -> &'static str {
    match protocol {
        PortProtocol::Tcp => "tcp",
    }
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

    fn manual_port(
        container: u16,
        host: Option<u16>,
        host_ip: &str,
        require_local: bool,
    ) -> LayerPort {
        LayerPort {
            enabled: true,
            container,
            host,
            host_ip: host_ip.to_owned(),
            protocol: PortProtocol::Tcp,
            require_local,
            label: None,
        }
    }
}
