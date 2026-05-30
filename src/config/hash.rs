#![allow(dead_code)]

use std::collections::BTreeMap;

use toml::Value;

use crate::config::{
    canonical::{CanonicalWriter, sha256_hex},
    resolved::{
        ResolvedConfig, ResolvedDevcontainer, ResolvedDevcontainerMount,
        ResolvedDevcontainerSource, ResolvedHook, ResolvedPublishPort, ResolvedRunArg,
        ResolvedUserEnvProbe,
    },
    types::{
        Command, DotfileConflict, GitHttpsMode, GithubCredentialsMode, HookLocation, MountCreate,
        MountType, PortProtocol, SshAgentMode,
    },
};
use crate::devcontainer::lifecycle::{
    LifecycleCommand, LifecycleDefinition, LifecycleStage, WaitFor,
};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ConfigHashInput<'a> {
    pub(crate) config: &'a ResolvedConfig,
    pub(crate) feature_locks: Vec<FeatureLockHashEntry>,
    pub(crate) cli_flags: BTreeMap<String, Value>,
    pub(crate) build: Option<BuildHashInput>,
    pub(crate) resolved_mounts: Vec<MountHashInput>,
}

impl<'a> ConfigHashInput<'a> {
    pub(crate) fn new(config: &'a ResolvedConfig) -> Self {
        Self {
            config,
            feature_locks: Vec::new(),
            cli_flags: BTreeMap::new(),
            build: None,
            resolved_mounts: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MountHashInput {
    pub(crate) source: Option<String>,
    pub(crate) target: String,
    pub(crate) mount_type: MountType,
    pub(crate) read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FeatureLockHashEntry {
    pub(crate) feature_id: String,
    pub(crate) digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct BuildHashInput {
    pub(crate) dockerfile_path: Option<String>,
    pub(crate) dockerfile_content_hash: Option<String>,
    pub(crate) context_path: Option<String>,
    pub(crate) dockerignore_content_hash: Option<String>,
}

pub(crate) fn config_hash(input: &ConfigHashInput<'_>) -> String {
    let mut writer = CanonicalWriter::default();

    writer.field("version", |writer| writer.string("decune-config-hash-v1"));
    writer.field("resolved_config", |writer| {
        write_resolved_config(writer, input.config);
    });
    writer.field("feature_locks", |writer| {
        write_feature_locks(writer, &input.feature_locks);
    });
    writer.field("cli_flags", |writer| {
        writer.map(input.cli_flags.iter(), |writer, value| {
            writer.toml_value(value);
        });
    });
    writer.field("build", |writer| match &input.build {
        Some(build) => write_build_input(writer, build),
        None => writer.none(),
    });
    writer.field("resolved_mounts", |writer| {
        write_resolved_mounts(writer, &input.resolved_mounts);
    });

    sha256_hex(writer.finish().as_bytes())
}

fn write_resolved_mounts(writer: &mut CanonicalWriter, mounts: &[MountHashInput]) {
    writer.seq(mounts.iter(), |writer, mount| {
        writer.object("ResolvedMountSpec", |writer| {
            writer.field("source", |writer| {
                writer.option_string(mount.source.as_deref());
            });
            writer.field("target", |writer| writer.string(&mount.target));
            writer.field("type", |writer| {
                writer.string(mount_type_name(mount.mount_type));
            });
            writer.field("read_only", |writer| writer.bool(mount.read_only));
        });
    });
}

fn write_resolved_config(writer: &mut CanonicalWriter, config: &ResolvedConfig) {
    writer.object("ResolvedConfig", |writer| {
        writer.field("shell", |writer| {
            writer.option_string(config.shell.as_deref())
        });
        writer.field("features", |writer| {
            writer.seq(config.features.iter(), |writer, feature| {
                writer.object("Feature", |writer| {
                    writer.field("id", |writer| writer.string(&feature.id));
                    writer.field("canonical_id", |writer| {
                        writer.string(&feature.canonical_id);
                    });
                    writer.field("options", |writer| {
                        writer.map(feature.options.iter(), |writer, value| {
                            writer.toml_value(value);
                        });
                    });
                });
            });
        });
        writer.field("dotfiles", |writer| {
            writer.seq(config.dotfiles.iter(), |writer, dotfile| {
                writer.object("Dotfile", |writer| {
                    writer.field("source", |writer| writer.string(&dotfile.source));
                    writer.field("target", |writer| writer.string(&dotfile.target));
                    writer.field("read_only", |writer| writer.bool(dotfile.read_only));
                    writer.field("resolve_symlink", |writer| {
                        writer.bool(dotfile.resolve_symlink);
                    });
                    writer.field("on_conflict", |writer| {
                        writer.string(dotfile_conflict_name(dotfile.on_conflict));
                    });
                });
            });
        });
        writer.field("mounts", |writer| {
            writer.seq(config.mounts.iter(), |writer, mount| {
                writer.object("Mount", |writer| {
                    writer.field("source", |writer| {
                        writer.option_string(mount.source.as_deref());
                    });
                    writer.field("target", |writer| writer.string(&mount.target));
                    writer.field("type", |writer| {
                        writer.string(mount_type_name(mount.mount_type));
                    });
                    writer.field("read_only", |writer| writer.bool(mount.read_only));
                    writer.field("resolve_symlink", |writer| {
                        writer.bool(mount.resolve_symlink);
                    });
                    writer.field("create", |writer| match mount.create {
                        Some(create) => writer.string(mount_create_name(create)),
                        None => writer.none(),
                    });
                });
            });
        });
        // forwarding は up 実行時の runtime 設定であり，container/image の再作成条件ではない．
        let _ = &config.ports;
        writer.field("devcontainer", |writer| {
            write_devcontainer(writer, &config.devcontainer);
        });
        writer.field("credentials", |writer| {
            writer.object("Credentials", |writer| {
                writer.field("git", |writer| {
                    writer.object("GitCredentials", |writer| {
                        writer.field("enabled", |writer| {
                            writer.bool(config.credentials.git.enabled);
                        });
                        writer.field("copy_user", |writer| {
                            writer.bool(config.credentials.git.copy_user);
                        });
                        writer.field("copy_global_config", |writer| {
                            writer.bool(config.credentials.git.copy_global_config);
                        });
                        writer.field("https", |writer| {
                            writer.string(git_https_mode_name(config.credentials.git.https));
                        });
                        writer.field("ssh_agent", |writer| {
                            writer.string(ssh_agent_mode_name(config.credentials.git.ssh_agent));
                        });
                    });
                });
                writer.field("github", |writer| {
                    writer.object("GithubCredentials", |writer| {
                        writer.field("enabled", |writer| {
                            writer.bool(config.credentials.github.enabled);
                        });
                        writer.field("mode", |writer| {
                            writer.string(github_credentials_mode_name(
                                config.credentials.github.mode,
                            ));
                        });
                        writer.field("install_feature_if_missing", |writer| {
                            writer.bool(config.credentials.github.install_feature_if_missing);
                        });
                    });
                });
            });
        });
        writer.field("hooks", |writer| {
            writer.object("Hooks", |writer| {
                writer.field("before_initialize", |writer| {
                    write_hooks(writer, &config.hooks.before_initialize);
                });
                writer.field("after_initialize", |writer| {
                    write_hooks(writer, &config.hooks.after_initialize);
                });
                writer.field("before_on_create", |writer| {
                    write_hooks(writer, &config.hooks.before_on_create);
                });
                writer.field("after_on_create", |writer| {
                    write_hooks(writer, &config.hooks.after_on_create);
                });
                writer.field("before_update_content", |writer| {
                    write_hooks(writer, &config.hooks.before_update_content);
                });
                writer.field("after_update_content", |writer| {
                    write_hooks(writer, &config.hooks.after_update_content);
                });
                writer.field("before_post_create", |writer| {
                    write_hooks(writer, &config.hooks.before_post_create);
                });
                writer.field("after_post_create", |writer| {
                    write_hooks(writer, &config.hooks.after_post_create);
                });
                writer.field("before_post_start", |writer| {
                    write_hooks(writer, &config.hooks.before_post_start);
                });
                writer.field("after_post_start", |writer| {
                    write_hooks(writer, &config.hooks.after_post_start);
                });
                writer.field("before_post_attach", |writer| {
                    write_hooks(writer, &config.hooks.before_post_attach);
                });
                writer.field("after_post_attach", |writer| {
                    write_hooks(writer, &config.hooks.after_post_attach);
                });
            });
        });
    });
}

fn write_devcontainer(writer: &mut CanonicalWriter, devcontainer: &ResolvedDevcontainer) {
    writer.object("Devcontainer", |writer| {
        writer.field("source", |writer| match &devcontainer.source {
            Some(source) => write_devcontainer_source(writer, source),
            None => writer.none(),
        });
        writer.field("override_feature_install_order", |writer| {
            writer.seq(
                devcontainer.override_feature_install_order.iter(),
                |writer, feature| writer.string(feature),
            );
        });
        writer.field("mounts", |writer| {
            writer.seq(devcontainer.mounts.iter(), |writer, mount| {
                write_devcontainer_mount(writer, mount);
            });
        });
        writer.field("workspace_mount", |writer| {
            writer.option_string(devcontainer.workspace_mount.as_deref());
        });
        writer.field("workspace_folder", |writer| {
            writer.option_string(devcontainer.workspace_folder.as_deref());
        });
        writer.field("container_env", |writer| {
            writer.map(devcontainer.container_env.iter(), |writer, value| {
                writer.string(value);
            });
        });
        writer.field("remote_env", |writer| {
            writer.map(devcontainer.remote_env.iter(), |writer, value| {
                writer.string(value);
            });
        });
        writer.field("remote_user", |writer| {
            writer.option_string(devcontainer.remote_user.as_deref());
        });
        writer.field("container_user", |writer| {
            writer.option_string(devcontainer.container_user.as_deref());
        });
        writer.field("update_remote_user_uid", |writer| {
            writer.bool(devcontainer.update_remote_user_uid);
        });
        writer.field("override_command", |writer| {
            writer.bool(devcontainer.override_command);
        });
        writer.field("user_env_probe", |writer| {
            match devcontainer.user_env_probe {
                Some(value) => writer.string(user_env_probe_name(value)),
                None => writer.none(),
            }
        });
        writer.field("publish_ports", |writer| {
            writer.seq(devcontainer.publish_ports.iter(), write_publish_port);
        });
        writer.field("run_args", |writer| {
            writer.seq(devcontainer.run_args.iter(), write_run_arg);
        });
        writer.field("init", |writer| writer.bool(devcontainer.init));
        writer.field("privileged", |writer| writer.bool(devcontainer.privileged));
        writer.field("cap_add", |writer| {
            writer.seq(devcontainer.cap_add.iter(), |writer, capability| {
                writer.string(capability);
            });
        });
        writer.field("security_opt", |writer| {
            writer.seq(devcontainer.security_opt.iter(), |writer, option| {
                writer.string(option);
            });
        });
        writer.field("lifecycle", |writer| match &devcontainer.lifecycle {
            Some(lifecycle) => write_lifecycle(writer, lifecycle),
            None => writer.none(),
        });
    });
}

fn write_devcontainer_mount(writer: &mut CanonicalWriter, mount: &ResolvedDevcontainerMount) {
    match mount {
        ResolvedDevcontainerMount::String(value) => {
            writer.object("DevcontainerMountString", |writer| {
                writer.field("value", |writer| writer.string(value));
            });
        }
        ResolvedDevcontainerMount::Object(values) => {
            writer.object("DevcontainerMountObject", |writer| {
                writer.field("value", |writer| {
                    writer.map(values.iter(), |writer, value| writer.json_value(value));
                });
            });
        }
    }
}

fn write_devcontainer_source(writer: &mut CanonicalWriter, source: &ResolvedDevcontainerSource) {
    match source {
        ResolvedDevcontainerSource::Image(image) => {
            writer.object("ImageSource", |writer| {
                writer.field("image", |writer| writer.string(image));
            });
        }
        ResolvedDevcontainerSource::Dockerfile(build) => {
            writer.object("DockerfileSource", |writer| {
                writer.field("dockerfile", |writer| writer.string(&build.dockerfile));
                writer.field("context", |writer| {
                    writer.option_string(build.context.as_deref());
                });
                writer.field("args", |writer| {
                    writer.map(build.args.iter(), |writer, value| writer.string(value));
                });
                writer.field("target", |writer| {
                    writer.option_string(build.target.as_deref());
                });
                writer.field("cache_from", |writer| {
                    writer.seq(build.cache_from.iter(), |writer, entry| {
                        writer.string(entry)
                    });
                });
            });
        }
    }
}

fn write_publish_port(writer: &mut CanonicalWriter, port: &ResolvedPublishPort) {
    writer.object("PublishPort", |writer| {
        writer.field("container", |writer| {
            writer.string(&port.container.to_string());
        });
        writer.field("host", |writer| match port.host {
            Some(host) => writer.string(&host.to_string()),
            None => writer.none(),
        });
        writer.field("host_ip", |writer| {
            writer.option_string(port.host_ip.as_deref())
        });
        writer.field("protocol", |writer| {
            writer.string(port_protocol_name(port.protocol));
        });
    });
}

fn write_run_arg(writer: &mut CanonicalWriter, run_arg: &ResolvedRunArg) {
    match run_arg {
        ResolvedRunArg::AddHost(value) => {
            writer.object("AddHost", |writer| {
                writer.field("value", |writer| writer.string(value))
            });
        }
        ResolvedRunArg::Dns(value) => {
            writer.object("Dns", |writer| {
                writer.field("value", |writer| writer.string(value))
            });
        }
        ResolvedRunArg::DnsSearch(value) => {
            writer.object("DnsSearch", |writer| {
                writer.field("value", |writer| writer.string(value));
            });
        }
    }
}

fn write_lifecycle(writer: &mut CanonicalWriter, lifecycle: &LifecycleDefinition) {
    writer.object("Lifecycle", |writer| {
        for stage in [
            LifecycleStage::Initialize,
            LifecycleStage::OnCreate,
            LifecycleStage::UpdateContent,
            LifecycleStage::PostCreate,
            LifecycleStage::PostStart,
            LifecycleStage::PostAttach,
        ] {
            writer.field(lifecycle_stage_name(stage), |writer| {
                match lifecycle.command(stage) {
                    Some(command) => write_lifecycle_command(writer, command),
                    None => writer.none(),
                }
            });
        }
        writer.field("wait_for", |writer| {
            writer.string(wait_for_name(lifecycle.wait_for()));
        });
    });
}

fn write_lifecycle_command(writer: &mut CanonicalWriter, command: &LifecycleCommand) {
    match command {
        LifecycleCommand::Shell(command) => {
            writer.object("LifecycleShell", |writer| {
                writer.field("value", |writer| writer.string(command));
            });
        }
        LifecycleCommand::Args(args) => {
            writer.object("LifecycleArgs", |writer| {
                writer.field("value", |writer| {
                    writer.seq(args.iter(), |writer, arg| writer.string(arg));
                });
            });
        }
        LifecycleCommand::Parallel(commands) => {
            writer.object("LifecycleParallel", |writer| {
                writer.field("value", |writer| {
                    writer.map(commands.iter(), |writer, command| {
                        write_lifecycle_command(writer, command);
                    });
                });
            });
        }
    }
}

fn write_hooks(writer: &mut CanonicalWriter, hooks: &[ResolvedHook]) {
    writer.seq(hooks.iter(), |writer, hook| {
        writer.object("Hook", |writer| {
            writer.field("command", |writer| write_command(writer, &hook.command));
            writer.field("location", |writer| match hook.location {
                Some(location) => writer.string(hook_location_name(location)),
                None => writer.none(),
            });
            writer.field("user", |writer| writer.option_string(hook.user.as_deref()));
            writer.field("shell", |writer| writer.bool(hook.shell));
            writer.field("workdir", |writer| {
                writer.option_string(hook.workdir.as_deref())
            });
        });
    });
}

fn write_command(writer: &mut CanonicalWriter, command: &Command) {
    match command {
        Command::Shell(command) => {
            writer.object("ShellCommand", |writer| {
                writer.field("value", |writer| writer.string(command));
            });
        }
        Command::Args(args) => {
            writer.object("ArgsCommand", |writer| {
                writer.field("value", |writer| {
                    writer.seq(args.iter(), |writer, arg| writer.string(arg));
                });
            });
        }
    }
}

fn write_feature_locks(writer: &mut CanonicalWriter, entries: &[FeatureLockHashEntry]) {
    let mut sorted = entries.iter().collect::<Vec<_>>();
    sorted.sort_by(|left, right| {
        left.feature_id
            .cmp(&right.feature_id)
            .then_with(|| left.digest.cmp(&right.digest))
    });

    writer.seq(sorted, |writer, entry| {
        writer.object("FeatureLock", |writer| {
            writer.field("feature_id", |writer| writer.string(&entry.feature_id));
            writer.field("digest", |writer| writer.string(&entry.digest));
        });
    });
}

fn write_build_input(writer: &mut CanonicalWriter, build: &BuildHashInput) {
    writer.object("Build", |writer| {
        writer.field("dockerfile_path", |writer| {
            writer.option_string(build.dockerfile_path.as_deref());
        });
        writer.field("dockerfile_content_hash", |writer| {
            writer.option_string(build.dockerfile_content_hash.as_deref());
        });
        writer.field("context_path", |writer| {
            writer.option_string(build.context_path.as_deref());
        });
        writer.field("dockerignore_content_hash", |writer| {
            writer.option_string(build.dockerignore_content_hash.as_deref());
        });
    });
}

fn dotfile_conflict_name(value: DotfileConflict) -> &'static str {
    match value {
        DotfileConflict::Fail => "fail",
        DotfileConflict::ReplaceSymlink => "replace-symlink",
        DotfileConflict::Backup => "backup",
    }
}

fn mount_type_name(value: MountType) -> &'static str {
    match value {
        MountType::Bind => "bind",
        MountType::Volume => "volume",
        MountType::Tmpfs => "tmpfs",
    }
}

fn mount_create_name(value: MountCreate) -> &'static str {
    match value {
        MountCreate::Directory => "directory",
    }
}

fn write_option_bool(writer: &mut CanonicalWriter, value: Option<bool>) {
    match value {
        Some(value) => writer.bool(value),
        None => writer.none(),
    }
}

fn user_env_probe_name(value: ResolvedUserEnvProbe) -> &'static str {
    match value {
        ResolvedUserEnvProbe::None => "none",
        ResolvedUserEnvProbe::LoginShell => "loginShell",
        ResolvedUserEnvProbe::InteractiveShell => "interactiveShell",
        ResolvedUserEnvProbe::LoginInteractiveShell => "loginInteractiveShell",
    }
}

fn port_protocol_name(value: PortProtocol) -> &'static str {
    match value {
        PortProtocol::Tcp => "tcp",
    }
}

fn lifecycle_stage_name(value: LifecycleStage) -> &'static str {
    match value {
        LifecycleStage::Initialize => "initializeCommand",
        LifecycleStage::OnCreate => "onCreateCommand",
        LifecycleStage::UpdateContent => "updateContentCommand",
        LifecycleStage::PostCreate => "postCreateCommand",
        LifecycleStage::PostStart => "postStartCommand",
        LifecycleStage::PostAttach => "postAttachCommand",
    }
}

fn wait_for_name(value: WaitFor) -> &'static str {
    match value {
        WaitFor::Initialize => "initializeCommand",
        WaitFor::OnCreate => "onCreateCommand",
        WaitFor::UpdateContent => "updateContentCommand",
        WaitFor::PostCreate => "postCreateCommand",
        WaitFor::PostStart => "postStartCommand",
    }
}

fn git_https_mode_name(value: GitHttpsMode) -> &'static str {
    match value {
        GitHttpsMode::Off => "off",
        GitHttpsMode::HostHelper => "host-helper",
    }
}

fn ssh_agent_mode_name(value: SshAgentMode) -> &'static str {
    match value {
        SshAgentMode::Off => "off",
        SshAgentMode::Auto => "auto",
        SshAgentMode::Required => "required",
    }
}

fn github_credentials_mode_name(value: GithubCredentialsMode) -> &'static str {
    match value {
        GithubCredentialsMode::Off => "off",
        GithubCredentialsMode::GhTokenFile => "gh-token-file",
    }
}

fn hook_location_name(value: HookLocation) -> &'static str {
    match value {
        HookLocation::Host => "host",
        HookLocation::Container => "container",
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::config::{
        layer::{
            LayerDevcontainerMetadata, LayerDevcontainerMount, LayerPortAttributes,
            LayerPublishPort,
        },
        merge::{ConfigLayer, ConfigMergeInput, resolve_config},
        types::{OnAutoForward, PortProtocol},
    };

    fn resolved_config(contents: &str) -> ResolvedConfig {
        let raw = toml::from_str(contents).expect("test config should parse");
        resolve_config(ConfigMergeInput {
            project: Some(ConfigLayer::from_raw_decune(raw)),
            ..ConfigMergeInput::default()
        })
    }

    fn hash_for(config: &ResolvedConfig) -> String {
        config_hash(&ConfigHashInput::new(config))
    }

    #[test]
    fn same_config_produces_same_hash() {
        let config = resolved_config(
            r#"
version = 1
shell = "/bin/zsh"
"#,
        );

        assert_eq!(hash_for(&config), hash_for(&config));
        assert_eq!(hash_for(&config).len(), 64);
    }

    #[test]
    fn scalar_change_changes_hash() {
        let bash = resolved_config(
            r#"
version = 1
shell = "/bin/bash"
"#,
        );
        let zsh = resolved_config(
            r#"
version = 1
shell = "/bin/zsh"
"#,
        );

        assert_ne!(hash_for(&bash), hash_for(&zsh));
    }

    #[test]
    fn open_browser_auto_forward_hashes_as_notify() {
        let notify = resolved_config(
            r#"
version = 1

[ports.auto]
on_auto_forward = "notify"
"#,
        );
        let open_browser = resolved_config(
            r#"
version = 1

[ports.auto]
on_auto_forward = "openBrowser"
"#,
        );

        assert_eq!(
            open_browser.ports.auto.on_auto_forward,
            OnAutoForward::Notify
        );
        assert_eq!(hash_for(&notify), hash_for(&open_browser));
    }

    #[test]
    fn manual_forwarding_ports_do_not_change_hash() {
        let no_ports = resolved_config("version = 1\n");
        let with_port = resolved_config(
            r#"
version = 1

[[ports]]
container = 3000
host = 3000
label = "web"
"#,
        );

        assert_eq!(hash_for(&no_ports), hash_for(&with_port));
    }

    #[test]
    fn auto_forwarding_settings_do_not_change_hash() {
        let default_auto = resolved_config("version = 1\n");
        let custom_auto = resolved_config(
            r#"
version = 1

[ports.auto]
enabled = false
min = 2000
max = 9000
ignore = [3000, 3001]
on_auto_forward = "silent"
"#,
        );

        assert_eq!(hash_for(&default_auto), hash_for(&custom_auto));
    }

    #[test]
    fn devcontainer_publish_ports_change_hash() {
        let without_publish = resolve_config(ConfigMergeInput::default());
        let with_publish = resolve_config(ConfigMergeInput {
            devcontainer: Some(ConfigLayer {
                devcontainer: Some(LayerDevcontainerMetadata {
                    publish_ports: vec![LayerPublishPort {
                        container: 8080,
                        host: Some(8080),
                        host_ip: Some("127.0.0.1".to_owned()),
                        protocol: PortProtocol::Tcp,
                    }],
                    ..LayerDevcontainerMetadata::default()
                }),
                ..ConfigLayer::default()
            }),
            ..ConfigMergeInput::default()
        });

        assert_ne!(hash_for(&without_publish), hash_for(&with_publish));
    }

    #[test]
    fn devcontainer_port_attributes_do_not_change_hash() {
        let without_attributes = resolve_config(ConfigMergeInput::default());
        let with_attributes = resolve_config(ConfigMergeInput {
            devcontainer: Some(ConfigLayer {
                devcontainer: Some(LayerDevcontainerMetadata {
                    port_attributes: BTreeMap::from([(
                        "3000".to_owned(),
                        LayerPortAttributes {
                            label: Some("web".to_owned()),
                            on_auto_forward: Some(OnAutoForward::Silent),
                            require_local_port: Some(true),
                        },
                    )]),
                    other_ports_attributes: Some(LayerPortAttributes {
                        label: Some("other".to_owned()),
                        on_auto_forward: Some(OnAutoForward::Ignore),
                        require_local_port: Some(false),
                    }),
                    ..LayerDevcontainerMetadata::default()
                }),
                ..ConfigLayer::default()
            }),
            ..ConfigMergeInput::default()
        });

        assert_eq!(hash_for(&without_attributes), hash_for(&with_attributes));
    }

    #[test]
    fn devcontainer_object_mount_hash_is_stable_by_key_order() {
        let first = resolve_config(ConfigMergeInput {
            devcontainer: Some(ConfigLayer {
                devcontainer: Some(LayerDevcontainerMetadata {
                    mounts: vec![LayerDevcontainerMount::Object(
                        [
                            ("type".to_owned(), serde_json::json!("bind")),
                            ("source".to_owned(), serde_json::json!("/host")),
                            ("target".to_owned(), serde_json::json!("/container")),
                        ]
                        .into(),
                    )],
                    ..LayerDevcontainerMetadata::default()
                }),
                ..ConfigLayer::default()
            }),
            ..ConfigMergeInput::default()
        });
        let second = resolve_config(ConfigMergeInput {
            devcontainer: Some(ConfigLayer {
                devcontainer: Some(LayerDevcontainerMetadata {
                    mounts: vec![LayerDevcontainerMount::Object(
                        [
                            ("target".to_owned(), serde_json::json!("/container")),
                            ("type".to_owned(), serde_json::json!("bind")),
                            ("source".to_owned(), serde_json::json!("/host")),
                        ]
                        .into(),
                    )],
                    ..LayerDevcontainerMetadata::default()
                }),
                ..ConfigLayer::default()
            }),
            ..ConfigMergeInput::default()
        });

        assert_eq!(hash_for(&first), hash_for(&second));
    }

    #[test]
    fn feature_option_change_changes_hash() {
        let stable = resolved_config(
            r#"
version = 1

[features."ghcr.io/example/features/tool:1"]
version = "stable"
"#,
        );
        let nightly = resolved_config(
            r#"
version = 1

[features."ghcr.io/example/features/tool:1"]
version = "nightly"
"#,
        );

        assert_ne!(hash_for(&stable), hash_for(&nightly));
    }

    #[test]
    fn feature_option_key_order_does_not_change_hash() {
        let first = resolved_config(
            r#"
version = 1

[features."ghcr.io/example/features/tool:1"]
version = "1"
channel = "stable"
"#,
        );
        let second = resolved_config(
            r#"
version = 1

[features."ghcr.io/example/features/tool:1"]
channel = "stable"
version = "1"
"#,
        );

        assert_eq!(hash_for(&first), hash_for(&second));
    }

    #[test]
    fn hook_shell_default_hashes_like_explicit_default() {
        let implicit = resolved_config(
            r#"
version = 1

[[hooks.before_post_create]]
command = "scripts/setup.sh"
"#,
        );
        let explicit = resolved_config(
            r#"
version = 1

[[hooks.before_post_create]]
command = "scripts/setup.sh"
shell = true
"#,
        );

        assert_eq!(hash_for(&implicit), hash_for(&explicit));
    }

    #[test]
    fn hook_args_shell_default_hashes_like_explicit_default() {
        let implicit = resolved_config(
            r#"
version = 1

[[hooks.before_post_create]]
command = ["bash", "scripts/setup.sh"]
"#,
        );
        let explicit = resolved_config(
            r#"
version = 1

[[hooks.before_post_create]]
command = ["bash", "scripts/setup.sh"]
shell = false
"#,
        );

        assert_eq!(hash_for(&implicit), hash_for(&explicit));
    }

    #[test]
    fn dockerfile_content_hash_changes_hash() {
        let config = resolved_config("version = 1\n");
        let first = config_hash(&ConfigHashInput {
            build: Some(BuildHashInput {
                dockerfile_path: Some(".devcontainer/Dockerfile".to_owned()),
                dockerfile_content_hash: Some("sha256:first".to_owned()),
                ..BuildHashInput::default()
            }),
            ..ConfigHashInput::new(&config)
        });
        let second = config_hash(&ConfigHashInput {
            build: Some(BuildHashInput {
                dockerfile_path: Some(".devcontainer/Dockerfile".to_owned()),
                dockerfile_content_hash: Some("sha256:second".to_owned()),
                ..BuildHashInput::default()
            }),
            ..ConfigHashInput::new(&config)
        });

        assert_ne!(first, second);
    }

    #[test]
    fn build_context_and_dockerignore_change_hash() {
        let config = resolved_config("version = 1\n");
        let first = config_hash(&ConfigHashInput {
            build: Some(BuildHashInput {
                context_path: Some(".devcontainer".to_owned()),
                dockerignore_content_hash: Some("sha256:first".to_owned()),
                ..BuildHashInput::default()
            }),
            ..ConfigHashInput::new(&config)
        });
        let second = config_hash(&ConfigHashInput {
            build: Some(BuildHashInput {
                context_path: Some(".".to_owned()),
                dockerignore_content_hash: Some("sha256:second".to_owned()),
                ..BuildHashInput::default()
            }),
            ..ConfigHashInput::new(&config)
        });

        assert_ne!(first, second);
    }

    #[test]
    fn dockerignore_content_hash_alone_changes_hash() {
        let config = resolved_config("version = 1\n");
        let first = config_hash(&ConfigHashInput {
            build: Some(BuildHashInput {
                dockerignore_content_hash: Some("sha256:first".to_owned()),
                ..BuildHashInput::default()
            }),
            ..ConfigHashInput::new(&config)
        });
        let second = config_hash(&ConfigHashInput {
            build: Some(BuildHashInput {
                dockerignore_content_hash: Some("sha256:second".to_owned()),
                ..BuildHashInput::default()
            }),
            ..ConfigHashInput::new(&config)
        });

        assert_ne!(first, second);
    }

    #[test]
    fn dockerfile_path_change_changes_hash() {
        let config = resolved_config("version = 1\n");
        let first = config_hash(&ConfigHashInput {
            build: Some(BuildHashInput {
                dockerfile_path: Some(".devcontainer/Dockerfile".to_owned()),
                ..BuildHashInput::default()
            }),
            ..ConfigHashInput::new(&config)
        });
        let second = config_hash(&ConfigHashInput {
            build: Some(BuildHashInput {
                dockerfile_path: Some("Dockerfile".to_owned()),
                ..BuildHashInput::default()
            }),
            ..ConfigHashInput::new(&config)
        });

        assert_ne!(first, second);
    }

    #[test]
    fn cli_flag_change_changes_hash() {
        let config = resolved_config("version = 1\n");
        let without_flag = config_hash(&ConfigHashInput::new(&config));
        let with_flag = config_hash(&ConfigHashInput {
            cli_flags: BTreeMap::from([("no_cache".to_owned(), Value::Boolean(true))]),
            ..ConfigHashInput::new(&config)
        });

        assert_ne!(without_flag, with_flag);
    }

    #[test]
    fn cli_flag_key_order_does_not_change_hash() {
        let config = resolved_config("version = 1\n");
        let first = config_hash(&ConfigHashInput {
            cli_flags: BTreeMap::from([
                ("pull".to_owned(), Value::Boolean(true)),
                ("rebuild".to_owned(), Value::Boolean(false)),
            ]),
            ..ConfigHashInput::new(&config)
        });
        let second = config_hash(&ConfigHashInput {
            cli_flags: BTreeMap::from([
                ("rebuild".to_owned(), Value::Boolean(false)),
                ("pull".to_owned(), Value::Boolean(true)),
            ]),
            ..ConfigHashInput::new(&config)
        });

        assert_eq!(first, second);
    }

    #[test]
    fn feature_lock_order_does_not_change_hash() {
        let config = resolved_config("version = 1\n");
        let first = config_hash(&ConfigHashInput {
            feature_locks: vec![
                FeatureLockHashEntry {
                    feature_id: "feature-b".to_owned(),
                    digest: "sha256:b".to_owned(),
                },
                FeatureLockHashEntry {
                    feature_id: "feature-a".to_owned(),
                    digest: "sha256:a".to_owned(),
                },
            ],
            ..ConfigHashInput::new(&config)
        });
        let second = config_hash(&ConfigHashInput {
            feature_locks: vec![
                FeatureLockHashEntry {
                    feature_id: "feature-a".to_owned(),
                    digest: "sha256:a".to_owned(),
                },
                FeatureLockHashEntry {
                    feature_id: "feature-b".to_owned(),
                    digest: "sha256:b".to_owned(),
                },
            ],
            ..ConfigHashInput::new(&config)
        });

        assert_eq!(first, second);
    }

    #[test]
    fn feature_lock_digest_change_changes_hash() {
        let config = resolved_config("version = 1\n");
        let first = config_hash(&ConfigHashInput {
            feature_locks: vec![FeatureLockHashEntry {
                feature_id: "feature-a".to_owned(),
                digest: "sha256:first".to_owned(),
            }],
            ..ConfigHashInput::new(&config)
        });
        let second = config_hash(&ConfigHashInput {
            feature_locks: vec![FeatureLockHashEntry {
                feature_id: "feature-a".to_owned(),
                digest: "sha256:second".to_owned(),
            }],
            ..ConfigHashInput::new(&config)
        });

        assert_ne!(first, second);
    }

    #[test]
    fn credentials_change_changes_hash() {
        let enabled = resolved_config(
            r#"
version = 1

[credentials.git]
enabled = true
"#,
        );
        let disabled = resolved_config(
            r#"
version = 1

[credentials.git]
enabled = false
"#,
        );

        assert_ne!(hash_for(&enabled), hash_for(&disabled));
    }

    #[test]
    fn update_remote_user_uid_change_changes_hash() {
        let default_uid_sync = resolve_config(ConfigMergeInput::default());
        let disabled_uid_sync = resolve_config(ConfigMergeInput {
            devcontainer: Some(ConfigLayer {
                devcontainer: Some(LayerDevcontainerMetadata {
                    update_remote_user_uid: Some(false),
                    ..LayerDevcontainerMetadata::default()
                }),
                ..ConfigLayer::default()
            }),
            ..ConfigMergeInput::default()
        });

        assert!(default_uid_sync.devcontainer.update_remote_user_uid);
        assert!(!disabled_uid_sync.devcontainer.update_remote_user_uid);
        assert_ne!(hash_for(&default_uid_sync), hash_for(&disabled_uid_sync));
    }

    #[test]
    fn override_command_change_changes_hash() {
        let default_override = resolve_config(ConfigMergeInput::default());
        let disabled_override = resolve_config(ConfigMergeInput {
            devcontainer: Some(ConfigLayer {
                devcontainer: Some(LayerDevcontainerMetadata {
                    override_command: Some(false),
                    ..LayerDevcontainerMetadata::default()
                }),
                ..ConfigLayer::default()
            }),
            ..ConfigMergeInput::default()
        });

        assert!(default_override.devcontainer.override_command);
        assert!(!disabled_override.devcontainer.override_command);
        assert_ne!(hash_for(&default_override), hash_for(&disabled_override));
    }

    #[test]
    fn hook_change_changes_hash() {
        let first = resolved_config(
            r#"
version = 1

[[hooks.before_post_create]]
command = "first.sh"
"#,
        );
        let second = resolved_config(
            r#"
version = 1

[[hooks.before_post_create]]
command = "second.sh"
"#,
        );

        assert_ne!(hash_for(&first), hash_for(&second));
    }

    proptest! {
        #[test]
        fn cli_flag_key_order_is_stable_for_generated_maps(
            pairs in proptest::collection::vec(("[a-z]{1,8}", any::<bool>()), 0..16)
        ) {
            let config = resolved_config("version = 1\n");
            let first_map = pairs
                .iter()
                .map(|(key, value)| (key.clone(), Value::Boolean(*value)))
                .collect::<BTreeMap<_, _>>();
            let second_map = first_map
                .iter()
                .rev()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<BTreeMap<_, _>>();

            let first = config_hash(&ConfigHashInput {
                cli_flags: first_map,
                ..ConfigHashInput::new(&config)
            });
            let second = config_hash(&ConfigHashInput {
                cli_flags: second_map,
                ..ConfigHashInput::new(&config)
            });

            prop_assert_eq!(first, second);
        }
    }
}
