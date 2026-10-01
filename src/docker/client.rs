use crate::runtime::docker_cli::DockerCli;

#[derive(Clone)]
pub(crate) struct DockerClient {
    cli: DockerCli,
}

impl DockerClient {
    pub(crate) fn connect_from_env() -> Self {
        Self {
            cli: DockerCli::default(),
        }
    }

    #[cfg(test)]
    pub(crate) const fn from_cli(cli: DockerCli) -> Self {
        Self { cli }
    }

    pub(crate) const fn cli(&self) -> &DockerCli {
        &self.cli
    }
}
