use super::*;

pub(in crate::up) fn report_deferred_config_messages(config: &ResolvedConfig) {
    for notice in security_notices(config) {
        ui::notice(&notice);
    }
    for warning in deferred_config_warnings(config) {
        ui::warn(&warning);
    }
}

pub(in crate::up) fn security_notices(config: &ResolvedConfig) -> Vec<String> {
    let mut notices = Vec::new();

    if matches!(
        config.devcontainer.source,
        Some(ResolvedDevcontainerSource::Dockerfile(_))
    ) {
        notices.push(
            "This dev container builds a workspace Dockerfile, which can execute arbitrary build steps. Review Dockerfile contents before running untrusted repositories."
                .to_owned(),
        );
    }
    if !config.features.is_empty() {
        notices.push(
            "This dev container installs Features, whose install.sh scripts execute during image build. Review Feature sources and lock digests before running untrusted repositories."
                .to_owned(),
        );
    }
    if !config.devcontainer.entrypoints.is_empty() {
        notices.push(
            "This dev container configures entrypoint commands that execute when the container starts. Review entrypoint scripts before running untrusted repositories."
                .to_owned(),
        );
    }
    if config
        .devcontainer
        .lifecycle
        .as_ref()
        .is_some_and(|lifecycle| lifecycle.has_commands())
    {
        notices.push(
            "This dev container defines lifecycle commands that execute on the host or in the container. Review lifecycle commands before running untrusted repositories."
                .to_owned(),
        );
    }
    if config
        .devcontainer
        .user_env_probe
        .is_some_and(|probe| probe != crate::config::layer::LayerUserEnvProbe::None)
    {
        notices.push(
            "This dev container enables userEnvProbe, which can run shell startup files in the container. Set userEnvProbe to \"none\" for untrusted repositories."
                .to_owned(),
        );
    }
    if config.devcontainer.privileged_enabled() {
        notices.push(
            "This dev container requests privileged mode, which grants broad container privileges. Remove privileged=true before running untrusted repositories."
                .to_owned(),
        );
    }
    if !config.devcontainer.cap_add.is_empty() {
        notices.push(format!(
            "This dev container adds Linux capabilities ({}), which can weaken container isolation. Remove capAdd entries before running untrusted repositories.",
            config.devcontainer.cap_add.join(", ")
        ));
    }
    if !config.devcontainer.security_opt.is_empty() {
        notices.push(format!(
            "This dev container sets Docker security options ({}), which can weaken container isolation. Remove securityOpt entries before running untrusted repositories.",
            config.devcontainer.security_opt.join(", ")
        ));
    }
    if has_extra_bind_mounts(config) {
        notices.push(
            "This dev container declares additional bind mounts that can expose host files. Review mount sources or remove extra bind mounts before running untrusted repositories."
                .to_owned(),
        );
    }
    if config.credentials.git.enabled
        && (config.credentials.git.copy_user
            || config.credentials.git.copy_global_config
            || matches!(
                config.credentials.git.https,
                GitHttpsMode::HostHelper | GitHttpsMode::HostHelperReadOnly
            ))
    {
        notices.push(
            "Git credential forwarding is enabled; use https = \"host-helper-read-only\" or set [credentials.git].enabled = false before running untrusted repositories."
                .to_owned(),
        );
    }
    if config.credentials.git.enabled && config.credentials.git.ssh_agent != SshAgentMode::Off {
        notices.push(
            "SSH agent forwarding is enabled; set [credentials.git].enabled = false or ssh_agent = \"off\" before running untrusted repositories."
                .to_owned(),
        );
    }
    if config.credentials.github.enabled
        && config.credentials.github.mode != GithubCredentialsMode::Off
    {
        notices.push(
            "GitHub credential forwarding is enabled; set [credentials.github].enabled = false before running untrusted repositories."
                .to_owned(),
        );
    }
    notices
}

pub(in crate::up) fn deferred_config_warnings(config: &ResolvedConfig) -> Vec<String> {
    let mut warnings = Vec::new();

    if config.devcontainer.publish_ports.iter().any(|port| {
        port.host_ip
            .as_deref()
            .is_none_or(|host_ip| host_ip != "127.0.0.1")
    }) {
        warnings.push(
            "This dev container publishes appPort through Docker, which may bind outside localhost when no host IP is specified. Use forwardPorts, decune [[ports]], or CLI -p for localhost-only access."
                .to_owned(),
        );
    }
    warnings.extend(unsupported_port_attribute_warnings(config));
    warnings
}

fn has_extra_bind_mounts(config: &ResolvedConfig) -> bool {
    config
        .mounts
        .iter()
        .any(|mount| mount.mount_type == MountType::Bind)
        || config
            .devcontainer
            .mounts
            .iter()
            .any(devcontainer_mount_is_bind_or_unknown)
        || config
            .devcontainer
            .workspace_mount
            .as_ref()
            .is_some_and(|mount| {
                devcontainer_mount_is_bind_or_unknown(
                    &crate::config::layer::LayerDevcontainerMount::String(mount.clone()),
                )
            })
}

fn devcontainer_mount_is_bind_or_unknown(
    mount: &crate::config::layer::LayerDevcontainerMount,
) -> bool {
    match devcontainer_mount_type(mount) {
        Ok(mount_type) => mount_type == MountType::Bind,
        Err(_) => true,
    }
}

fn unsupported_port_attribute_warnings(config: &ResolvedConfig) -> Vec<String> {
    let mut warnings = Vec::new();

    for (key, attributes) in &config.devcontainer.port_attributes {
        warnings.extend(unsupported_single_port_attribute_warnings(
            &format!("portsAttributes.{key}"),
            attributes,
        ));
    }
    if let Some(attributes) = &config.devcontainer.other_ports_attributes {
        warnings.extend(unsupported_single_port_attribute_warnings(
            "otherPortsAttributes",
            attributes,
        ));
    }

    warnings
}

fn unsupported_single_port_attribute_warnings(
    path: &str,
    attributes: &ResolvedPortAttributes,
) -> Vec<String> {
    let mut warnings = Vec::new();

    if let Some(protocol) = &attributes.unsupported_protocol {
        warnings.push(format!(
            "{path}.protocol is ignored (value: {protocol}); raw TCP forwarding only supports label, onAutoForward, and requireLocalPort."
        ));
    }
    if attributes.unsupported_elevate_if_needed.is_some() {
        warnings.push(format!(
            "{path}.elevateIfNeeded is ignored; low-port privilege elevation is not supported."
        ));
    }

    warnings
}
