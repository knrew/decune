use std::{error::Error, fmt};

use serde::{Deserialize, Serialize};

pub const HOST_DAEMON_PROTOCOL_VERSION: u16 = 1;
pub const REQUEST_TYPE_CREDENTIAL: &str = "credential";
pub const REQUEST_TYPE_CLI_QUERY: &str = "cliQuery";

pub const ERROR_CODE_INVALID_REQUEST: &str = "invalid_request";
pub const ERROR_CODE_UNSUPPORTED_PROTOCOL_VERSION: &str = "unsupported_protocol_version";
pub const ERROR_CODE_REQUEST_TOO_LARGE: &str = "request_too_large";
pub const ERROR_CODE_UNKNOWN_REQUEST_TYPE: &str = "unknown_request_type";
pub const ERROR_CODE_NOT_IMPLEMENTED: &str = "not_implemented";
pub const ERROR_CODE_CREDENTIAL_FAILED: &str = "credential_failed";
pub const ERROR_CODE_UNSUPPORTED_COMMAND: &str = "unsupported_command";
pub const ERROR_CODE_UNSUPPORTED_FORMAT: &str = "unsupported_format";
pub const ERROR_CODE_CONTAINER_CLI_DISABLED: &str = "container_cli_disabled";
pub const ERROR_CODE_CLI_QUERY_FAILED: &str = "cli_query_failed";
pub const ERROR_CODE_CLI_QUERY_BUSY: &str = "cli_query_busy";
pub const ERROR_CODE_CLI_QUERY_TIMEOUT: &str = "cli_query_timeout";

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

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CliQueryRequest {
    pub version: u16,
    #[serde(rename = "type")]
    pub request_type: String,
    pub command: String,
    pub format: String,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostDaemonResponse {
    pub version: u16,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<HostDaemonError>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl HostDaemonResponse {
    pub fn success(output: impl Into<String>) -> Self {
        Self::success_with_warnings(output, Vec::new())
    }

    pub fn success_with_warnings(output: impl Into<String>, warnings: Vec<String>) -> Self {
        Self {
            version: HOST_DAEMON_PROTOCOL_VERSION,
            ok: true,
            output: Some(output.into()),
            error: None,
            warnings,
        }
    }

    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            version: HOST_DAEMON_PROTOCOL_VERSION,
            ok: false,
            output: None,
            error: Some(HostDaemonError {
                code: code.into(),
                message: message.into(),
            }),
            warnings: Vec::new(),
        }
    }

    /// Validates the success/error field invariant for this response.
    ///
    /// # Errors
    ///
    /// Returns an error when required fields are missing or mutually exclusive fields are present.
    pub const fn validate(&self) -> Result<(), HostDaemonResponseValidationError> {
        let valid = if self.ok {
            self.output.is_some() && self.error.is_none()
        } else {
            self.output.is_none() && self.error.is_some() && self.warnings.is_empty()
        };

        if valid {
            Ok(())
        } else {
            Err(HostDaemonResponseValidationError)
        }
    }
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostDaemonError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostDaemonResponseValidationError;

impl fmt::Display for HostDaemonResponseValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("host daemon response violates the protocol invariant")
    }
}

impl Error for HostDaemonResponseValidationError {}

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

#[cfg(test)]
mod tests {
    use super::{CliQueryRequest, HostDaemonError, HostDaemonResponse, REQUEST_TYPE_CLI_QUERY};

    #[test]
    fn success_response_round_trips_with_warnings() {
        let response = HostDaemonResponse::success_with_warnings(
            "workspace status",
            vec!["status may be stale".to_owned()],
        );

        let json = serde_json::to_string(&response).unwrap();
        let decoded: HostDaemonResponse = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded, response);
        assert!(decoded.validate().is_ok());
    }

    #[test]
    fn error_response_round_trips() {
        let response = HostDaemonResponse::error("cli_query_failed", "query failed");

        let json = serde_json::to_string(&response).unwrap();
        let decoded: HostDaemonResponse = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded, response);
        assert!(decoded.validate().is_ok());
    }

    #[test]
    fn response_without_warnings_deserializes_with_empty_warnings() {
        let response: HostDaemonResponse =
            serde_json::from_str(r#"{"version":1,"ok":true,"output":"credential"}"#).unwrap();

        assert!(response.warnings.is_empty());
        assert!(response.validate().is_ok());
    }

    #[test]
    fn response_validation_covers_every_field_combination() {
        for ok in [false, true] {
            for has_output in [false, true] {
                for has_error in [false, true] {
                    for has_warnings in [false, true] {
                        let response = HostDaemonResponse {
                            version: 1,
                            ok,
                            output: has_output.then(|| "output".to_owned()),
                            error: has_error.then(|| HostDaemonError {
                                code: "error_code".to_owned(),
                                message: "error message".to_owned(),
                            }),
                            warnings: if has_warnings {
                                vec!["warning".to_owned()]
                            } else {
                                Vec::new()
                            },
                        };
                        let expected_valid = if ok {
                            has_output && !has_error
                        } else {
                            !has_output && has_error && !has_warnings
                        };

                        assert_eq!(
                            response.validate().is_ok(),
                            expected_valid,
                            "unexpected validation result for {response:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn unknown_future_error_code_deserializes() {
        let response: HostDaemonResponse = serde_json::from_str(
            r#"{"version":1,"ok":false,"error":{"code":"future_error","message":"failed"}}"#,
        )
        .unwrap();

        assert_eq!(response.error.unwrap().code, "future_error");
    }

    #[test]
    fn error_code_is_required() {
        let error = serde_json::from_str::<HostDaemonResponse>(
            r#"{"version":1,"ok":false,"error":{"message":"failed"}}"#,
        )
        .unwrap_err();

        assert!(error.to_string().contains("missing field `code`"));
    }

    #[test]
    fn cli_query_request_preserves_raw_command_and_format() {
        let request: CliQueryRequest = serde_json::from_str(
            r#"{"version":1,"type":"cliQuery","command":"futureCommand","format":"futureFormat"}"#,
        )
        .unwrap();

        assert_eq!(request.request_type, REQUEST_TYPE_CLI_QUERY);
        assert_eq!(request.command, "futureCommand");
        assert_eq!(request.format, "futureFormat");
    }

    #[test]
    fn cli_query_request_rejects_unknown_fields() {
        let error = serde_json::from_str::<CliQueryRequest>(
            r#"{"version":1,"type":"cliQuery","command":"status","format":"text","workspace":"demo"}"#,
        )
        .unwrap_err();

        assert!(error.to_string().contains("unknown field `workspace`"));
    }
}
