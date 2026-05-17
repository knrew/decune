use bollard::models::{Mount, MountType as DockerMountType};

use crate::config::types::MountType;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DockerMountSpec {
    pub(crate) source: Option<String>,
    pub(crate) target: String,
    pub(crate) mount_type: MountType,
    pub(crate) read_only: bool,
}

impl DockerMountSpec {
    pub(crate) fn to_bollard_mount(&self) -> Mount {
        Mount {
            target: Some(self.target.clone()),
            source: self.source.clone(),
            typ: Some(docker_mount_type(self.mount_type)),
            read_only: Some(self.read_only),
            ..Default::default()
        }
    }
}

fn docker_mount_type(mount_type: MountType) -> DockerMountType {
    match mount_type {
        MountType::Bind => DockerMountType::BIND,
        MountType::Volume => DockerMountType::VOLUME,
        MountType::Tmpfs => DockerMountType::TMPFS,
    }
}
