use anyhow::{Error, Result};

use crate::runtime::docker_cli::DockerCli;

#[derive(Clone)]
pub(crate) struct DockerClient {
    cli: DockerCli,
    #[allow(dead_code)]
    endpoint: String,
}

impl DockerClient {
    pub(crate) fn connect_from_env() -> Result<Self> {
        Ok(Self {
            cli: DockerCli::default(),
            endpoint: docker_endpoint_from_env(),
        })
    }

    pub(crate) fn cli(&self) -> &DockerCli {
        &self.cli
    }

    #[allow(dead_code)]
    pub(crate) fn endpoint(&self) -> &str {
        &self.endpoint
    }

    #[allow(dead_code)]
    pub(crate) async fn ping(&self) -> Result<()> {
        self.cli()
            .ping()
            .await
            .map_err(|error| connection_error("ping Docker daemon", &self.endpoint, error))
    }

    #[allow(dead_code)]
    pub(crate) async fn version(&self) -> Result<serde_json::Value> {
        self.cli()
            .version_json()
            .await
            .map_err(|error| connection_error("read Docker daemon version", &self.endpoint, error))
    }
}

fn docker_endpoint_from_env() -> String {
    docker_endpoint_from_env_value(std::env::var("DOCKER_HOST").ok().as_deref())
}

fn docker_endpoint_from_env_value(docker_host: Option<&str>) -> String {
    match docker_host {
        Some(docker_host) => docker_host.to_owned(),
        None => default_endpoint().to_owned(),
    }
}

fn default_endpoint() -> &'static str {
    #[cfg(unix)]
    {
        "unix:///var/run/docker.sock"
    }

    #[cfg(windows)]
    {
        "npipe:////./pipe/docker_engine"
    }

    #[cfg(not(any(unix, windows)))]
    {
        "tcp://localhost:2375"
    }
}

#[allow(dead_code)]
fn connection_error(action: &'static str, endpoint: &str, source: impl Into<Error>) -> Error {
    source.into().context(format!(
        "Failed to {action}: {endpoint}. {}",
        guidance_for_endpoint(endpoint)
    ))
}

#[allow(dead_code)]
fn guidance_for_endpoint(endpoint: &str) -> &'static str {
    if endpoint.starts_with("unix://") || endpoint.starts_with("npipe://") {
        "Ensure Docker or Podman is running, DOCKER_HOST is set correctly, and socket permissions allow access."
    } else {
        "Ensure Docker or Podman is running and DOCKER_HOST is set correctly."
    }
}

#[cfg(test)]
mod tests {
    use anyhow::anyhow;

    use super::{connection_error, docker_endpoint_from_env_value, guidance_for_endpoint};

    #[test]
    fn default_endpoint_describes_local_daemon_when_docker_host_is_unset() {
        assert_eq!(
            docker_endpoint_from_env_value(None),
            platform_default_endpoint()
        );
    }

    #[test]
    fn endpoint_description_uses_docker_host_when_set() {
        assert_eq!(
            docker_endpoint_from_env_value(Some("tcp://127.0.0.1:2375")),
            "tcp://127.0.0.1:2375"
        );
    }

    #[test]
    fn connection_error_includes_endpoint_and_guidance() {
        let error = connection_error(
            "ping Docker daemon",
            "unix:///tmp/missing-docker.sock",
            anyhow!("connection refused"),
        );

        let message = format!("{error:#}");
        assert!(message.contains("Failed to ping Docker daemon: unix:///tmp/missing-docker.sock"));
        assert!(message.contains("Ensure Docker or Podman is running"));
        assert!(message.contains("DOCKER_HOST"));
        assert!(message.contains("connection refused"));
    }

    #[test]
    fn guidance_mentions_socket_permissions_for_unix_socket() {
        let guidance = guidance_for_endpoint("unix:///var/run/docker.sock");

        assert!(guidance.contains("socket permissions"));
    }

    fn platform_default_endpoint() -> &'static str {
        #[cfg(unix)]
        {
            "unix:///var/run/docker.sock"
        }

        #[cfg(windows)]
        {
            "npipe:////./pipe/docker_engine"
        }

        #[cfg(not(any(unix, windows)))]
        {
            "tcp://localhost:2375"
        }
    }

    #[test]
    fn error_keeps_source_chain() {
        let error = connection_error(
            "connect to Docker daemon",
            "tcp://127.0.0.1:2375",
            anyhow!("tcp failure"),
        );

        let mut source = error.source();
        let mut found_source = false;
        while let Some(current) = source {
            if current.to_string().contains("tcp failure") {
                found_source = true;
                break;
            }
            source = current.source();
        }

        assert!(found_source);
    }
}
