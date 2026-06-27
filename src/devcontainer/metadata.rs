use std::collections::BTreeMap;

use anyhow::Result;
use serde_json::Value;

use crate::{
    devcontainer::mounts::DevcontainerMount,
    devcontainer::ports::{DevcontainerPort, DevcontainerPortAttributes},
};

mod args;
mod conversion;
mod raw;
mod serde_helpers;
mod types;

pub(crate) use types::{
    DevcontainerBuild, DevcontainerCompose, DevcontainerRunArg, DevcontainerShutdownAction,
    DevcontainerSource, LifecycleProperty, UserEnvProbe,
};

use raw::{MetadataLayerKind, SourceRequirement, parse_metadata_value};

pub(crate) fn parse_metadata(value: Value) -> Result<DevcontainerMetadata> {
    parse_metadata_value(
        value,
        SourceRequirement::Required,
        MetadataLayerKind::DevcontainerJson,
    )
}

pub(crate) fn parse_metadata_layer(value: Value) -> Result<DevcontainerMetadata> {
    parse_metadata_value(
        value,
        SourceRequirement::Optional,
        MetadataLayerKind::Generic,
    )
}

pub(crate) fn parse_image_metadata_layer(value: Value) -> Result<DevcontainerMetadata> {
    parse_metadata_value(
        value,
        SourceRequirement::Optional,
        MetadataLayerKind::ImageLabel,
    )
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DevcontainerMetadata {
    source: Option<DevcontainerSource>,
    features: BTreeMap<String, Value>,
    override_feature_install_order: Vec<String>,
    mounts: Vec<DevcontainerMount>,
    workspace_mount: Option<String>,
    workspace_folder: Option<String>,
    container_env: BTreeMap<String, String>,
    remote_env: BTreeMap<String, String>,
    remote_user: Option<String>,
    container_user: Option<String>,
    update_remote_user_uid: Option<bool>,
    override_command: Option<bool>,
    user_env_probe: Option<UserEnvProbe>,
    forward_ports: Vec<DevcontainerPort>,
    ports_attributes: BTreeMap<String, DevcontainerPortAttributes>,
    other_ports_attributes: Option<DevcontainerPortAttributes>,
    app_port: Vec<DevcontainerPort>,
    run_args: Vec<DevcontainerRunArg>,
    init: Option<bool>,
    privileged: Option<bool>,
    cap_add: Vec<String>,
    security_opt: Vec<String>,
    entrypoints: Vec<String>,
    shutdown_action: Option<DevcontainerShutdownAction>,
    lifecycle: BTreeMap<LifecycleProperty, Value>,
    customizations: Option<Value>,
    unsupported_properties: BTreeMap<String, Value>,
}

impl DevcontainerMetadata {
    #[cfg(test)]
    pub(crate) const fn source(&self) -> Option<&DevcontainerSource> {
        self.source.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn mounts(&self) -> &[DevcontainerMount] {
        &self.mounts
    }

    #[cfg(test)]
    pub(crate) fn workspace_mount(&self) -> Option<&str> {
        self.workspace_mount.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn workspace_folder(&self) -> Option<&str> {
        self.workspace_folder.as_deref()
    }

    #[cfg(test)]
    pub(crate) const fn container_env(&self) -> &BTreeMap<String, String> {
        &self.container_env
    }

    #[cfg(test)]
    pub(crate) const fn remote_env(&self) -> &BTreeMap<String, String> {
        &self.remote_env
    }

    #[cfg(test)]
    pub(crate) fn remote_user(&self) -> Option<&str> {
        self.remote_user.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn container_user(&self) -> Option<&str> {
        self.container_user.as_deref()
    }

    #[cfg(test)]
    pub(crate) const fn update_remote_user_uid(&self) -> Option<bool> {
        self.update_remote_user_uid
    }

    #[cfg(test)]
    pub(crate) const fn user_env_probe(&self) -> Option<&UserEnvProbe> {
        self.user_env_probe.as_ref()
    }

    pub(crate) fn forward_ports(&self) -> &[DevcontainerPort] {
        &self.forward_ports
    }

    #[cfg(test)]
    pub(crate) const fn ports_attributes(&self) -> &BTreeMap<String, DevcontainerPortAttributes> {
        &self.ports_attributes
    }

    #[cfg(test)]
    pub(crate) const fn other_ports_attributes(&self) -> Option<&DevcontainerPortAttributes> {
        self.other_ports_attributes.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn app_port(&self) -> &[DevcontainerPort] {
        &self.app_port
    }

    #[cfg(test)]
    pub(crate) fn run_args(&self) -> &[DevcontainerRunArg] {
        &self.run_args
    }

    #[cfg(test)]
    pub(crate) const fn init(&self) -> Option<bool> {
        self.init
    }

    #[cfg(test)]
    pub(crate) const fn privileged(&self) -> Option<bool> {
        self.privileged
    }

    #[cfg(test)]
    pub(crate) fn cap_add(&self) -> &[String] {
        &self.cap_add
    }

    #[cfg(test)]
    pub(crate) fn security_opt(&self) -> &[String] {
        &self.security_opt
    }

    #[cfg(test)]
    pub(crate) const fn lifecycle(&self) -> &BTreeMap<LifecycleProperty, Value> {
        &self.lifecycle
    }

    #[cfg(test)]
    pub(crate) const fn customizations(&self) -> Option<&Value> {
        self.customizations.as_ref()
    }

    #[cfg(test)]
    pub(crate) const fn unsupported_properties(&self) -> &BTreeMap<String, Value> {
        &self.unsupported_properties
    }
}
