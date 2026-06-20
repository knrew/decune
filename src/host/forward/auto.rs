use std::{collections::BTreeMap, path::Path, path::PathBuf};

use anyhow::{Context, Result};
use decune_container_protocol::{
    ForwardAgentRequest, ForwardAgentScanRequest, ForwardAgentScanResponse,
};
use tokio::{
    io::AsyncReadExt,
    net::UnixStream,
    time::{Duration, sleep},
};

use crate::{
    config::{
        resolved::{
            ResolvedAutoPorts, ResolvedConfig, ResolvedPortAttributes, ResolvedPublishPort,
        },
        types::OnAutoForward,
    },
    docker::ports::{ResolvedForwardPort, resolve_auto_forward_ports},
    ui,
};

use super::{
    agent::write_agent_request,
    session::{ForwardListener, start_forward_listeners},
};

const AUTO_FORWARD_INITIAL_DELAY: Duration = Duration::from_secs(3);
const AUTO_FORWARD_SCAN_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub(crate) struct AutoForwardConfig {
    auto: ResolvedAutoPorts,
    publish_ports: Vec<ResolvedPublishPort>,
    port_attributes: BTreeMap<String, ResolvedPortAttributes>,
    other_ports_attributes: Option<ResolvedPortAttributes>,
}

impl AutoForwardConfig {
    pub(crate) fn from_config(config: &ResolvedConfig) -> Option<Self> {
        config.ports.auto.enabled.then(|| Self {
            auto: config.ports.auto.clone(),
            publish_ports: config.devcontainer.publish_ports.clone(),
            port_attributes: config.devcontainer.port_attributes.clone(),
            other_ports_attributes: config.devcontainer.other_ports_attributes.clone(),
        })
    }
}

pub(super) async fn run_auto_forward_loop(
    mut forward_ports: Vec<ResolvedForwardPort>,
    config: AutoForwardConfig,
    agent_socket_path: PathBuf,
    secret: String,
) {
    let mut listeners = Vec::new();
    let mut reported_error = false;
    sleep(AUTO_FORWARD_INITIAL_DELAY).await;

    loop {
        match scan_and_add_auto_forwards(
            &mut forward_ports,
            &mut listeners,
            &config,
            &agent_socket_path,
            &secret,
        )
        .await
        {
            Ok(()) => reported_error = false,
            Err(error) if !reported_error => {
                ui::warn(&format!("Automatic port forwarding failed: {error:#}"));
                reported_error = true;
            }
            Err(_) => {}
        }
        sleep(AUTO_FORWARD_SCAN_INTERVAL).await;
    }
}

async fn scan_and_add_auto_forwards(
    forward_ports: &mut Vec<ResolvedForwardPort>,
    listeners: &mut Vec<ForwardListener>,
    config: &AutoForwardConfig,
    agent_socket_path: &Path,
    secret: &str,
) -> Result<()> {
    let detected = request_auto_forward_ports(agent_socket_path, secret, &config.auto).await?;
    let additions = resolve_auto_forward_ports(
        detected,
        forward_ports,
        &config.publish_ports,
        &config.auto,
        &config.port_attributes,
        config.other_ports_attributes.as_ref(),
    )?;
    if additions.is_empty() {
        return Ok(());
    }

    let new_ports = additions
        .iter()
        .map(|addition| addition.port.clone())
        .collect::<Vec<_>>();
    let mut new_listeners = start_forward_listeners(&new_ports, agent_socket_path, secret).await?;
    for (addition, listener) in additions.into_iter().zip(new_listeners.iter()) {
        let port = listener.port().clone();
        if addition.on_auto_forward == OnAutoForward::Notify {
            ui::info(&format!(
                "Forwarded localhost:{} -> container:{}",
                port.host, port.container
            ));
        }
        forward_ports.push(port);
    }
    listeners.append(&mut new_listeners);

    Ok(())
}

async fn request_auto_forward_ports(
    agent_socket_path: &Path,
    secret: &str,
    auto: &ResolvedAutoPorts,
) -> Result<Vec<u16>> {
    let mut agent = UnixStream::connect(agent_socket_path)
        .await
        .with_context(|| {
            format!(
                "Failed to connect to port forwarding agent socket for auto scan: {}",
                agent_socket_path.display()
            )
        })?;
    write_agent_request(
        &mut agent,
        ForwardAgentRequest {
            port: None,
            shutdown: None,
            secret: Some(secret.to_owned()),
            scan: Some(ForwardAgentScanRequest {
                min: auto.min,
                max: auto.max,
                ignore: auto.ignore.clone(),
            }),
        },
    )
    .await?;
    let mut response = Vec::new();
    agent
        .read_to_end(&mut response)
        .await
        .context("Failed to read automatic port forwarding scan response")?;
    let response: ForwardAgentScanResponse = serde_json::from_slice(&response)
        .context("Invalid automatic port forwarding scan response JSON")?;
    Ok(response.ports)
}
