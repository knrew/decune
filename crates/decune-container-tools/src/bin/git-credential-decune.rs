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
        .context("Failed to write Git credential request to host daemon")?;
    stream
        .shutdown(std::net::Shutdown::Write)
        .context("Failed to close Git credential request stream")?;

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .context("Failed to read Git credential response from host daemon")?;
    let output = parse_git_credential_helper_response(&response)?;
    std::io::stdout()
        .write_all(output.as_bytes())
        .context("Failed to write Git credential helper stdout")?;

    Ok(())
}

fn parse_git_credential_helper_response(bytes: &[u8]) -> Result<String> {
    let response: HostDaemonResponse =
        serde_json::from_slice(bytes).context("Invalid host daemon response JSON")?;

    if response.version != HOST_DAEMON_PROTOCOL_VERSION {
        bail!(
            "Unsupported host daemon protocol version: {}",
            response.version
        );
    }

    if response.ok {
        return Ok(response.output.unwrap_or_default());
    }

    let message = response
        .error
        .map(|error| error.message)
        .unwrap_or_else(|| "Host daemon request failed".to_owned());
    Err(anyhow!(message))
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
