#![allow(dead_code)]

use crate::config::types::PortProtocol;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DockerPublishPort {
    pub(crate) container: u16,
    pub(crate) host: Option<u16>,
    pub(crate) host_ip: Option<String>,
    pub(crate) protocol: PortProtocol,
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
