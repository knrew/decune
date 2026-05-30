use serde::{Deserialize, Serialize};

pub(crate) const HOST_DAEMON_PROTOCOL_VERSION: u16 = 1;

const REQUEST_TYPE_CREDENTIAL: &str = "credential";
const REQUEST_TYPE_PORT_FORWARD: &str = "portForward";

#[derive(Debug, Deserialize)]
struct HostDaemonRequest {
    version: u16,
    #[serde(rename = "type")]
    request_type: String,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct HostDaemonResponse {
    version: u16,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<HostDaemonError>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct HostDaemonError {
    code: String,
    message: String,
}

impl HostDaemonResponse {
    fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            version: HOST_DAEMON_PROTOCOL_VERSION,
            ok: false,
            error: Some(HostDaemonError {
                code: code.into(),
                message: message.into(),
            }),
        }
    }
}

pub(crate) fn handle_host_daemon_request(bytes: &[u8]) -> HostDaemonResponse {
    let request = match serde_json::from_slice::<HostDaemonRequest>(bytes) {
        Ok(request) => request,
        Err(error) => {
            return HostDaemonResponse::error(
                "invalid_request",
                format!("Invalid host daemon request JSON: {error}"),
            );
        }
    };

    if request.version != HOST_DAEMON_PROTOCOL_VERSION {
        return HostDaemonResponse::error(
            "unsupported_protocol_version",
            format!(
                "Unsupported host daemon protocol version: {}",
                request.version
            ),
        );
    }

    match request.request_type.as_str() {
        REQUEST_TYPE_CREDENTIAL | REQUEST_TYPE_PORT_FORWARD => HostDaemonResponse::error(
            "not_implemented",
            format!(
                "Host daemon request is not implemented yet: {}",
                request.request_type
            ),
        ),
        _ => HostDaemonResponse::error(
            "unknown_request_type",
            format!("Unknown host daemon request type: {}", request.request_type),
        ),
    }
}
