mod agent;
mod auto;
mod proc_scan;
mod runtime;
mod session;

pub(crate) use agent::{
    ForwardAgentStatus, invoked_as_forward_agent, run_forward_agent,
    wait_for_forward_agent_with_status,
};
pub(crate) use auto::AutoForwardConfig;
pub(crate) use runtime::{
    ForwardRuntime, forward_agent_command, new_forward_agent_secret, prepare_forward_runtime,
};
pub(crate) use session::{ForwardSession, start_forward_session_with_auto};

const FORWARD_AGENT_NAME: &str = "decune-forward-agent";
const FORWARD_AGENT_SOCKET_NAME: &str = "forward-agent.sock";
const FORWARD_AGENT_DIAGNOSTIC_NAME: &str = "forward-agent.err";
const FORWARD_AGENT_SOCKET_TARGET: &str = "/run/decune/forward-agent.sock";
const FORWARD_AGENT_TARGET: &str = "/run/decune/decune-forward-agent";
const FORWARD_AGENT_USER: &str = "0";
const FORWARD_AGENT_ALLOWED_PORTS_ENV: &str = "DECUNE_FORWARD_AGENT_ALLOWED_PORTS";
const FORWARD_AGENT_SECRET_ENV: &str = "DECUNE_FORWARD_AGENT_SECRET";
const FORWARD_AGENT_START_RETRIES: usize = 100;
const FORWARD_AGENT_START_DELAY: std::time::Duration = std::time::Duration::from_millis(50);
const FORWARD_AGENT_DIAGNOSTIC_TAIL_BYTES: usize = 4096;

#[cfg(test)]
mod tests {
    use crate::{config::types::PortProtocol, docker::ports::ResolvedForwardPort};

    pub(super) fn forward_port(host: u16, container: u16) -> ResolvedForwardPort {
        ResolvedForwardPort {
            container,
            host,
            host_ip: "127.0.0.1".to_owned(),
            protocol: PortProtocol::Tcp,
            require_local: false,
            label: None,
        }
    }
}
