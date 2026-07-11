use crate::{
    config::{
        resolved::{ResolvedConfig, ResolvedDevcontainerSource, ResolvedPortAttributes},
        types::{GitHttpsMode, GithubCredentialsMode, MountType, SshAgentMode},
    },
    docker::mounts::devcontainer_mount_type,
    ui,
};

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
        .is_some_and(crate::devcontainer::lifecycle::LifecycleDefinition::has_commands)
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
    if !config.compose.clone_isolation.enabled
        && !config.compose.clone_isolation.endpoints.is_empty()
    {
        warnings.push(
            "compose.clone_isolation.endpoints is ignored because compose.clone_isolation.enabled is false; set compose.clone_isolation.enabled = true to enable endpoint rewriting."
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
    devcontainer_mount_type(mount).map_or(true, |mount_type| mount_type == MountType::Bind)
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{deferred_config_warnings, security_notices};
    use crate::config::{
        layer::LayerUserEnvProbe,
        resolved::{
            ResolvedConfig, ResolvedDevcontainerSource, ResolvedPortAttributes, ResolvedPublishPort,
        },
        types::{PortProtocol, SshAgentMode},
    };

    #[test]
    fn security_notices_are_empty_for_default_plan_security_surface() {
        let mut config = ResolvedConfig::default();
        config.credentials.git.enabled = false;
        config.credentials.github.enabled = false;
        let notices = security_notices(&config);

        assert!(notices.is_empty());
    }
    #[test]
    fn security_notices_report_risky_container_settings() {
        let mut config = ResolvedConfig::default();
        config.credentials.git.enabled = false;
        config.credentials.github.enabled = false;
        config.devcontainer.privileged = Some(true);
        config.devcontainer.cap_add = vec!["SYS_PTRACE".to_owned()];
        config.devcontainer.security_opt = vec!["seccomp=unconfined".to_owned()];
        config.devcontainer.mounts = vec![crate::config::layer::LayerDevcontainerMount::String(
            "type=bind,source=/tmp,target=/host-tmp".to_owned(),
        )];

        let notices = security_notices(&config);

        assert!(notices.iter().any(|notice| notice.contains("privileged")));
        assert!(notices.iter().any(|notice| notice.contains("SYS_PTRACE")));
        assert!(
            notices
                .iter()
                .any(|notice| notice.contains("seccomp=unconfined"))
        );
        assert!(
            notices
                .iter()
                .any(|notice| notice.contains("additional bind mounts"))
        );
        assert!(notices.iter().all(|notice| !notice.contains("/tmp")));
    }
    #[test]
    fn security_notices_skip_devcontainer_volume_mounts() {
        let mut config = ResolvedConfig::default();
        config.credentials.git.enabled = false;
        config.credentials.github.enabled = false;
        config.devcontainer.mounts = vec![
            crate::config::layer::LayerDevcontainerMount::String(
                "type=volume,source=project-cache,target=/cache".to_owned(),
            ),
            crate::config::layer::LayerDevcontainerMount::Object(
                [
                    ("type".to_owned(), serde_json::json!("volume")),
                    ("source".to_owned(), serde_json::json!("project-deps")),
                    ("target".to_owned(), serde_json::json!("/deps")),
                ]
                .into(),
            ),
        ];
        config.devcontainer.workspace_mount =
            Some("type=volume,source=project-workspace,target=/workspace".to_owned());

        let notices = security_notices(&config);

        assert!(
            notices
                .iter()
                .all(|notice| !notice.contains("additional bind mounts"))
        );
    }
    #[test]
    fn security_notices_report_devcontainer_workspace_bind_mount() {
        let mut config = ResolvedConfig::default();
        config.credentials.git.enabled = false;
        config.credentials.github.enabled = false;
        config.devcontainer.workspace_mount =
            Some("type=bind,source=${localWorkspaceFolder},target=/workspace".to_owned());

        let notices = security_notices(&config);

        assert!(
            notices
                .iter()
                .any(|notice| notice.contains("additional bind mounts"))
        );
    }
    #[test]
    fn security_notices_report_code_execution_surfaces() {
        let mut config = ResolvedConfig::default();
        config.credentials.git.enabled = false;
        config.credentials.github.enabled = false;
        config.features = vec![crate::config::resolved::ResolvedFeature {
            id: "tool".to_owned(),
            canonical_id: "ghcr.io/example/features/tool".to_owned(),
            options: BTreeMap::new(),
        }];
        config.devcontainer.source = Some(ResolvedDevcontainerSource::Dockerfile(
            crate::config::layer::LayerDevcontainerBuild {
                dockerfile: "Dockerfile".to_owned(),
                context: None,
                args: BTreeMap::new(),
                options: Vec::new(),
                target: None,
                cache_from: Vec::new(),
            },
        ));
        config.devcontainer.entrypoints = vec!["/usr/local/bin/start".to_owned()];
        config.devcontainer.lifecycle = Some(
            crate::devcontainer::lifecycle::parse_lifecycle_definition(&BTreeMap::from([(
                crate::devcontainer::metadata::LifecycleProperty::PostStartCommand,
                serde_json::json!("make setup"),
            )]))
            .unwrap(),
        );
        config.devcontainer.user_env_probe = Some(LayerUserEnvProbe::LoginShell);

        let notices = security_notices(&config);

        assert!(notices.iter().any(|notice| notice.contains("Dockerfile")));
        assert!(notices.iter().any(|notice| notice.contains("install.sh")));
        assert!(notices.iter().any(|notice| notice.contains("entrypoint")));
        assert!(notices.iter().any(|notice| notice.contains("lifecycle")));
        assert!(notices.iter().any(|notice| {
            notice.contains("userEnvProbe") && notice.contains("userEnvProbe to \"none\"")
        }));
    }
    #[test]
    fn security_notices_report_enabled_credentials() {
        let notices = security_notices(&ResolvedConfig::default());

        assert!(notices.iter().any(|notice| {
            notice.contains("Git credential forwarding")
                && notice.contains("[credentials.git].enabled = false")
        }));
        assert!(notices.iter().any(|notice| {
            notice.contains("SSH agent forwarding") && notice.contains("ssh_agent = \"off\"")
        }));
        assert!(notices.iter().any(|notice| {
            notice.contains("GitHub credential forwarding")
                && notice.contains("[credentials.github].enabled = false")
        }));

        let mut disabled = ResolvedConfig::default();
        disabled.credentials.git.enabled = false;
        disabled.credentials.github.enabled = false;
        let disabled_notices = security_notices(&disabled);
        assert!(
            disabled_notices
                .iter()
                .all(|notice| !notice.contains("credential forwarding"))
        );

        let mut ssh_off = ResolvedConfig::default();
        ssh_off.credentials.git.ssh_agent = SshAgentMode::Off;
        let ssh_off_notices = security_notices(&ssh_off);
        assert!(
            ssh_off_notices
                .iter()
                .all(|notice| !notice.contains("SSH agent forwarding"))
        );
    }
    #[test]
    fn deferred_config_warnings_report_app_port_without_explicit_host_ip() {
        let mut config = ResolvedConfig::default();
        config.devcontainer.publish_ports = vec![ResolvedPublishPort {
            container: 8080,
            host: Some(18080),
            host_ip: None,
            protocol: PortProtocol::Tcp,
        }];

        let warnings = deferred_config_warnings(&config);
        let warning = warnings
            .iter()
            .find(|warning| warning.contains("appPort"))
            .expect("expected appPort warning");

        assert!(warning.contains("forwardPorts"));
        assert!(warning.contains("[[ports]]"));
        assert!(warning.contains("localhost-only"));
    }
    #[test]
    fn deferred_config_warnings_skip_localhost_only_app_port() {
        let mut config = ResolvedConfig::default();
        config.devcontainer.publish_ports = vec![ResolvedPublishPort {
            container: 8080,
            host: Some(18080),
            host_ip: Some("127.0.0.1".to_owned()),
            protocol: PortProtocol::Tcp,
        }];

        let warnings = deferred_config_warnings(&config);

        assert!(!warnings.iter().any(|warning| warning.contains("appPort")));
    }
    #[test]
    fn deferred_config_warnings_report_endpoints_when_clone_isolation_is_disabled() {
        let mut config = ResolvedConfig::default();
        config.compose.clone_isolation.endpoints.push(
            crate::config::resolved::ResolvedComposeCloneIsolationEndpoint {
                service: "app".to_owned(),
                env: "HOST_AGENT_ENDPOINT".to_owned(),
                value: "grpc://${decune.network.grpc.gateway}:50051".to_owned(),
            },
        );

        let warnings = deferred_config_warnings(&config);

        assert!(warnings.iter().any(|warning| {
            warning.contains("compose.clone_isolation.endpoints is ignored")
                && warning.contains("compose.clone_isolation.enabled is false")
                && warning.contains("compose.clone_isolation.enabled = true")
        }));
    }
    #[test]
    fn deferred_config_warnings_report_unsupported_port_attributes() {
        let mut config = ResolvedConfig::default();
        config.devcontainer.port_attributes.insert(
            "3000".to_owned(),
            ResolvedPortAttributes {
                label: Some("web".to_owned()),
                on_auto_forward: None,
                require_local_port: Some(true),
                unsupported_protocol: Some("https".to_owned()),
                unsupported_elevate_if_needed: Some(true),
            },
        );
        config.devcontainer.other_ports_attributes = Some(ResolvedPortAttributes {
            label: None,
            on_auto_forward: None,
            require_local_port: None,
            unsupported_protocol: Some("http".to_owned()),
            unsupported_elevate_if_needed: None,
        });

        let warnings = deferred_config_warnings(&config);

        assert!(warnings.iter().any(|warning| {
            warning.contains("portsAttributes.3000.protocol")
                && warning.contains("ignored")
                && warning.contains("label")
        }));
        assert!(warnings.iter().any(|warning| {
            warning.contains("portsAttributes.3000.elevateIfNeeded") && warning.contains("ignored")
        }));
        assert!(warnings.iter().any(|warning| {
            warning.contains("otherPortsAttributes.protocol") && warning.contains("ignored")
        }));
    }
}
