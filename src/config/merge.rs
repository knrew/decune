#![allow(dead_code)]

pub(crate) use crate::config::{
    layer::{ConfigLayer, ConfigMergeInput},
    resolved::ResolvedConfig,
};

use crate::config::{
    layer::{
        LayerAutoPorts, LayerCredentials, LayerDevcontainerMetadata, LayerDotfile, LayerFeature,
        LayerMount, LayerPort, LayerPortAttributes, LayerPublishPort,
    },
    resolved::{
        ResolvedAutoPorts, ResolvedCredentials, ResolvedDevcontainer, ResolvedDotfile,
        ResolvedFeature, ResolvedHooks, ResolvedMount, ResolvedPort, ResolvedPorts,
    },
};

pub(crate) fn resolve_config(input: ConfigMergeInput) -> ResolvedConfig {
    let mut accumulator = MergeAccumulator::default();

    for layer in [
        input.image_metadata,
        input.global,
        input.devcontainer,
        input.project,
        input.cli,
    ]
    .into_iter()
    .flatten()
    {
        accumulator.apply_layer(layer);
    }

    accumulator.into_resolved()
}

#[derive(Debug, Default)]
struct MergeAccumulator {
    shell: Option<String>,
    features: Vec<ResolvedFeature>,
    dotfiles: Vec<ResolvedDotfile>,
    mounts: Vec<ResolvedMount>,
    ports: Vec<ResolvedPort>,
    auto_ports: ResolvedAutoPorts,
    devcontainer: ResolvedDevcontainer,
    credentials: ResolvedCredentials,
    hooks: ResolvedHooks,
}

impl MergeAccumulator {
    fn apply_layer(&mut self, layer: ConfigLayer) {
        if let Some(shell) = layer.shell {
            self.shell = Some(shell);
        }

        for feature in layer.features {
            self.merge_feature(feature);
        }

        for dotfile in layer.dotfiles {
            self.merge_dotfile(dotfile);
        }

        for mount in layer.mounts {
            self.merge_mount(mount);
        }

        for port in layer.ports {
            self.merge_port(port);
        }

        if let Some(auto_ports) = layer.auto_ports {
            self.merge_auto_ports(auto_ports);
        }

        if let Some(devcontainer) = layer.devcontainer {
            self.merge_devcontainer(devcontainer);
        }

        self.merge_credentials(layer.credentials);
        self.hooks.append(layer.hooks);
    }

    fn merge_feature(&mut self, feature: LayerFeature) {
        if let Some(position) = self
            .features
            .iter()
            .position(|entry| entry.canonical_id == feature.canonical_id)
        {
            if feature.enabled {
                let existing = &mut self.features[position];
                existing.id = feature.id;
                existing.options.extend(feature.options);
            } else {
                self.features.remove(position);
            }
            return;
        }

        if feature.enabled {
            self.features.push(ResolvedFeature {
                id: feature.id,
                canonical_id: feature.canonical_id,
                options: feature.options,
            });
        }
    }

    fn merge_dotfile(&mut self, dotfile: LayerDotfile) {
        if !dotfile.enabled {
            remove_by_identity(&mut self.dotfiles, |existing| {
                existing.target == dotfile.target
            });
            return;
        }

        if let Some(source) = dotfile.source {
            replace_by_identity(
                &mut self.dotfiles,
                ResolvedDotfile {
                    source,
                    target: dotfile.target,
                    read_only: dotfile.read_only,
                    resolve_symlink: dotfile.resolve_symlink,
                    on_conflict: dotfile.on_conflict,
                },
                |left, right| left.target == right.target,
            );
        }
    }

    fn merge_mount(&mut self, mount: LayerMount) {
        if !mount.enabled {
            remove_by_identity(&mut self.mounts, |existing| existing.target == mount.target);
            return;
        }

        if let Some(mount_type) = mount.mount_type {
            replace_by_identity(
                &mut self.mounts,
                ResolvedMount {
                    source: mount.source,
                    target: mount.target,
                    mount_type,
                    read_only: mount.read_only,
                    resolve_symlink: mount.resolve_symlink,
                    create: mount.create,
                },
                |left, right| left.target == right.target,
            );
        }
    }

    fn merge_port(&mut self, port: LayerPort) {
        if !port.enabled {
            remove_by_identity(&mut self.ports, |existing| {
                existing.protocol == port.protocol
                    && existing.container == port.container
                    && existing.host_ip == port.host_ip
            });
            return;
        }

        replace_by_identity(&mut self.ports, port, same_port_identity);
    }

    fn merge_auto_ports(&mut self, auto_ports: LayerAutoPorts) {
        if let Some(enabled) = auto_ports.enabled {
            self.auto_ports.enabled = enabled;
        }
        if let Some(min) = auto_ports.min {
            self.auto_ports.min = min;
        }
        if let Some(max) = auto_ports.max {
            self.auto_ports.max = max;
        }
        if let Some(ignore) = auto_ports.ignore {
            self.auto_ports.ignore = ignore;
        }
        if let Some(on_auto_forward) = auto_ports.on_auto_forward {
            self.auto_ports.on_auto_forward = on_auto_forward;
        }
    }

    fn merge_credentials(&mut self, credentials: LayerCredentials) {
        if let Some(enabled) = credentials.git.enabled {
            self.credentials.git.enabled = enabled;
        }
        if let Some(copy_user) = credentials.git.copy_user {
            self.credentials.git.copy_user = copy_user;
        }
        if let Some(copy_global_config) = credentials.git.copy_global_config {
            self.credentials.git.copy_global_config = copy_global_config;
        }
        if let Some(https) = credentials.git.https {
            self.credentials.git.https = https;
        }
        if let Some(ssh_agent) = credentials.git.ssh_agent {
            self.credentials.git.ssh_agent = ssh_agent;
        }

        if let Some(enabled) = credentials.github.enabled {
            self.credentials.github.enabled = enabled;
        }
        if let Some(mode) = credentials.github.mode {
            self.credentials.github.mode = mode;
        }
        if let Some(install_feature_if_missing) = credentials.github.install_feature_if_missing {
            self.credentials.github.install_feature_if_missing = install_feature_if_missing;
        }
    }

    fn merge_devcontainer(&mut self, devcontainer: LayerDevcontainerMetadata) {
        if let Some(source) = devcontainer.source {
            self.devcontainer.source = Some(source);
        }

        if !devcontainer.override_feature_install_order.is_empty() {
            self.devcontainer.override_feature_install_order =
                devcontainer.override_feature_install_order;
        }
        self.devcontainer.mounts.extend(devcontainer.mounts);
        if let Some(workspace_mount) = devcontainer.workspace_mount {
            self.devcontainer.workspace_mount = Some(workspace_mount);
        }
        if let Some(workspace_folder) = devcontainer.workspace_folder {
            self.devcontainer.workspace_folder = Some(workspace_folder);
        }
        self.devcontainer
            .container_env
            .extend(devcontainer.container_env);
        self.devcontainer.remote_env.extend(devcontainer.remote_env);
        if let Some(remote_user) = devcontainer.remote_user {
            self.devcontainer.remote_user = Some(remote_user);
        }
        if let Some(container_user) = devcontainer.container_user {
            self.devcontainer.container_user = Some(container_user);
        }
        if let Some(update_remote_user_uid) = devcontainer.update_remote_user_uid {
            self.devcontainer.update_remote_user_uid = Some(update_remote_user_uid);
        }
        if let Some(user_env_probe) = devcontainer.user_env_probe {
            self.devcontainer.user_env_probe = Some(user_env_probe);
        }

        for port in devcontainer.publish_ports {
            replace_by_identity(
                &mut self.devcontainer.publish_ports,
                port,
                same_publish_port_identity,
            );
        }

        self.devcontainer
            .port_attributes
            .extend(devcontainer.port_attributes);
        merge_optional_port_attributes(
            &mut self.devcontainer.other_ports_attributes,
            devcontainer.other_ports_attributes,
        );
        self.devcontainer.run_args.extend(devcontainer.run_args);

        if let Some(init) = devcontainer.init {
            self.devcontainer.init = init;
        }
        if let Some(privileged) = devcontainer.privileged {
            self.devcontainer.privileged = privileged;
        }
        self.devcontainer.cap_add.extend(devcontainer.cap_add);
        self.devcontainer
            .security_opt
            .extend(devcontainer.security_opt);
        if let Some(lifecycle) = devcontainer.lifecycle {
            self.devcontainer.lifecycle = Some(lifecycle);
        }
    }

    fn into_resolved(self) -> ResolvedConfig {
        ResolvedConfig {
            shell: self.shell,
            features: self.features,
            dotfiles: self.dotfiles,
            mounts: self.mounts,
            ports: ResolvedPorts {
                entries: self.ports,
                auto: self.auto_ports,
            },
            devcontainer: self.devcontainer,
            credentials: self.credentials,
            hooks: self.hooks,
        }
    }
}

fn replace_by_identity<T, F>(entries: &mut Vec<T>, entry: T, same_identity: F)
where
    F: Fn(&T, &T) -> bool,
{
    if let Some(position) = entries
        .iter()
        .position(|existing| same_identity(existing, &entry))
    {
        entries[position] = entry;
    } else {
        entries.push(entry);
    }
}

fn remove_by_identity<T, F>(entries: &mut Vec<T>, same_identity: F)
where
    F: Fn(&T) -> bool,
{
    entries.retain(|existing| !same_identity(existing));
}

fn same_port_identity(left: &ResolvedPort, right: &ResolvedPort) -> bool {
    left.protocol == right.protocol
        && left.container == right.container
        && left.host_ip == right.host_ip
}

fn same_publish_port_identity(left: &LayerPublishPort, right: &LayerPublishPort) -> bool {
    left.protocol == right.protocol
        && left.container == right.container
        && left.host_ip == right.host_ip
}

fn merge_optional_port_attributes(
    target: &mut Option<LayerPortAttributes>,
    source: Option<LayerPortAttributes>,
) {
    let Some(source) = source else {
        return;
    };

    match target {
        Some(target) => {
            if source.label.is_some() {
                target.label = source.label;
            }
            if source.on_auto_forward.is_some() {
                target.on_auto_forward = source.on_auto_forward;
            }
            if source.require_local_port.is_some() {
                target.require_local_port = source.require_local_port;
            }
        }
        None => *target = Some(source),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        layer::{LayerHook, canonical_feature_id},
        schema::RawDecuneConfig,
        types::{
            Command, DotfileConflict, GitHttpsMode, GithubCredentialsMode, OnAutoForward,
            SshAgentMode,
        },
    };
    use toml::Value;

    fn raw_config(contents: &str) -> RawDecuneConfig {
        toml::from_str(contents).expect("test config should parse")
    }

    fn raw_layer(contents: &str) -> ConfigLayer {
        ConfigLayer::from_raw_decune(raw_config(contents))
    }

    fn shell_commands(hooks: &[LayerHook]) -> Vec<&str> {
        hooks
            .iter()
            .map(|hook| match &hook.command {
                Command::Shell(command) => command.as_str(),
                Command::Args(_) => panic!("expected shell command"),
            })
            .collect()
    }

    #[test]
    fn empty_input_resolves_documented_defaults() {
        let config = resolve_config(ConfigMergeInput::default());

        assert_eq!(config.shell, None);
        assert!(config.features.is_empty());
        assert!(config.dotfiles.is_empty());
        assert!(config.mounts.is_empty());
        assert!(config.ports.entries.is_empty());
        assert!(config.ports.auto.enabled);
        assert_eq!(config.ports.auto.min, 1024);
        assert_eq!(config.ports.auto.max, 32768);
        assert!(config.ports.auto.ignore.is_empty());
        assert_eq!(config.ports.auto.on_auto_forward, OnAutoForward::Notify);
        assert!(config.credentials.git.enabled);
        assert!(config.credentials.git.copy_user);
        assert!(!config.credentials.git.copy_global_config);
        assert_eq!(config.credentials.git.https, GitHttpsMode::HostHelper);
        assert_eq!(config.credentials.git.ssh_agent, SshAgentMode::Auto);
        assert!(config.credentials.github.enabled);
        assert_eq!(
            config.credentials.github.mode,
            GithubCredentialsMode::GhTokenFile
        );
        assert!(config.credentials.github.install_feature_if_missing);
    }

    #[test]
    fn hooks_follow_documented_layer_order() {
        let config = resolve_config(ConfigMergeInput {
            image_metadata: Some(raw_layer(
                r#"
version = 1

[[hooks.before_initialize]]
command = "image.sh"
"#,
            )),
            global: Some(raw_layer(
                r#"
version = 1

[[hooks.before_initialize]]
command = "global.sh"
"#,
            )),
            devcontainer: Some(raw_layer(
                r#"
version = 1

[[hooks.before_initialize]]
command = "devcontainer.sh"
"#,
            )),
            project: Some(raw_layer(
                r#"
version = 1

[[hooks.before_initialize]]
command = "project.sh"
"#,
            )),
            cli: Some(raw_layer(
                r#"
version = 1

[[hooks.before_initialize]]
command = "cli.sh"
"#,
            )),
        });

        assert_eq!(
            shell_commands(&config.hooks.before_initialize),
            vec![
                "image.sh",
                "global.sh",
                "devcontainer.sh",
                "project.sh",
                "cli.sh"
            ]
        );
    }

    #[test]
    fn project_can_disable_global_feature_by_canonical_id() {
        let config = resolve_config(ConfigMergeInput {
            global: Some(raw_layer(
                r#"
version = 1

[features."ghcr.io/devcontainers/features/github-cli:1"]
version = "latest"
"#,
            )),
            project: Some(raw_layer(
                r#"
version = 1

[features."ghcr.io/devcontainers/features/github-cli:2"]
enabled = false
"#,
            )),
            ..ConfigMergeInput::default()
        });

        assert!(config.features.is_empty());
    }

    #[test]
    fn feature_can_be_readded_after_disable_in_later_layer() {
        let config = resolve_config(ConfigMergeInput {
            global: Some(raw_layer(
                r#"
version = 1

[features."ghcr.io/example/features/tool:1"]
version = "global"
"#,
            )),
            devcontainer: Some(raw_layer(
                r#"
version = 1

[features."ghcr.io/example/features/tool:2"]
enabled = false
"#,
            )),
            project: Some(raw_layer(
                r#"
version = 1

[features."ghcr.io/example/features/tool:3"]
version = "project"
"#,
            )),
            ..ConfigMergeInput::default()
        });

        assert_eq!(config.features.len(), 1);
        assert_eq!(config.features[0].id, "ghcr.io/example/features/tool:3");
        assert_eq!(
            config.features[0].options.get("version"),
            Some(&Value::String("project".to_owned()))
        );
    }

    #[test]
    fn canonical_feature_id_preserves_registry_port() {
        assert_eq!(
            canonical_feature_id("localhost:5000/devcontainers/features/tool:1"),
            "localhost:5000/devcontainers/features/tool"
        );
        assert_eq!(
            canonical_feature_id("localhost:5000/devcontainers/features/tool@sha256:abcd"),
            "localhost:5000/devcontainers/features/tool"
        );
    }

    #[test]
    fn feature_options_merge_by_canonical_id() {
        let config = resolve_config(ConfigMergeInput {
            global: Some(raw_layer(
                r#"
version = 1

[features."ghcr.io/example/features/tool:1"]
version = "1"
channel = "stable"
"#,
            )),
            project: Some(raw_layer(
                r#"
version = 1

[features."ghcr.io/example/features/tool@sha256:abcd"]
version = "2"
"#,
            )),
            ..ConfigMergeInput::default()
        });

        assert_eq!(config.features.len(), 1);
        assert_eq!(
            config.features[0].id,
            "ghcr.io/example/features/tool@sha256:abcd"
        );
        assert_eq!(
            config.features[0].options.get("version"),
            Some(&Value::String("2".to_owned()))
        );
        assert_eq!(
            config.features[0].options.get("channel"),
            Some(&Value::String("stable".to_owned()))
        );
    }

    #[test]
    fn project_mount_replaces_same_target() {
        let config = resolve_config(ConfigMergeInput {
            global: Some(raw_layer(
                r#"
version = 1

[[mounts]]
source = "/global"
target = "/work"
type = "bind"
read_only = true
"#,
            )),
            project: Some(raw_layer(
                r#"
version = 1

[[mounts]]
source = "/project"
target = "/work"
type = "bind"
"#,
            )),
            ..ConfigMergeInput::default()
        });

        assert_eq!(config.mounts.len(), 1);
        assert_eq!(config.mounts[0].source.as_deref(), Some("/project"));
        assert!(!config.mounts[0].read_only);
    }

    #[test]
    fn project_dotfile_replaces_same_target() {
        let config = resolve_config(ConfigMergeInput {
            global: Some(raw_layer(
                r#"
version = 1

[[dotfiles]]
source = "~/.config/nvim"
target = ".config/nvim"
read_only = true
"#,
            )),
            project: Some(raw_layer(
                r#"
version = 1

[[dotfiles]]
source = ".decune/nvim"
target = ".config/nvim"
read_only = false
resolve_symlink = false
on_conflict = "backup"
"#,
            )),
            ..ConfigMergeInput::default()
        });

        assert_eq!(config.dotfiles.len(), 1);
        assert_eq!(config.dotfiles[0].source, ".decune/nvim");
        assert!(!config.dotfiles[0].read_only);
        assert!(!config.dotfiles[0].resolve_symlink);
        assert_eq!(config.dotfiles[0].on_conflict, DotfileConflict::Backup);
    }

    #[test]
    fn dotfile_and_mount_defaults_are_documented() {
        let config = resolve_config(ConfigMergeInput {
            project: Some(raw_layer(
                r#"
version = 1

[[dotfiles]]
source = "~/.config/git"
target = ".config/git"

[[mounts]]
source = "/workspace"
target = "/workspace"
type = "bind"
"#,
            )),
            ..ConfigMergeInput::default()
        });

        assert!(config.dotfiles[0].read_only);
        assert!(config.dotfiles[0].resolve_symlink);
        assert_eq!(config.dotfiles[0].on_conflict, DotfileConflict::Fail);
        assert!(!config.mounts[0].read_only);
        assert!(config.mounts[0].resolve_symlink);
        assert_eq!(config.mounts[0].create, None);
    }

    #[test]
    fn project_can_disable_global_mount_by_target() {
        let config = resolve_config(ConfigMergeInput {
            global: Some(raw_layer(
                r#"
version = 1

[[mounts]]
source = "/global"
target = "/work"
type = "bind"
"#,
            )),
            project: Some(raw_layer(
                r#"
version = 1

[[mounts]]
target = "/work"
enabled = false
"#,
            )),
            ..ConfigMergeInput::default()
        });

        assert!(config.mounts.is_empty());
    }

    #[test]
    fn project_can_disable_global_dotfile_by_target() {
        let config = resolve_config(ConfigMergeInput {
            global: Some(raw_layer(
                r#"
version = 1

[[dotfiles]]
source = "~/.config/nvim"
target = ".config/nvim"
"#,
            )),
            project: Some(raw_layer(
                r#"
version = 1

[[dotfiles]]
target = ".config/nvim"
enabled = false
"#,
            )),
            ..ConfigMergeInput::default()
        });

        assert!(config.dotfiles.is_empty());
    }

    #[test]
    fn project_can_disable_global_port_by_identity() {
        let config = resolve_config(ConfigMergeInput {
            global: Some(raw_layer(
                r#"
version = 1

[[ports]]
container = 3000
host = 3000
label = "global"
"#,
            )),
            project: Some(raw_layer(
                r#"
version = 1

[[ports]]
container = 3000
enabled = false
"#,
            )),
            ..ConfigMergeInput::default()
        });

        assert!(config.ports.entries.is_empty());
    }

    #[test]
    fn disabled_port_identity_ignores_host_port_but_keeps_host_ip_boundary() {
        let config = resolve_config(ConfigMergeInput {
            global: Some(raw_layer(
                r#"
version = 1

[[ports]]
container = 3000
host = 3000
host_ip = "127.0.0.1"
label = "loopback"

[[ports]]
container = 3000
host = 3000
host_ip = "0.0.0.0"
label = "public"
"#,
            )),
            project: Some(raw_layer(
                r#"
version = 1

[[ports]]
container = 3000
host = 13000
host_ip = "127.0.0.1"
enabled = false
"#,
            )),
            ..ConfigMergeInput::default()
        });

        assert_eq!(config.ports.entries.len(), 1);
        assert_eq!(config.ports.entries[0].host_ip, "0.0.0.0");
        assert_eq!(config.ports.entries[0].label.as_deref(), Some("public"));
    }

    #[test]
    fn hooks_append_in_merge_order() {
        let config = resolve_config(ConfigMergeInput {
            global: Some(raw_layer(
                r#"
version = 1

[[hooks.before_post_create]]
command = "global.sh"
"#,
            )),
            project: Some(raw_layer(
                r#"
version = 1

[[hooks.before_post_create]]
command = "project.sh"
"#,
            )),
            ..ConfigMergeInput::default()
        });

        assert_eq!(
            config.hooks.before_post_create,
            vec![
                LayerHook {
                    command: Command::Shell("global.sh".to_owned()),
                    location: None,
                    user: None,
                    shell: true,
                    workdir: None,
                },
                LayerHook {
                    command: Command::Shell("project.sh".to_owned()),
                    location: None,
                    user: None,
                    shell: true,
                    workdir: None,
                },
            ]
        );
    }

    #[test]
    fn hook_shell_defaults_follow_command_form() {
        let config = resolve_config(ConfigMergeInput {
            project: Some(raw_layer(
                r#"
version = 1

[[hooks.before_post_create]]
command = "shell-form.sh"

[[hooks.before_post_create]]
command = ["bash", "args-form.sh"]
"#,
            )),
            ..ConfigMergeInput::default()
        });

        assert!(config.hooks.before_post_create[0].shell);
        assert!(!config.hooks.before_post_create[1].shell);
    }

    #[test]
    fn explicit_hook_shell_overrides_default() {
        let config = resolve_config(ConfigMergeInput {
            project: Some(raw_layer(
                r#"
version = 1

[[hooks.before_post_create]]
command = "shell-form.sh"
shell = false

[[hooks.before_post_create]]
command = ["bash", "args-form.sh"]
shell = true
"#,
            )),
            ..ConfigMergeInput::default()
        });

        assert!(!config.hooks.before_post_create[0].shell);
        assert!(config.hooks.before_post_create[1].shell);
    }

    #[test]
    fn scalar_and_cli_layer_win_by_precedence() {
        let config = resolve_config(ConfigMergeInput {
            global: Some(raw_layer("version = 1\nshell = '/bin/bash'\n")),
            project: Some(raw_layer("version = 1\nshell = '/bin/zsh'\n")),
            cli: Some(ConfigLayer {
                shell: Some("/usr/bin/fish".to_owned()),
                ..ConfigLayer::default()
            }),
            ..ConfigMergeInput::default()
        });

        assert_eq!(config.shell.as_deref(), Some("/usr/bin/fish"));
    }

    #[test]
    fn scalar_without_cli_follows_documented_layer_precedence() {
        let config = resolve_config(ConfigMergeInput {
            image_metadata: Some(raw_layer("version = 1\nshell = '/bin/image'\n")),
            global: Some(raw_layer("version = 1\nshell = '/bin/global'\n")),
            devcontainer: Some(raw_layer("version = 1\nshell = '/bin/devcontainer'\n")),
            project: Some(raw_layer("version = 1\nshell = '/bin/project'\n")),
            ..ConfigMergeInput::default()
        });

        assert_eq!(config.shell.as_deref(), Some("/bin/project"));
    }

    #[test]
    fn credentials_merge_fieldwise_by_precedence() {
        let config = resolve_config(ConfigMergeInput {
            global: Some(raw_layer(
                r#"
version = 1

[credentials.git]
enabled = false
copy_user = false
copy_global_config = true
https = "off"
ssh_agent = "required"

[credentials.github]
enabled = false
mode = "off"
install_feature_if_missing = false
"#,
            )),
            project: Some(raw_layer(
                r#"
version = 1

[credentials.git]
https = "host-helper"

[credentials.github]
enabled = true
"#,
            )),
            ..ConfigMergeInput::default()
        });

        assert!(!config.credentials.git.enabled);
        assert!(!config.credentials.git.copy_user);
        assert!(config.credentials.git.copy_global_config);
        assert_eq!(config.credentials.git.https, GitHttpsMode::HostHelper);
        assert_eq!(config.credentials.git.ssh_agent, SshAgentMode::Required);
        assert!(config.credentials.github.enabled);
        assert_eq!(config.credentials.github.mode, GithubCredentialsMode::Off);
        assert!(!config.credentials.github.install_feature_if_missing);
    }

    #[test]
    fn port_identity_replaces_protocol_container_and_host_ip() {
        let config = resolve_config(ConfigMergeInput {
            global: Some(raw_layer(
                r#"
version = 1

[[ports]]
container = 3000
host = 3000
host_ip = "127.0.0.1"
label = "global"
"#,
            )),
            project: Some(raw_layer(
                r#"
version = 1

[[ports]]
container = 3000
host = 13000
host_ip = "127.0.0.1"
label = "project"

[[ports]]
container = 3000
host = 23000
host_ip = "0.0.0.0"
label = "public"
"#,
            )),
            ..ConfigMergeInput::default()
        });

        assert_eq!(config.ports.entries.len(), 2);
        assert_eq!(config.ports.entries[0].host, Some(13000));
        assert_eq!(config.ports.entries[0].label.as_deref(), Some("project"));
        assert_eq!(config.ports.entries[1].host, Some(23000));
    }

    #[test]
    fn auto_ports_ignore_is_preserved_when_upper_layer_omits_it() {
        let config = resolve_config(ConfigMergeInput {
            global: Some(raw_layer(
                r#"
version = 1

[ports.auto]
ignore = [22]
"#,
            )),
            project: Some(raw_layer(
                r#"
version = 1

[ports.auto]
enabled = false
"#,
            )),
            ..ConfigMergeInput::default()
        });

        assert!(!config.ports.auto.enabled);
        assert_eq!(config.ports.auto.ignore, vec![22]);
    }

    #[test]
    fn open_browser_on_auto_forward_resolves_to_notify() {
        let config = resolve_config(ConfigMergeInput {
            project: Some(raw_layer(
                r#"
version = 1

[ports.auto]
on_auto_forward = "openBrowser"
"#,
            )),
            ..ConfigMergeInput::default()
        });

        assert_eq!(config.ports.auto.on_auto_forward, OnAutoForward::Notify);
    }
}
