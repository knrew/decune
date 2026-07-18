use serde::Deserialize;

pub(crate) use decune_container_protocol::HostDaemonResponse;
use decune_container_protocol::{
    CliQueryRequest, ERROR_CODE_CREDENTIAL_FAILED, ERROR_CODE_INVALID_REQUEST,
    ERROR_CODE_NOT_IMPLEMENTED, ERROR_CODE_UNKNOWN_REQUEST_TYPE, ERROR_CODE_UNSUPPORTED_COMMAND,
    ERROR_CODE_UNSUPPORTED_FORMAT, ERROR_CODE_UNSUPPORTED_PROTOCOL_VERSION,
    GitCredentialHostRequest, REQUEST_TYPE_CLI_QUERY, REQUEST_TYPE_CREDENTIAL,
};

use crate::{
    config::types::GitHttpsMode,
    host::credentials::{GitCredentialExecutor, handle_git_credential_request},
};

pub(crate) const HOST_DAEMON_PROTOCOL_VERSION: u16 =
    decune_container_protocol::HOST_DAEMON_PROTOCOL_VERSION;

const REQUEST_TYPE_PORT_FORWARD: &str = "portForward";

#[derive(Debug, Deserialize)]
struct HostDaemonRequest {
    version: u16,
    #[serde(rename = "type")]
    request_type: String,
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
                ERROR_CODE_INVALID_REQUEST,
                format!("Invalid host daemon request JSON: {error}"),
            );
        }
    };

    if request.version != HOST_DAEMON_PROTOCOL_VERSION {
        return HostDaemonResponse::error(
            ERROR_CODE_UNSUPPORTED_PROTOCOL_VERSION,
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
        REQUEST_TYPE_CLI_QUERY => handle_cli_query_request(bytes),
        REQUEST_TYPE_PORT_FORWARD => HostDaemonResponse::error(
            ERROR_CODE_NOT_IMPLEMENTED,
            "Host daemon request is not implemented yet: portForward",
        ),
        _ => HostDaemonResponse::error(
            ERROR_CODE_UNKNOWN_REQUEST_TYPE,
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
                ERROR_CODE_INVALID_REQUEST,
                format!("Invalid Git credential request JSON: {error}"),
            );
        }
    };

    match handle_git_credential_request(&request, git_credentials, git_https_mode) {
        Ok(output) => HostDaemonResponse::success(output),
        Err(error) => HostDaemonResponse::error(ERROR_CODE_CREDENTIAL_FAILED, error.to_string()),
    }
}

fn handle_cli_query_request(bytes: &[u8]) -> HostDaemonResponse {
    let request = match serde_json::from_slice::<CliQueryRequest>(bytes) {
        Ok(request) => request,
        Err(error) => {
            return HostDaemonResponse::error(
                ERROR_CODE_INVALID_REQUEST,
                format!("Invalid container CLI query request JSON: {error}"),
            );
        }
    };

    match request.command.as_str() {
        "status" => {
            if request.format != "text" {
                return HostDaemonResponse::error(
                    ERROR_CODE_UNSUPPORTED_FORMAT,
                    format!(
                        "Unsupported container CLI query format for status: {}",
                        request.format
                    ),
                );
            }
        }
        "ports" => {
            if !matches!(request.format.as_str(), "text" | "json") {
                return HostDaemonResponse::error(
                    ERROR_CODE_UNSUPPORTED_FORMAT,
                    format!(
                        "Unsupported container CLI query format for ports: {}",
                        request.format
                    ),
                );
            }
        }
        _ => {
            return HostDaemonResponse::error(
                ERROR_CODE_UNSUPPORTED_COMMAND,
                format!(
                    "Unsupported container CLI query command: {}",
                    request.command
                ),
            );
        }
    }

    HostDaemonResponse::error(
        ERROR_CODE_NOT_IMPLEMENTED,
        "Host daemon request is not implemented yet: cliQuery",
    )
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

    #[test]
    fn cli_query_valid_command_and_format_reach_execution_seam() {
        let executor = RecordingGitCredentialExecutor::with_output("unused");

        for (command, format) in [("status", "text"), ("ports", "text"), ("ports", "json")] {
            let request = serde_json::to_vec(&json!({
                "version": 1,
                "type": "cliQuery",
                "command": command,
                "format": format,
            }))
            .unwrap();

            let response =
                handle_host_daemon_request(&request, &executor, GitHttpsMode::HostHelper);
            let response = serde_json::to_value(response).unwrap();

            assert_eq!(response["error"]["code"], "not_implemented");
        }

        assert!(executor.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn cli_query_rejects_unknown_command() {
        let executor = RecordingGitCredentialExecutor::with_output("unused");

        let response = handle_host_daemon_request(
            br#"{"version":1,"type":"cliQuery","command":"inspect","format":"text"}"#,
            &executor,
            GitHttpsMode::HostHelper,
        );
        let response = serde_json::to_value(response).unwrap();

        assert_eq!(response["error"]["code"], "unsupported_command");
        assert!(executor.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn cli_query_rejects_unsupported_command_format_matrix() {
        let executor = RecordingGitCredentialExecutor::with_output("unused");

        for (command, format) in [("status", "json"), ("status", "yaml"), ("ports", "yaml")] {
            let request = serde_json::to_vec(&json!({
                "version": 1,
                "type": "cliQuery",
                "command": command,
                "format": format,
            }))
            .unwrap();

            let response =
                handle_host_daemon_request(&request, &executor, GitHttpsMode::HostHelper);
            let response = serde_json::to_value(response).unwrap();

            assert_eq!(response["error"]["code"], "unsupported_format");
        }

        assert!(executor.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn cli_query_rejects_unknown_fields() {
        let executor = RecordingGitCredentialExecutor::with_output("unused");

        for field in ["workspace", "workspace_id", "all"] {
            let mut request = json!({
                "version": 1,
                "type": "cliQuery",
                "command": "status",
                "format": "text",
            });
            request
                .as_object_mut()
                .unwrap()
                .insert(field.to_owned(), json!("unexpected"));
            let request = serde_json::to_vec(&request).unwrap();

            let response =
                handle_host_daemon_request(&request, &executor, GitHttpsMode::HostHelper);
            let response = serde_json::to_value(response).unwrap();

            assert_eq!(response["error"]["code"], "invalid_request");
        }

        assert!(executor.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn cli_query_rejects_missing_fields() {
        let executor = RecordingGitCredentialExecutor::with_output("unused");

        let response = handle_host_daemon_request(
            br#"{"version":1,"type":"cliQuery","command":"status"}"#,
            &executor,
            GitHttpsMode::HostHelper,
        );
        let response = serde_json::to_value(response).unwrap();

        assert_eq!(response["error"]["code"], "invalid_request");
        assert!(executor.calls.lock().unwrap().is_empty());
    }
}
