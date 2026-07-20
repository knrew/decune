use std::{
    env,
    io::{Read, Write},
    os::unix::net::UnixStream,
};

use anyhow::{Context, Result, anyhow, bail};
use decune_container_protocol::{
    GitCredentialAction, GitCredentialHostRequest, HOST_DAEMON_PROTOCOL_VERSION, HostDaemonResponse,
};

const HOST_DAEMON_SOCKET_TARGET: &str = "/run/decune/host-daemon.sock";

fn main() {
    if let Err(error) = run() {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let action = git_credential_action_from_args()?;
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .context("Failed to read Git credential helper stdin")?;
    let request = serde_json::to_string(&GitCredentialHostRequest::new(action, input))
        .context("Failed to serialize Git credential helper request")?;

    let socket_path =
        env::var("DECUNE_HOST_DAEMON_SOCKET").unwrap_or_else(|_| HOST_DAEMON_SOCKET_TARGET.into());
    let mut stream = UnixStream::connect(&socket_path).with_context(|| {
        format!("Failed to connect to decune host daemon socket: {socket_path}")
    })?;
    stream
        .write_all(request.as_bytes())
        .context("Failed to write Git credential request to decune host daemon")?;
    stream
        .shutdown(std::net::Shutdown::Write)
        .context("Failed to close Git credential request stream")?;

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .context("Failed to read Git credential response from decune host daemon")?;
    let output = parse_git_credential_helper_response(&response)?;
    std::io::stdout()
        .write_all(output.as_bytes())
        .context("Failed to write Git credential helper stdout")?;

    Ok(())
}

fn parse_git_credential_helper_response(bytes: &[u8]) -> Result<String> {
    let response: HostDaemonResponse =
        serde_json::from_slice(bytes).context("Invalid decune host daemon response JSON")?;

    if response.version != HOST_DAEMON_PROTOCOL_VERSION {
        bail!(
            "Unsupported decune host daemon protocol version: {}",
            response.version
        );
    }

    match response.into_result() {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(error)) => Err(anyhow!(
            "decune host daemon request failed ({}): {}",
            error.code,
            error.message
        )),
        Err(_validation_error) => Err(anyhow!("Invalid decune host daemon response")),
    }
}

fn git_credential_action_from_args() -> Result<GitCredentialAction> {
    match env::args().nth(1).as_deref() {
        Some("fill" | "get") => Ok(GitCredentialAction::Get),
        Some("approve" | "store") => Ok(GitCredentialAction::Store),
        Some("reject" | "erase") => Ok(GitCredentialAction::Erase),
        Some(action) => bail!("Unsupported Git credential helper action: {action}"),
        None => bail!("Git credential helper action is required"),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_git_credential_helper_response;

    #[test]
    fn git_credential_response_accepts_success_without_warnings_field() {
        let output = parse_git_credential_helper_response(
            br#"{"version":1,"ok":true,"output":"username=octo\npassword=SECRET\n"}"#,
        )
        .unwrap();

        assert_eq!(output, "username=octo\npassword=SECRET\n");
    }

    #[test]
    fn git_credential_response_preserves_server_error_code() {
        let error = parse_git_credential_helper_response(
            br#"{"version":1,"ok":false,"error":{"code":"credential_failed","message":"Host git credential fill failed"}}"#,
        )
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            "decune host daemon request failed (credential_failed): Host git credential fill failed"
        );
    }

    #[test]
    fn git_credential_response_rejects_malformed_invariants() {
        for response in [
            br#"{"version":1,"ok":true}"#.as_slice(),
            br#"{"version":1,"ok":true,"output":"value","error":{"code":"future_error","message":"failed"}}"#
                .as_slice(),
            br#"{"version":1,"ok":false,"output":"value","error":{"code":"future_error","message":"failed"}}"#
                .as_slice(),
            br#"{"version":1,"ok":false}"#.as_slice(),
            br#"{"version":1,"ok":false,"error":{"code":"future_error","message":"failed"},"warnings":["warning"]}"#
                .as_slice(),
        ] {
            let error = parse_git_credential_helper_response(response).unwrap_err();

            assert_eq!(error.to_string(), "Invalid decune host daemon response");
        }
    }

    #[test]
    fn git_credential_response_rejects_unsupported_protocol_version() {
        let error = parse_git_credential_helper_response(
            br#"{"version":999,"ok":true,"output":"credential"}"#,
        )
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            "Unsupported decune host daemon protocol version: 999"
        );
    }
}
