use serde::Deserialize;

pub(crate) use decune_container_protocol::HostDaemonResponse;
use decune_container_protocol::{
    CliQueryRequest, ERROR_CODE_CONTAINER_CLI_DISABLED, ERROR_CODE_CREDENTIAL_FAILED,
    ERROR_CODE_INVALID_REQUEST, ERROR_CODE_NOT_IMPLEMENTED, ERROR_CODE_UNKNOWN_REQUEST_TYPE,
    ERROR_CODE_UNSUPPORTED_COMMAND, ERROR_CODE_UNSUPPORTED_FORMAT,
    ERROR_CODE_UNSUPPORTED_PROTOCOL_VERSION, GitCredentialHostRequest, REQUEST_TYPE_CLI_QUERY,
    REQUEST_TYPE_CREDENTIAL,
};

use std::sync::Arc;

use crate::{
    config::types::GitHttpsMode,
    host::{
        credentials::{GitCredentialExecutor, handle_git_credential_request},
        query::{ContainerCliQuery, ContainerCliQueryRuntime, ContainerCliQueryService},
    },
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

pub(super) enum HostDaemonRequestDispatch {
    Respond(HostDaemonResponse),
    CliQuery {
        query: ContainerCliQuery,
        service: Arc<ContainerCliQueryService>,
    },
}

pub(super) fn handle_host_daemon_request(
    bytes: &[u8],
    git_credentials: &dyn GitCredentialExecutor,
    git_https_mode: GitHttpsMode,
    cli_query_runtime: &ContainerCliQueryRuntime,
) -> HostDaemonRequestDispatch {
    let request = match serde_json::from_slice::<HostDaemonRequest>(bytes) {
        Ok(request) => request,
        Err(error) => {
            return HostDaemonRequestDispatch::Respond(HostDaemonResponse::error(
                ERROR_CODE_INVALID_REQUEST,
                format!("Invalid host daemon request JSON: {error}"),
            ));
        }
    };

    if request.version != HOST_DAEMON_PROTOCOL_VERSION {
        return HostDaemonRequestDispatch::Respond(HostDaemonResponse::error(
            ERROR_CODE_UNSUPPORTED_PROTOCOL_VERSION,
            format!(
                "Unsupported host daemon protocol version: {}",
                request.version
            ),
        ));
    }

    match request.request_type.as_str() {
        REQUEST_TYPE_CREDENTIAL => HostDaemonRequestDispatch::Respond(handle_credential_request(
            bytes,
            git_credentials,
            git_https_mode,
        )),
        REQUEST_TYPE_CLI_QUERY => handle_cli_query_request(bytes, cli_query_runtime),
        REQUEST_TYPE_PORT_FORWARD => HostDaemonRequestDispatch::Respond(HostDaemonResponse::error(
            ERROR_CODE_NOT_IMPLEMENTED,
            "Host daemon request is not implemented yet: portForward",
        )),
        _ => HostDaemonRequestDispatch::Respond(HostDaemonResponse::error(
            ERROR_CODE_UNKNOWN_REQUEST_TYPE,
            format!("Unknown host daemon request type: {}", request.request_type),
        )),
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

fn handle_cli_query_request(
    bytes: &[u8],
    cli_query_runtime: &ContainerCliQueryRuntime,
) -> HostDaemonRequestDispatch {
    let request = match serde_json::from_slice::<CliQueryRequest>(bytes) {
        Ok(request) => request,
        Err(error) => {
            return HostDaemonRequestDispatch::Respond(HostDaemonResponse::error(
                ERROR_CODE_INVALID_REQUEST,
                format!("Invalid container CLI query request JSON: {error}"),
            ));
        }
    };

    let query = match request.command.as_str() {
        "status" if request.format == "text" => ContainerCliQuery::StatusText,
        "status" => {
            return HostDaemonRequestDispatch::Respond(HostDaemonResponse::error(
                ERROR_CODE_UNSUPPORTED_FORMAT,
                format!(
                    "Unsupported container CLI query format for status: {}",
                    request.format
                ),
            ));
        }
        "ports" => match request.format.as_str() {
            "text" => ContainerCliQuery::PortsText,
            "json" => ContainerCliQuery::PortsJson,
            _ => {
                return HostDaemonRequestDispatch::Respond(HostDaemonResponse::error(
                    ERROR_CODE_UNSUPPORTED_FORMAT,
                    format!(
                        "Unsupported container CLI query format for ports: {}",
                        request.format
                    ),
                ));
            }
        },
        _ => {
            return HostDaemonRequestDispatch::Respond(HostDaemonResponse::error(
                ERROR_CODE_UNSUPPORTED_COMMAND,
                format!(
                    "Unsupported container CLI query command: {}",
                    request.command
                ),
            ));
        }
    };

    match cli_query_runtime {
        ContainerCliQueryRuntime::Disabled => {
            HostDaemonRequestDispatch::Respond(HostDaemonResponse::error(
                ERROR_CODE_CONTAINER_CLI_DISABLED,
                "Container CLI queries are disabled",
            ))
        }
        ContainerCliQueryRuntime::Enabled(service) => HostDaemonRequestDispatch::CliQuery {
            query,
            service: Arc::clone(service),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Mutex};

    use anyhow::{Result, anyhow};
    use serde_json::json;

    use super::{HostDaemonRequestDispatch, handle_host_daemon_request};
    use crate::config::types::GitHttpsMode;
    use crate::host::{
        credentials::{GitCredentialCommand, GitCredentialExecutor},
        query::{ContainerCliQuery, ContainerCliQueryRuntime},
        query_context::HostDaemonCliQueryPolicy,
    };

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
            &ContainerCliQueryRuntime::Disabled,
        );

        assert_eq!(
            serde_json::to_value(response_from_dispatch(response)).unwrap(),
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
            &ContainerCliQueryRuntime::Disabled,
        );

        assert_eq!(
            serde_json::to_value(response_from_dispatch(response)).unwrap(),
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
            &ContainerCliQueryRuntime::Disabled,
        );
        let response = serde_json::to_value(response_from_dispatch(response)).unwrap();

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
            &ContainerCliQueryRuntime::Disabled,
        );
        let response = serde_json::to_value(response_from_dispatch(response)).unwrap();

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
        let runtime = enabled_runtime();

        for (command, format, expected) in [
            ("status", "text", ContainerCliQuery::StatusText),
            ("ports", "text", ContainerCliQuery::PortsText),
            ("ports", "json", ContainerCliQuery::PortsJson),
        ] {
            let request = serde_json::to_vec(&json!({
                "version": 1,
                "type": "cliQuery",
                "command": command,
                "format": format,
            }))
            .unwrap();

            let dispatch =
                handle_host_daemon_request(&request, &executor, GitHttpsMode::HostHelper, &runtime);

            match dispatch {
                HostDaemonRequestDispatch::CliQuery { query, .. } => assert_eq!(query, expected),
                HostDaemonRequestDispatch::Respond(response) => panic!(
                    "expected query dispatch, got immediate response: {:?}",
                    serde_json::to_value(response)
                ),
            }
        }

        assert!(executor.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn cli_query_disabled_policy_blocks_supported_command_and_format_matrix() {
        let executor = RecordingGitCredentialExecutor::with_output("unused");

        for (command, format) in [("status", "text"), ("ports", "text"), ("ports", "json")] {
            let request = serde_json::to_vec(&json!({
                "version": 1,
                "type": "cliQuery",
                "command": command,
                "format": format,
            }))
            .unwrap();

            let response = handle_host_daemon_request(
                &request,
                &executor,
                GitHttpsMode::HostHelper,
                &ContainerCliQueryRuntime::Disabled,
            );
            let response = serde_json::to_value(response_from_dispatch(response)).unwrap();

            assert_eq!(response["error"]["code"], "container_cli_disabled");
            assert_eq!(
                response["error"]["message"],
                "Container CLI queries are disabled"
            );
        }

        assert!(executor.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn cli_query_rejects_unknown_command() {
        let executor = RecordingGitCredentialExecutor::with_output("unused");
        let runtime = enabled_runtime();

        let response = handle_host_daemon_request(
            br#"{"version":1,"type":"cliQuery","command":"inspect","format":"text"}"#,
            &executor,
            GitHttpsMode::HostHelper,
            &runtime,
        );
        let response = serde_json::to_value(response_from_dispatch(response)).unwrap();

        assert_eq!(response["error"]["code"], "unsupported_command");
        assert!(executor.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn cli_query_rejects_unsupported_command_format_matrix() {
        let executor = RecordingGitCredentialExecutor::with_output("unused");
        let runtime = enabled_runtime();

        for (command, format) in [("status", "json"), ("status", "yaml"), ("ports", "yaml")] {
            let request = serde_json::to_vec(&json!({
                "version": 1,
                "type": "cliQuery",
                "command": command,
                "format": format,
            }))
            .unwrap();

            let response =
                handle_host_daemon_request(&request, &executor, GitHttpsMode::HostHelper, &runtime);
            let response = serde_json::to_value(response_from_dispatch(response)).unwrap();

            assert_eq!(response["error"]["code"], "unsupported_format");
        }

        assert!(executor.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn cli_query_rejects_unknown_fields() {
        let executor = RecordingGitCredentialExecutor::with_output("unused");
        let runtime = enabled_runtime();

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
                handle_host_daemon_request(&request, &executor, GitHttpsMode::HostHelper, &runtime);
            let response = serde_json::to_value(response_from_dispatch(response)).unwrap();

            assert_eq!(response["error"]["code"], "invalid_request");
        }

        assert!(executor.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn cli_query_rejects_missing_fields() {
        let executor = RecordingGitCredentialExecutor::with_output("unused");
        let runtime = enabled_runtime();

        let response = handle_host_daemon_request(
            br#"{"version":1,"type":"cliQuery","command":"status"}"#,
            &executor,
            GitHttpsMode::HostHelper,
            &runtime,
        );
        let response = serde_json::to_value(response_from_dispatch(response)).unwrap();

        assert_eq!(response["error"]["code"], "invalid_request");
        assert!(executor.calls.lock().unwrap().is_empty());
    }

    fn enabled_runtime() -> ContainerCliQueryRuntime {
        ContainerCliQueryRuntime::from_policy(&HostDaemonCliQueryPolicy::enabled_for_test(
            "012345abcdef",
            PathBuf::from("/state/workspace"),
            PathBuf::from("/run/decune/workspace"),
        ))
    }

    fn response_from_dispatch(
        dispatch: HostDaemonRequestDispatch,
    ) -> decune_container_protocol::HostDaemonResponse {
        match dispatch {
            HostDaemonRequestDispatch::Respond(response) => response,
            HostDaemonRequestDispatch::CliQuery { query, .. } => {
                panic!("expected immediate response, got query dispatch: {query:?}")
            }
        }
    }
}
