#![allow(
    clippy::all,
    clippy::cargo,
    clippy::nursery,
    clippy::pedantic,
    clippy::restriction,
    reason = "Temporary allow while strict clippy policy is introduced; code fixes will follow separately."
)]

use serde::{Deserialize, Serialize};

pub const HOST_DAEMON_PROTOCOL_VERSION: u16 = 1;
pub const REQUEST_TYPE_CREDENTIAL: &str = "credential";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GitCredentialAction {
    Get,
    Store,
    Erase,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GitCredentialHostRequest {
    pub version: u16,
    #[serde(rename = "type")]
    pub request_type: String,
    pub action: GitCredentialAction,
    pub input: String,
}

impl GitCredentialHostRequest {
    pub fn new(action: GitCredentialAction, input: impl Into<String>) -> Self {
        Self {
            version: HOST_DAEMON_PROTOCOL_VERSION,
            request_type: REQUEST_TYPE_CREDENTIAL.to_owned(),
            action,
            input: input.into(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct HostDaemonResponse {
    pub version: u16,
    pub ok: bool,
    pub output: Option<String>,
    pub error: Option<HostDaemonError>,
}

#[derive(Debug, Deserialize)]
pub struct HostDaemonError {
    pub message: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ForwardAgentRequest {
    pub port: Option<u16>,
    pub shutdown: Option<bool>,
    pub secret: Option<String>,
    pub scan: Option<ForwardAgentScanRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForwardAgentScanRequest {
    pub min: u16,
    pub max: u16,
    pub ignore: Vec<u16>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ForwardAgentScanResponse {
    pub ports: Vec<u16>,
}
