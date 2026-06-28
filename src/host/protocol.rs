use serde::{Deserialize, Serialize};

use decune_container_protocol::GitCredentialHostRequest;

use crate::{
    config::types::GitHttpsMode,
    host::credentials::{GitCredentialExecutor, handle_git_credential_request},
};

pub(crate) const HOST_DAEMON_PROTOCOL_VERSION: u16 =
    decune_container_protocol::HOST_DAEMON_PROTOCOL_VERSION;

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
    output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<HostDaemonError>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct HostDaemonError {
    code: String,
    message: String,
}

impl HostDaemonResponse {
    pub(crate) fn request_too_large(max_bytes: usize) -> Self {
        Self::error(
            "request_too_large",
            format!("Host daemon request exceeds {max_bytes} bytes"),
        )
    }

    fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            version: HOST_DAEMON_PROTOCOL_VERSION,
            ok: false,
            output: None,
            error: Some(HostDaemonError {
                code: code.into(),
                message: message.into(),
            }),
        }
    }

    fn ok(output: impl Into<String>) -> Self {
        Self {
            version: HOST_DAEMON_PROTOCOL_VERSION,
            ok: true,
            output: Some(output.into()),
            error: None,
        }
    }
}

pub(crate) fn handle_host_daemon_request(
    bytes: &[u8],
    git_credentials: &dyn GitCredentialExecutor,
    git_https_mode: GitHttpsMode,
) -> HostDaemonResponse {
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
        REQUEST_TYPE_CREDENTIAL => {
            handle_credential_request(bytes, git_credentials, git_https_mode)
        }
        REQUEST_TYPE_PORT_FORWARD => HostDaemonResponse::error(
            "not_implemented",
            "Host daemon request is not implemented yet: portForward",
        ),
        _ => HostDaemonResponse::error(
            "unknown_request_type",
            format!("Unknown host daemon request type: {}", request.request_type),
        ),
    }
}

fn handle_credential_request(
    bytes: &[u8],
    git_credentials: &dyn GitCredentialExecutor,
    git_https_mode: GitHttpsMode,
) -> HostDaemonResponse {
    let request = match serde_json::from_slice::<GitCredentialHostRequest>(bytes) {
        Ok(request) => request,
        Err(error) => {
            return HostDaemonResponse::error(
                "invalid_request",
                format!("Invalid Git credential request JSON: {error}"),
            );
        }
    };

    match handle_git_credential_request(&request, git_credentials, git_https_mode) {
        Ok(output) => HostDaemonResponse::ok(output),
        Err(error) => HostDaemonResponse::error("credential_failed", error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use anyhow::{Result, anyhow};
    use serde_json::json;

    use super::handle_host_daemon_request;
    use crate::config::types::GitHttpsMode;
    use crate::host::credentials::{GitCredentialCommand, GitCredentialExecutor};

    #[derive(Debug)]
    struct RecordingGitCredentialExecutor {
        calls: Mutex<Vec<(GitCredentialCommand, String)>>,
        result: Mutex<Result<String, String>>,
    }

    impl RecordingGitCredentialExecutor {
        fn with_output(output: &str) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                result: Mutex::new(Ok(output.to_owned())),
            }
        }

        fn with_error(message: &str) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                result: Mutex::new(Err(message.to_owned())),
            }
        }
    }

    impl GitCredentialExecutor for RecordingGitCredentialExecutor {
        fn run(&self, command: GitCredentialCommand, input: &str) -> Result<String> {
            self.calls
                .lock()
                .map_err(|error| {
                    anyhow!("Git credential call recorder mutex was poisoned: {error}")
                })?
                .push((command, input.to_owned()));
            self.result
                .lock()
                .map_err(|error| anyhow!("Git credential result mutex was poisoned: {error}"))?
                .clone()
                .map_err(anyhow::Error::msg)
        }
    }

    #[test]
    fn credential_get_invokes_fill_and_returns_output() {
        let executor =
            RecordingGitCredentialExecutor::with_output("username=octo\npassword=SECRET\n");

        let response = handle_host_daemon_request(
            br#"{"version":1,"type":"credential","action":"get","input":"protocol=https\nhost=github.com\n\n"}"#,
            &executor,
            GitHttpsMode::HostHelper,
        );

        assert_eq!(
            serde_json::to_value(response).unwrap(),
            json!({
                "version": 1,
                "ok": true,
                "output": "username=octo\npassword=SECRET\n"
            })
        );
        assert_eq!(
            executor.calls.lock().unwrap().as_slice(),
            [(
                GitCredentialCommand::Fill,
                "protocol=https\nhost=github.com\n\n".to_owned()
            )]
        );
    }

    #[test]
    fn credential_store_invokes_approve() {
        let executor = RecordingGitCredentialExecutor::with_output("");

        let response = handle_host_daemon_request(
            br#"{"version":1,"type":"credential","action":"store","input":"protocol=https\nhost=github.com\nusername=octo\npassword=SECRET\n\n"}"#,
            &executor,
            GitHttpsMode::HostHelper,
        );

        assert_eq!(
            serde_json::to_value(response).unwrap(),
            json!({
                "version": 1,
                "ok": true,
                "output": ""
            })
        );
        assert_eq!(
            executor.calls.lock().unwrap()[0].0,
            GitCredentialCommand::Approve
        );
    }

    #[test]
    fn credential_error_does_not_echo_request_secret() {
        let executor =
            RecordingGitCredentialExecutor::with_error("Host git credential fill failed");

        let response = handle_host_daemon_request(
            br#"{"version":1,"type":"credential","action":"get","input":"password=SECRET\n\n"}"#,
            &executor,
            GitHttpsMode::HostHelper,
        );
        let response = serde_json::to_value(response).unwrap();

        assert_eq!(response["ok"], false);
        assert_eq!(response["error"]["code"], "credential_failed");
        assert_eq!(
            response["error"]["message"],
            "Host git credential fill failed"
        );
        assert!(!response.to_string().contains("SECRET"));
    }

    #[test]
    fn credential_off_mode_rejects_request_without_invoking_executor() {
        let executor =
            RecordingGitCredentialExecutor::with_output("username=octo\npassword=SECRET\n");

        let response = handle_host_daemon_request(
            br#"{"version":1,"type":"credential","action":"get","input":"password=SECRET\n\n"}"#,
            &executor,
            GitHttpsMode::Off,
        );
        let response = serde_json::to_value(response).unwrap();

        assert_eq!(response["ok"], false);
        assert_eq!(response["error"]["code"], "credential_failed");
        assert_eq!(
            response["error"]["message"],
            "Git HTTPS credential forwarding is disabled"
        );
        assert!(!response.to_string().contains("SECRET"));
        assert!(executor.calls.lock().unwrap().is_empty());
    }
}
