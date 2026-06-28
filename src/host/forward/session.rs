use std::{
    net::IpAddr,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use decune_container_protocol::ForwardAgentRequest;
use tokio::{
    io::copy_bidirectional,
    net::{TcpListener, TcpStream, UnixStream},
    task::JoinHandle,
};

use crate::{config::types::PortProtocol, docker::ports::ResolvedForwardPort, ui};

use super::{
    agent::{send_agent_shutdown, write_agent_request},
    auto::{AutoForwardConfig, run_auto_forward_loop},
    status::{ForwardStatusRegistry, ForwardStatusSource},
};

#[derive(Debug)]
pub(crate) struct ForwardSession {
    agent_socket_path: PathBuf,
    secret: String,
    listeners: Vec<ForwardListener>,
    auto_task: Option<JoinHandle<()>>,
}

#[derive(Debug)]
pub(super) struct ForwardListener {
    port: ResolvedForwardPort,
    task: JoinHandle<()>,
    #[cfg(test)]
    local_addr: std::net::SocketAddr,
}

impl Drop for ForwardListener {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl ForwardListener {
    pub(super) const fn port(&self) -> &ResolvedForwardPort {
        &self.port
    }
}

pub(crate) async fn start_forward_session_with_auto(
    forward_ports: &[ResolvedForwardPort],
    auto_forward: Option<AutoForwardConfig>,
    agent_socket_path: PathBuf,
    secret: String,
    status_registry: Option<ForwardStatusRegistry>,
) -> Result<ForwardSession> {
    let listeners = match start_forward_listeners(forward_ports, &agent_socket_path, &secret).await
    {
        Ok(listeners) => listeners,
        Err(error) => {
            let _ = send_agent_shutdown(&agent_socket_path, &secret).await;
            return Err(error);
        }
    };
    let active_forward_ports = listeners
        .iter()
        .map(|listener| listener.port.clone())
        .collect::<Vec<_>>();
    if let Some(registry) = &status_registry {
        for port in &active_forward_ports {
            registry.record(port, ForwardStatusSource::Configured);
        }
    }
    let auto_task = auto_forward.map(|config| {
        tokio::spawn(run_auto_forward_loop(
            active_forward_ports,
            config,
            agent_socket_path.clone(),
            secret.clone(),
            status_registry.clone(),
        ))
    });

    Ok(ForwardSession {
        agent_socket_path,
        secret,
        listeners,
        auto_task,
    })
}

pub(super) async fn start_forward_listeners(
    forward_ports: &[ResolvedForwardPort],
    agent_socket_path: &Path,
    secret: &str,
) -> Result<Vec<ForwardListener>> {
    let mut listeners = Vec::new();
    for port in forward_ports {
        if port.protocol != PortProtocol::Tcp {
            bail!("Unsupported port forwarding protocol: {:?}", port.protocol);
        }
        let host_ip = port
            .host_ip
            .parse::<IpAddr>()
            .with_context(|| format!("Invalid host IP for port forwarding: {}", port.host_ip))?;
        let (listener, actual_host) = bind_forward_listener(host_ip, port).await?;
        let local_addr = listener.local_addr().with_context(|| {
            format!(
                "Failed to inspect port forwarding listener: {}:{}",
                port.host_ip, port.host
            )
        })?;
        let actual_host = if actual_host == 0 {
            local_addr.port()
        } else {
            actual_host
        };
        let mut resolved_port = port.clone();
        resolved_port.host = actual_host;
        if resolved_port.require_local && resolved_port.host != resolved_port.requested_host {
            ui::warn(&format!(
                "Local port {}:{} is unavailable; forwarded {}:{} -> container:{} instead",
                resolved_port.host_ip,
                resolved_port.requested_host,
                resolved_port.host_ip,
                resolved_port.host,
                resolved_port.container
            ));
        }
        let target_port = port.container;
        let socket_path = agent_socket_path.to_path_buf();
        let secret = secret.to_owned();
        let task = tokio::spawn(async move {
            run_forward_listener(listener, socket_path, target_port, secret).await;
        });
        listeners.push(ForwardListener {
            port: resolved_port,
            task,
            #[cfg(test)]
            local_addr,
        });
    }

    Ok(listeners)
}

async fn bind_forward_listener(
    host_ip: IpAddr,
    port: &ResolvedForwardPort,
) -> Result<(TcpListener, u16)> {
    let mut candidate = port.host;
    loop {
        match TcpListener::bind((host_ip, candidate)).await {
            Ok(listener) => return Ok((listener, candidate)),
            Err(error) if is_addr_in_use(&error) => {
                if let Some(next) = candidate.checked_add(1) {
                    candidate = next;
                } else {
                    let listener = TcpListener::bind((host_ip, 0)).await.with_context(|| {
                        format!(
                            "Failed to bind port forwarding listener: {}:{}",
                            port.host_ip, port.host
                        )
                    })?;
                    return Ok((listener, 0));
                }
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "Failed to bind port forwarding listener: {}:{}",
                        port.host_ip, port.host
                    )
                });
            }
        }
    }
}

fn is_addr_in_use(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::AddrInUse
}

impl ForwardSession {
    #[cfg(test)]
    pub(crate) fn for_test(agent_socket_path: PathBuf, secret: impl Into<String>) -> Self {
        Self {
            agent_socket_path,
            secret: secret.into(),
            listeners: Vec::new(),
            auto_task: None,
        }
    }

    #[cfg(test)]
    fn local_addr(&self, index: usize) -> std::net::SocketAddr {
        self.listeners[index].local_addr
    }

    #[cfg(test)]
    fn forwarded_port(&self, index: usize) -> &ResolvedForwardPort {
        &self.listeners[index].port
    }

    pub(crate) async fn stop(mut self) {
        for listener in &self.listeners {
            listener.task.abort();
        }
        self.listeners.clear();
        if let Some(task) = self.auto_task.take() {
            task.abort();
        }
        let _ = send_agent_shutdown(&self.agent_socket_path, &self.secret).await;
    }
}

impl Drop for ForwardSession {
    fn drop(&mut self) {
        for listener in &self.listeners {
            listener.task.abort();
        }
        if let Some(task) = &self.auto_task {
            task.abort();
        }
    }
}

async fn run_forward_listener(
    listener: TcpListener,
    agent_socket_path: PathBuf,
    target_port: u16,
    secret: String,
) {
    while let Ok((client, _)) = listener.accept().await {
        let socket_path = agent_socket_path.clone();
        let secret = secret.clone();
        tokio::spawn(async move {
            let _ = proxy_client_connection(client, &socket_path, target_port, &secret).await;
        });
    }
}

async fn proxy_client_connection(
    mut client: TcpStream,
    agent_socket_path: &Path,
    target_port: u16,
    secret: &str,
) -> Result<()> {
    let mut agent = UnixStream::connect(agent_socket_path)
        .await
        .with_context(|| {
            format!(
                "Failed to connect to port forwarding agent socket: {}",
                agent_socket_path.display()
            )
        })?;
    write_agent_request(
        &mut agent,
        ForwardAgentRequest {
            port: Some(target_port),
            shutdown: None,
            secret: Some(secret.to_owned()),
            scan: None,
        },
    )
    .await?;
    copy_bidirectional(&mut client, &mut agent)
        .await
        .context("Failed to proxy port forwarding stream")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::time::timeout;

    use super::*;
    use crate::host::forward::agent::{
        ForwardAgentAccess, run_forward_agent_at_with_access, tests::wait_for_socket,
    };
    use crate::host::forward::tests::forward_port;

    #[test]
    fn forwards_streams_through_agent_to_localhost_target() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let temp = TempDir::new().unwrap();

        runtime.block_on(async {
            let echo = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let target_port = echo.local_addr().unwrap().port();
            let echo_task = tokio::spawn(async move {
                let (mut stream, _) = echo.accept().await.unwrap();
                let mut bytes = [0; 4];
                stream.read_exact(&mut bytes).await.unwrap();
                stream.write_all(&bytes).await.unwrap();
            });
            let agent_socket = temp.path().join("forward-agent.sock");
            let agent_task = tokio::spawn({
                let agent_socket = agent_socket.clone();
                async move {
                    run_forward_agent_at_with_access(
                        &agent_socket,
                        ForwardAgentAccess::new([target_port], "test-secret".to_owned()),
                    )
                    .await
                    .unwrap();
                }
            });
            wait_for_socket(&agent_socket).await;
            let session = start_forward_session_with_auto(
                &[forward_port(0, target_port)],
                None,
                agent_socket.clone(),
                "test-secret".to_owned(),
                None,
            )
            .await
            .unwrap();

            let mut client = TcpStream::connect(session.local_addr(0)).await.unwrap();
            client.write_all(b"ping").await.unwrap();
            let mut response = [0; 4];
            client.read_exact(&mut response).await.unwrap();

            assert_eq!(&response, b"ping");
            session.stop().await;
            echo_task.await.unwrap();
            agent_task.await.unwrap();
            assert!(!agent_socket.exists());
        });
    }

    #[test]
    fn stop_closes_host_listener() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let temp = TempDir::new().unwrap();

        runtime.block_on(async {
            let agent_socket = temp.path().join("forward-agent.sock");
            let agent_task = tokio::spawn({
                let agent_socket = agent_socket.clone();
                async move {
                    run_forward_agent_at_with_access(
                        &agent_socket,
                        ForwardAgentAccess::new([9], "test-secret".to_owned()),
                    )
                    .await
                    .unwrap();
                }
            });
            wait_for_socket(&agent_socket).await;
            let session = start_forward_session_with_auto(
                &[forward_port(0, 9)],
                None,
                agent_socket,
                "test-secret".to_owned(),
                None,
            )
            .await
            .unwrap();
            let local_addr = session.local_addr(0);

            session.stop().await;
            agent_task.await.unwrap();
            assert!(TcpStream::connect(local_addr).await.is_err());
        });
    }

    #[test]
    fn listener_falls_back_when_requested_host_port_is_occupied() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let temp = TempDir::new().unwrap();

        runtime.block_on(async {
            let occupied = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let occupied_port = occupied.local_addr().unwrap().port();
            let agent_socket = temp.path().join("forward-agent.sock");
            let agent_task = tokio::spawn({
                let agent_socket = agent_socket.clone();
                async move {
                    run_forward_agent_at_with_access(
                        &agent_socket,
                        ForwardAgentAccess::new([9], "test-secret".to_owned()),
                    )
                    .await
                    .unwrap();
                }
            });
            wait_for_socket(&agent_socket).await;

            let mut port = forward_port(occupied_port, 9);
            port.require_local = true;
            let session = start_forward_session_with_auto(
                &[port],
                None,
                agent_socket.clone(),
                "test-secret".to_owned(),
                None,
            )
            .await
            .unwrap();

            assert_eq!(session.forwarded_port(0).requested_host, occupied_port);
            assert_ne!(session.forwarded_port(0).host, occupied_port);
            assert_ne!(session.local_addr(0).port(), occupied_port);

            session.stop().await;
            agent_task.await.unwrap();
            assert!(!agent_socket.exists());
        });
    }

    #[test]
    fn listener_start_failure_stops_agent() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let temp = TempDir::new().unwrap();

        runtime.block_on(async {
            let agent_socket = temp.path().join("forward-agent.sock");
            let agent_task = tokio::spawn({
                let agent_socket = agent_socket.clone();
                async move {
                    run_forward_agent_at_with_access(
                        &agent_socket,
                        ForwardAgentAccess::new([9], "test-secret".to_owned()),
                    )
                    .await
                    .unwrap();
                }
            });
            wait_for_socket(&agent_socket).await;
            let mut port = forward_port(0, 9);
            port.protocol = PortProtocol::Udp;
            let error = start_forward_session_with_auto(
                &[port],
                None,
                agent_socket.clone(),
                "test-secret".to_owned(),
                None,
            )
            .await
            .unwrap_err();
            let message = format!("{error:#}");

            assert!(message.contains("Unsupported port forwarding protocol"));
            timeout(std::time::Duration::from_secs(1), agent_task)
                .await
                .unwrap()
                .unwrap();
            assert!(!agent_socket.exists());
        });
    }
}
