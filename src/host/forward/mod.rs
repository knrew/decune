mod agent;
mod auto;
mod proc_scan;
mod runtime;
mod session;

use std::path::{Path, PathBuf};

use crate::config::canonical::sha256_hex;

pub(crate) use agent::{
    ForwardAgentStatus, invoked_as_forward_agent, run_forward_agent,
    wait_for_forward_agent_with_status,
};
pub(crate) use auto::AutoForwardConfig;
pub(crate) use runtime::{
    ForwardRuntime, ServiceForwardRuntime, forward_agent_command_at, new_forward_agent_secret,
    new_forward_agent_socket_id, prepare_forward_runtime, prepare_service_forward_runtimes,
};
pub(crate) use session::{ForwardSession, start_forward_session_with_auto};

const FORWARD_AGENT_NAME: &str = "decune-forward-agent";
const FORWARD_AGENT_SOCKET_NAME: &str = "forward-agent.sock";
const FORWARD_AGENT_DIAGNOSTIC_NAME: &str = "forward-agent.err";
const FORWARD_AGENT_STATUS_NAME: &str = "forward-agent.status";
const FORWARD_AGENT_SOCKET_TARGET: &str = "/run/decune/forward-agent.sock";
const FORWARD_AGENT_TARGET: &str = "/run/decune/decune-forward-agent";
const FORWARD_AGENT_USER: &str = "0";
const FORWARD_AGENT_ALLOWED_PORTS_ENV: &str = "DECUNE_FORWARD_AGENT_ALLOWED_PORTS";
const FORWARD_AGENT_SECRET_ENV: &str = "DECUNE_FORWARD_AGENT_SECRET";
const FORWARD_AGENT_SOCKET_ENV: &str = "DECUNE_FORWARD_AGENT_SOCKET";
const FORWARD_AGENT_START_RETRIES: usize = 100;
const FORWARD_AGENT_START_DELAY: std::time::Duration = std::time::Duration::from_millis(50);
const FORWARD_AGENT_DIAGNOSTIC_TAIL_BYTES: usize = 4096;

pub(crate) fn forward_agent_socket_name(service: Option<&str>) -> String {
    match service {
        Some(service) => format!("forward-agent-{}.sock", service_socket_key(service)),
        None => FORWARD_AGENT_SOCKET_NAME.to_owned(),
    }
}

pub(crate) fn forward_agent_session_socket_name(service: Option<&str>, session_id: &str) -> String {
    match service {
        Some(service) => format!(
            "forward-agent-{}-{session_id}.sock",
            service_socket_key(service)
        ),
        None => format!("forward-agent-{session_id}.sock"),
    }
}

pub(crate) fn forward_agent_socket_target_from_name(socket_name: &str) -> String {
    format!(
        "{}/{}",
        FORWARD_AGENT_SOCKET_TARGET
            .rsplit_once('/')
            .map(|(parent, _)| parent)
            .unwrap_or("/run/decune"),
        socket_name
    )
}

pub(crate) fn service_forward_runtime_dir(runtime_dir: &Path, service: &str) -> PathBuf {
    runtime_dir
        .join("forward")
        .join(service_runtime_key(service))
}

fn service_runtime_key(service: &str) -> String {
    let safe = service_socket_key(service);
    let hash = sha256_hex(service.as_bytes());
    format!("{}-{}", &safe[..safe.len().min(48)], &hash[..12])
}

fn service_socket_key(service: &str) -> String {
    let safe = service
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    let safe = safe.trim_matches('_');
    let safe = if safe.is_empty() { "service" } else { safe };
    safe[..safe.len().min(48)].to_owned()
}

#[cfg(test)]
mod tests {
    use crate::{config::types::PortProtocol, docker::ports::ResolvedForwardPort};

    pub(super) fn forward_port(host: u16, container: u16) -> ResolvedForwardPort {
        ResolvedForwardPort {
            service: None,
            container,
            requested_host: host,
            host,
            host_ip: "127.0.0.1".to_owned(),
            protocol: PortProtocol::Tcp,
            require_local: false,
            label: None,
        }
    }

    #[test]
    fn session_socket_names_are_service_aware() {
        assert_eq!(
            super::forward_agent_session_socket_name(None, "abc123"),
            "forward-agent-abc123.sock"
        );
        assert_eq!(
            super::forward_agent_session_socket_name(Some("db"), "abc123"),
            "forward-agent-db-abc123.sock"
        );
        assert_eq!(
            super::forward_agent_session_socket_name(Some("db/primary"), "abc123"),
            "forward-agent-db_primary-abc123.sock"
        );
    }

    #[test]
    fn socket_target_can_be_built_from_session_socket_name() {
        assert_eq!(
            super::forward_agent_socket_target_from_name("forward-agent-abc123.sock"),
            "/run/decune/forward-agent-abc123.sock"
        );
    }
}
