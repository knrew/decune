use anyhow::Result;

use crate::runtime::docker_cli::DockerCli;

#[derive(Clone)]
pub(crate) struct DockerClient {
    cli: DockerCli,
}

impl DockerClient {
    pub(crate) fn connect_from_env() -> Result<Self> {
        Ok(Self {
            cli: DockerCli::default(),
        })
    }

    pub(crate) const fn cli(&self) -> &DockerCli {
        &self.cli
    }
}
