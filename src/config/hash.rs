use std::collections::BTreeMap;

use serde_json::Value as JsonValue;
use toml::Value;

use crate::config::{
    canonical::{CanonicalWriter, sha256_hex},
    resolved::{
        ResolvedConfig, ResolvedDevcontainer, ResolvedDevcontainerMount,
        ResolvedDevcontainerSource, ResolvedDotfile, ResolvedDotfileEntry, ResolvedHook,
        ResolvedPublishPort, ResolvedRunArg, ResolvedShutdownAction, ResolvedUserEnvProbe,
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
    pub(crate) internal_versions: BTreeMap<String, String>,
    pub(crate) build: Option<BuildHashInput>,
    pub(crate) compose_files: Vec<ComposeFileHashInput>,
    pub(crate) compose_generated_override: Option<ComposeGeneratedOverrideHashInput>,
    pub(crate) compose_canonical_model: Option<JsonValue>,
    pub(crate) resolved_mounts: Vec<MountHashInput>,
    pub(crate) startup_command: Option<StartupCommandHashInput>,
    pub(crate) uid_gid_sync: Option<UidGidSyncHashInput>,
}

impl<'a> ConfigHashInput<'a> {
    pub(crate) fn new(config: &'a ResolvedConfig) -> Self {
        Self {
            config,
            feature_locks: Vec::new(),
            cli_flags: BTreeMap::new(),
            internal_versions: BTreeMap::new(),
            build: None,
            compose_files: Vec::new(),
            compose_generated_override: None,
            compose_canonical_model: None,
            resolved_mounts: Vec::new(),
            startup_command: None,
            uid_gid_sync: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposeFileHashInput {
    pub(crate) canonical_path: String,
    pub(crate) digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposeGeneratedOverrideHashInput {
    pub(crate) path: String,
    pub(crate) content_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MountHashInput {
    pub(crate) source: Option<String>,
    pub(crate) target: String,
    pub(crate) mount_type: MountType,
    pub(crate) read_only: bool,
    pub(crate) consistency: Option<String>,
    pub(crate) bind_options: Option<MountBindOptionsHashInput>,
    pub(crate) volume_options: Option<MountVolumeOptionsHashInput>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MountBindOptionsHashInput {
    pub(crate) propagation: Option<String>,
    pub(crate) non_recursive: Option<bool>,
    pub(crate) create_mountpoint: Option<bool>,
    pub(crate) read_only_non_recursive: Option<bool>,
    pub(crate) read_only_force_recursive: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MountVolumeOptionsHashInput {
    pub(crate) no_copy: Option<bool>,
    pub(crate) labels: Option<BTreeMap<String, String>>,
    pub(crate) driver_config: Option<MountVolumeDriverConfigHashInput>,
    pub(crate) subpath: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MountVolumeDriverConfigHashInput {
    pub(crate) name: Option<String>,
    pub(crate) options: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FeatureLockHashEntry {
    pub(crate) feature_id: String,
    pub(crate) digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StartupCommandHashInput {
    pub(crate) entrypoint: Vec<String>,
    pub(crate) command: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UidGidSyncHashInput {
    pub(crate) state: UidGidSyncHashState,
    pub(crate) host_uid: u32,
    pub(crate) host_gid: u32,
    pub(crate) target_kind: Option<String>,
    pub(crate) target_user: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UidGidSyncHashState {
    Sync,
    Noop(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct BuildHashInput {
    pub(crate) dockerfile_path: Option<String>,
    pub(crate) dockerfile_content_hash: Option<String>,
    pub(crate) context_path: Option<String>,
    pub(crate) dockerignore_path: Option<String>,
    pub(crate) dockerignore_content_hash: Option<String>,
    pub(crate) context_content_hash: Option<String>,
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
    writer.field("internal_versions", |writer| {
        writer.map(input.internal_versions.iter(), |writer, value| {
            writer.string(value);
        });
    });
    writer.field("build", |writer| match &input.build {
        Some(build) => write_build_input(writer, build),
        None => writer.none(),
    });
    if !input.compose_files.is_empty() {
        writer.field("compose_files", |writer| {
            write_compose_file_inputs(writer, &input.compose_files);
        });
    }
    if let Some(generated_override) = &input.compose_generated_override {
        writer.field("compose_generated_override", |writer| {
            write_compose_generated_override_input(writer, generated_override);
        });
    }
    if let Some(model) = &input.compose_canonical_model {
        let model = redact_compose_canonical_model_for_hash(model);
        writer.field("compose_canonical_model", |writer| {
            writer.json_value(&model);
        });
    }
    writer.field("resolved_mounts", |writer| {
        write_resolved_mounts(writer, &input.resolved_mounts);
    });
    writer.field("startup_command", |writer| match &input.startup_command {
        Some(startup_command) => write_startup_command(writer, startup_command),
        None => writer.none(),
    });
    writer.field("uid_gid_sync", |writer| match &input.uid_gid_sync {
        Some(uid_gid_sync) => write_uid_gid_sync_input(writer, uid_gid_sync),
        None => writer.none(),
    });

    sha256_hex(writer.finish().as_bytes())
}

fn write_compose_generated_override_input(
    writer: &mut CanonicalWriter,
    input: &ComposeGeneratedOverrideHashInput,
) {
    writer.object("ComposeGeneratedOverride", |writer| {
        writer.field("path", |writer| writer.string(&input.path));
        writer.field("content_hash", |writer| writer.string(&input.content_hash));
    });
}

fn write_compose_file_inputs(writer: &mut CanonicalWriter, inputs: &[ComposeFileHashInput]) {
    writer.seq(inputs.iter(), |writer, input| {
        writer.object("ComposeFile", |writer| {
            writer.field("canonical_path", |writer| {
                writer.string(&input.canonical_path)
            });
            writer.field("digest", |writer| writer.string(&input.digest));
        });
    });
}

fn redact_compose_canonical_model_for_hash(model: &JsonValue) -> JsonValue {
    let mut model = model.clone();
    redact_compose_canonical_model_value(&mut model, &mut Vec::new());
    model
}

fn redact_compose_canonical_model_value(value: &mut JsonValue, path: &mut Vec<String>) {
    match value {
        JsonValue::Object(map) => {
            if is_compose_service_environment_path(path) {
                redact_json_leaf_values(value);
                return;
            }
            for (child_key, child_value) in map {
                path.push(child_key.clone());
                redact_compose_canonical_model_value(child_value, path);
                path.pop();
            }
        }
        JsonValue::Array(values) => {
            for value in values {
                redact_compose_canonical_model_value(value, path);
            }
        }
        _ => {}
    }
}

fn is_compose_service_environment_path(path: &[String]) -> bool {
    path.len() == 3 && path[0] == "services" && path[2] == "environment"
}

fn redact_json_leaf_values(value: &mut JsonValue) {
    match value {
        JsonValue::Object(map) => {
            for value in map.values_mut() {
                redact_json_leaf_values(value);
            }
        }
        JsonValue::Array(values) => {
            for value in values {
                redact_json_leaf_values(value);
            }
        }
        JsonValue::Null => {}
        _ => *value = JsonValue::String("<redacted>".to_owned()),
    }
}

fn write_uid_gid_sync_input(writer: &mut CanonicalWriter, input: &UidGidSyncHashInput) {
    writer.object("UidGidSync", |writer| {
        writer.field("state", |writer| match &input.state {
            UidGidSyncHashState::Sync => writer.string("sync"),
            UidGidSyncHashState::Noop(reason) => writer.string(&format!("noop:{reason}")),
        });
        writer.field("host_uid", |writer| {
            writer.string(&input.host_uid.to_string())
        });
        writer.field("host_gid", |writer| {
            writer.string(&input.host_gid.to_string())
        });
        writer.field("target_kind", |writer| {
            writer.option_string(input.target_kind.as_deref());
        });
        writer.field("target_user", |writer| {
            writer.option_string(input.target_user.as_deref());
        });
    });
}

fn write_startup_command(writer: &mut CanonicalWriter, startup_command: &StartupCommandHashInput) {
    writer.object("StartupCommand", |writer| {
        writer.field("entrypoint", |writer| {
            writer.seq(startup_command.entrypoint.iter(), |writer, value| {
                writer.string(value);
            });
        });
        writer.field("command", |writer| {
            writer.seq(startup_command.command.iter(), |writer, value| {
                writer.string(value);
            });
        });
    });
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
            writer.field("consistency", |writer| {
                writer.option_string(mount.consistency.as_deref());
            });
            writer.field("bind_options", |writer| match &mount.bind_options {
                Some(options) => write_mount_bind_options(writer, options),
                None => writer.none(),
            });
            writer.field("volume_options", |writer| match &mount.volume_options {
                Some(options) => write_mount_volume_options(writer, options),
                None => writer.none(),
            });
        });
    });
}

fn write_mount_bind_options(writer: &mut CanonicalWriter, options: &MountBindOptionsHashInput) {
    writer.object("MountBindOptions", |writer| {
        writer.field("propagation", |writer| {
            writer.option_string(options.propagation.as_deref());
        });
        writer.field("non_recursive", |writer| {
            write_option_bool(writer, options.non_recursive);
        });
        writer.field("create_mountpoint", |writer| {
            write_option_bool(writer, options.create_mountpoint);
        });
        writer.field("read_only_non_recursive", |writer| {
            write_option_bool(writer, options.read_only_non_recursive);
        });
        writer.field("read_only_force_recursive", |writer| {
            write_option_bool(writer, options.read_only_force_recursive);
        });
    });
}

fn write_mount_volume_options(writer: &mut CanonicalWriter, options: &MountVolumeOptionsHashInput) {
    writer.object("MountVolumeOptions", |writer| {
        writer.field("no_copy", |writer| {
            write_option_bool(writer, options.no_copy)
        });
        writer.field("labels", |writer| match &options.labels {
            Some(labels) => writer.map(labels.iter(), |writer, value| writer.string(value)),
            None => writer.none(),
        });
        writer.field("driver_config", |writer| match &options.driver_config {
            Some(driver_config) => write_mount_volume_driver_config(writer, driver_config),
            None => writer.none(),
        });
        writer.field("subpath", |writer| {
            writer.option_string(options.subpath.as_deref());
        });
    });
}

fn write_mount_volume_driver_config(
    writer: &mut CanonicalWriter,
    driver_config: &MountVolumeDriverConfigHashInput,
) {
    writer.object("MountVolumeDriverConfig", |writer| {
        writer.field("name", |writer| {
            writer.option_string(driver_config.name.as_deref());
        });
        writer.field("options", |writer| match &driver_config.options {
            Some(options) => writer.map(options.iter(), |writer, value| writer.string(value)),
            None => writer.none(),
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
            if config.dotfile_entries.is_empty() {
                writer.seq(config.dotfiles.iter(), write_enabled_dotfile);
            } else {
                writer.seq(config.dotfile_entries.iter(), |writer, entry| match entry {
                    ResolvedDotfileEntry::Enabled(dotfile) => {
                        write_enabled_dotfile(writer, dotfile);
                    }
                    ResolvedDotfileEntry::Disabled(dotfile) => {
                        writer.object("Dotfile", |writer| {
                            writer.field("target", |writer| writer.string(&dotfile.target));
                            writer.field("enabled", |writer| writer.bool(false));
                            writer.field("origin", |writer| {
                                writer.string(config_path_origin_name(dotfile.origin));
                            });
                        });
                    }
                });
            }
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

fn write_enabled_dotfile(writer: &mut CanonicalWriter, dotfile: &ResolvedDotfile) {
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
        writer.field("origin", |writer| {
            writer.string(config_path_origin_name(dotfile.origin));
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
        writer.field("shutdown_action", |writer| {
            writer.string(shutdown_action_name(devcontainer.shutdown_action));
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
        writer.field("entrypoints", |writer| {
            writer.seq(devcontainer.entrypoints.iter(), |writer, entrypoint| {
                writer.string(entrypoint);
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
        ResolvedDevcontainerSource::Compose(compose) => {
            writer.object("ComposeSource", |writer| {
                writer.field("files", |writer| {
                    writer.seq(compose.files.iter(), |writer, file| writer.string(file));
                });
                writer.field("service", |writer| writer.string(&compose.service));
                writer.field("run_services", |writer| match &compose.run_services {
                    Some(run_services) => {
                        writer.seq(run_services.iter(), |writer, service| {
                            writer.string(service)
                        });
                    }
                    None => writer.none(),
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
                let commands = lifecycle.commands(stage);
                if commands.is_empty() {
                    writer.none();
                } else {
                    writer.seq(commands.iter(), |writer, command| {
                        write_lifecycle_command(writer, command);
                    });
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
        writer.field("dockerignore_path", |writer| {
            writer.option_string(build.dockerignore_path.as_deref());
        });
        writer.field("dockerignore_content_hash", |writer| {
            writer.option_string(build.dockerignore_content_hash.as_deref());
        });
        writer.field("context_content_hash", |writer| {
            writer.option_string(build.context_content_hash.as_deref());
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

fn config_path_origin_name(value: crate::config::path::ConfigPathOrigin) -> &'static str {
    match value {
        crate::config::path::ConfigPathOrigin::Global => "global",
        crate::config::path::ConfigPathOrigin::Project => "project",
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

fn shutdown_action_name(value: ResolvedShutdownAction) -> &'static str {
    match value {
        ResolvedShutdownAction::None => "none",
        ResolvedShutdownAction::StopContainer => "stopContainer",
        ResolvedShutdownAction::StopCompose => "stopCompose",
    }
}

fn port_protocol_name(value: PortProtocol) -> &'static str {
    match value {
        PortProtocol::Tcp => "tcp",
        PortProtocol::Udp => "udp",
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

    fn legacy_hash_without_compose_files_field(input: &ConfigHashInput<'_>) -> String {
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
        writer.field("internal_versions", |writer| {
            writer.map(input.internal_versions.iter(), |writer, value| {
                writer.string(value);
            });
        });
        writer.field("build", |writer| match &input.build {
            Some(build) => write_build_input(writer, build),
            None => writer.none(),
        });
        writer.field("resolved_mounts", |writer| {
            write_resolved_mounts(writer, &input.resolved_mounts);
        });
        writer.field("startup_command", |writer| match &input.startup_command {
            Some(startup_command) => write_startup_command(writer, startup_command),
            None => writer.none(),
        });
        writer.field("uid_gid_sync", |writer| match &input.uid_gid_sync {
            Some(uid_gid_sync) => write_uid_gid_sync_input(writer, uid_gid_sync),
            None => writer.none(),
        });

        sha256_hex(writer.finish().as_bytes())
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
    fn empty_compose_file_inputs_preserve_legacy_non_compose_hash() {
        let config = resolved_config(
            r#"
version = 1
shell = "/bin/zsh"
"#,
        );
        let input = ConfigHashInput::new(&config);

        assert_eq!(
            config_hash(&input),
            legacy_hash_without_compose_files_field(&input)
        );
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
                            ..LayerPortAttributes::default()
                        },
                    )]),
                    other_ports_attributes: Some(LayerPortAttributes {
                        label: Some("other".to_owned()),
                        on_auto_forward: Some(OnAutoForward::Ignore),
                        require_local_port: Some(false),
                        ..LayerPortAttributes::default()
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
    fn dockerignore_path_alone_changes_hash() {
        let config = resolved_config("version = 1\n");
        let first = config_hash(&ConfigHashInput {
            build: Some(BuildHashInput {
                dockerignore_path: Some(".devcontainer/.dockerignore".to_owned()),
                ..BuildHashInput::default()
            }),
            ..ConfigHashInput::new(&config)
        });
        let second = config_hash(&ConfigHashInput {
            build: Some(BuildHashInput {
                dockerignore_path: Some(".devcontainer/Dockerfile.dockerignore".to_owned()),
                ..BuildHashInput::default()
            }),
            ..ConfigHashInput::new(&config)
        });

        assert_ne!(first, second);
    }

    #[test]
    fn context_content_hash_alone_changes_hash() {
        let config = resolved_config("version = 1\n");
        let first = config_hash(&ConfigHashInput {
            build: Some(BuildHashInput {
                context_content_hash: Some("sha256:first".to_owned()),
                ..BuildHashInput::default()
            }),
            ..ConfigHashInput::new(&config)
        });
        let second = config_hash(&ConfigHashInput {
            build: Some(BuildHashInput {
                context_content_hash: Some("sha256:second".to_owned()),
                ..BuildHashInput::default()
            }),
            ..ConfigHashInput::new(&config)
        });

        assert_ne!(first, second);
    }

    #[test]
    fn uid_gid_sync_state_changes_hash() {
        let config = resolved_config("version = 1\n");
        let without_sync = config_hash(&ConfigHashInput::new(&config));
        let with_sync = config_hash(&ConfigHashInput {
            uid_gid_sync: Some(UidGidSyncHashInput {
                state: UidGidSyncHashState::Sync,
                host_uid: 1000,
                host_gid: 1000,
                target_kind: Some("remoteUser".to_owned()),
                target_user: Some("vscode".to_owned()),
            }),
            ..ConfigHashInput::new(&config)
        });
        let other_host = config_hash(&ConfigHashInput {
            uid_gid_sync: Some(UidGidSyncHashInput {
                state: UidGidSyncHashState::Sync,
                host_uid: 1001,
                host_gid: 1000,
                target_kind: Some("remoteUser".to_owned()),
                target_user: Some("vscode".to_owned()),
            }),
            ..ConfigHashInput::new(&config)
        });
        let no_explicit_user = config_hash(&ConfigHashInput {
            uid_gid_sync: Some(UidGidSyncHashInput {
                state: UidGidSyncHashState::Noop("noExplicitUser".to_owned()),
                host_uid: 1000,
                host_gid: 1000,
                target_kind: None,
                target_user: None,
            }),
            ..ConfigHashInput::new(&config)
        });

        assert_ne!(without_sync, with_sync);
        assert_ne!(with_sync, other_host);
        assert_ne!(with_sync, no_explicit_user);
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
    fn config_hash_changes_when_compose_file_digest_changes() {
        let config = resolved_config("version = 1\n");
        let first = config_hash(&ConfigHashInput {
            compose_files: vec![ComposeFileHashInput {
                canonical_path: "/workspace/.devcontainer/compose.yaml".to_owned(),
                digest: "sha256:first".to_owned(),
            }],
            ..ConfigHashInput::new(&config)
        });
        let second = config_hash(&ConfigHashInput {
            compose_files: vec![ComposeFileHashInput {
                canonical_path: "/workspace/.devcontainer/compose.yaml".to_owned(),
                digest: "sha256:second".to_owned(),
            }],
            ..ConfigHashInput::new(&config)
        });

        assert_ne!(first, second);
    }

    #[test]
    fn config_hash_changes_when_compose_file_canonical_path_changes() {
        let config = resolved_config("version = 1\n");
        let first = config_hash(&ConfigHashInput {
            compose_files: vec![ComposeFileHashInput {
                canonical_path: "/workspace/compose.yaml".to_owned(),
                digest: "sha256:same".to_owned(),
            }],
            ..ConfigHashInput::new(&config)
        });
        let second = config_hash(&ConfigHashInput {
            compose_files: vec![ComposeFileHashInput {
                canonical_path: "/workspace/.devcontainer/compose.yaml".to_owned(),
                digest: "sha256:same".to_owned(),
            }],
            ..ConfigHashInput::new(&config)
        });

        assert_ne!(first, second);
    }

    #[test]
    fn config_hash_changes_when_compose_canonical_model_non_secret_changes() {
        let config = resolved_config("version = 1\n");
        let first = config_hash(&ConfigHashInput {
            compose_files: vec![ComposeFileHashInput {
                canonical_path: "/workspace/.devcontainer/compose.yaml".to_owned(),
                digest: "sha256:same".to_owned(),
            }],
            compose_canonical_model: Some(serde_json::json!({
                "services": {
                    "app": {
                        "image": "alpine:3.20"
                    }
                }
            })),
            ..ConfigHashInput::new(&config)
        });
        let second = config_hash(&ConfigHashInput {
            compose_files: vec![ComposeFileHashInput {
                canonical_path: "/workspace/.devcontainer/compose.yaml".to_owned(),
                digest: "sha256:same".to_owned(),
            }],
            compose_canonical_model: Some(serde_json::json!({
                "services": {
                    "app": {
                        "image": "alpine:3.21"
                    }
                }
            })),
            ..ConfigHashInput::new(&config)
        });

        assert_ne!(first, second);
    }

    #[test]
    fn config_hash_redacts_compose_environment_values() {
        let config = resolved_config("version = 1\n");
        let first = config_hash(&ConfigHashInput {
            compose_files: vec![ComposeFileHashInput {
                canonical_path: "/workspace/.devcontainer/compose.yaml".to_owned(),
                digest: "sha256:same".to_owned(),
            }],
            compose_canonical_model: Some(serde_json::json!({
                "services": {
                    "app": {
                        "image": "alpine:3.20",
                        "environment": {
                            "TOKEN": "first-secret"
                        }
                    }
                }
            })),
            ..ConfigHashInput::new(&config)
        });
        let second = config_hash(&ConfigHashInput {
            compose_files: vec![ComposeFileHashInput {
                canonical_path: "/workspace/.devcontainer/compose.yaml".to_owned(),
                digest: "sha256:same".to_owned(),
            }],
            compose_canonical_model: Some(serde_json::json!({
                "services": {
                    "app": {
                        "image": "alpine:3.20",
                        "environment": {
                            "TOKEN": "second-secret"
                        }
                    }
                }
            })),
            ..ConfigHashInput::new(&config)
        });

        assert_eq!(first, second);
    }

    #[test]
    fn config_hash_keeps_compose_environment_keys() {
        let config = resolved_config("version = 1\n");
        let first = config_hash(&ConfigHashInput {
            compose_files: vec![ComposeFileHashInput {
                canonical_path: "/workspace/.devcontainer/compose.yaml".to_owned(),
                digest: "sha256:same".to_owned(),
            }],
            compose_canonical_model: Some(serde_json::json!({
                "services": {
                    "app": {
                        "image": "alpine:3.20",
                        "environment": {
                            "TOKEN": "secret"
                        }
                    }
                }
            })),
            ..ConfigHashInput::new(&config)
        });
        let second = config_hash(&ConfigHashInput {
            compose_files: vec![ComposeFileHashInput {
                canonical_path: "/workspace/.devcontainer/compose.yaml".to_owned(),
                digest: "sha256:same".to_owned(),
            }],
            compose_canonical_model: Some(serde_json::json!({
                "services": {
                    "app": {
                        "image": "alpine:3.20",
                        "environment": {
                            "OTHER_TOKEN": "secret"
                        }
                    }
                }
            })),
            ..ConfigHashInput::new(&config)
        });

        assert_ne!(first, second);
    }

    #[test]
    fn config_hash_keeps_compose_secret_source_metadata() {
        let config = resolved_config("version = 1\n");
        let first = config_hash(&ConfigHashInput {
            compose_files: vec![ComposeFileHashInput {
                canonical_path: "/workspace/.devcontainer/compose.yaml".to_owned(),
                digest: "sha256:same".to_owned(),
            }],
            compose_canonical_model: Some(serde_json::json!({
                "services": {
                    "app": {
                        "image": "alpine:3.20",
                        "secrets": ["app_token"]
                    }
                },
                "secrets": {
                    "app_token": {
                        "file": "/tmp/first-token"
                    }
                }
            })),
            ..ConfigHashInput::new(&config)
        });
        let second = config_hash(&ConfigHashInput {
            compose_files: vec![ComposeFileHashInput {
                canonical_path: "/workspace/.devcontainer/compose.yaml".to_owned(),
                digest: "sha256:same".to_owned(),
            }],
            compose_canonical_model: Some(serde_json::json!({
                "services": {
                    "app": {
                        "image": "alpine:3.20",
                        "secrets": ["app_token"]
                    }
                },
                "secrets": {
                    "app_token": {
                        "file": "/tmp/second-token"
                    }
                }
            })),
            ..ConfigHashInput::new(&config)
        });

        assert_ne!(first, second);
    }

    #[test]
    fn config_hash_keeps_compose_service_secret_mount_metadata() {
        let config = resolved_config("version = 1\n");
        let first = config_hash(&ConfigHashInput {
            compose_files: vec![ComposeFileHashInput {
                canonical_path: "/workspace/.devcontainer/compose.yaml".to_owned(),
                digest: "sha256:same".to_owned(),
            }],
            compose_canonical_model: Some(serde_json::json!({
                "services": {
                    "app": {
                        "image": "alpine:3.20",
                        "secrets": [
                            {
                                "source": "app_token",
                                "target": "app_token.txt",
                                "uid": "103",
                                "gid": "103",
                                "mode": "0440"
                            }
                        ]
                    }
                },
                "secrets": {
                    "app_token": {
                        "environment": "APP_TOKEN"
                    }
                }
            })),
            ..ConfigHashInput::new(&config)
        });
        let second = config_hash(&ConfigHashInput {
            compose_files: vec![ComposeFileHashInput {
                canonical_path: "/workspace/.devcontainer/compose.yaml".to_owned(),
                digest: "sha256:same".to_owned(),
            }],
            compose_canonical_model: Some(serde_json::json!({
                "services": {
                    "app": {
                        "image": "alpine:3.20",
                        "secrets": [
                            {
                                "source": "app_token",
                                "target": "renamed-token.txt",
                                "uid": "103",
                                "gid": "103",
                                "mode": "0440"
                            }
                        ]
                    }
                },
                "secrets": {
                    "app_token": {
                        "environment": "APP_TOKEN"
                    }
                }
            })),
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
    fn disabled_dotfile_tombstone_changes_hash() {
        let enabled = resolve_config(ConfigMergeInput {
            global: Some(ConfigLayer::from_raw_decune(
                toml::from_str(
                    r#"
version = 1

[[dotfiles]]
source = "~/.config/gitconfig"
target = ".config/${remoteUser}/gitconfig"
"#,
                )
                .unwrap(),
            )),
            ..ConfigMergeInput::default()
        });
        let disabled = resolve_config(ConfigMergeInput {
            global: Some(ConfigLayer::from_raw_decune(
                toml::from_str(
                    r#"
version = 1

[[dotfiles]]
source = "~/.config/gitconfig"
target = ".config/${remoteUser}/gitconfig"
"#,
                )
                .unwrap(),
            )),
            project: Some(ConfigLayer::from_raw_decune(
                toml::from_str(
                    r#"
version = 1

[[dotfiles]]
target = ".config/vscode/gitconfig"
enabled = false
"#,
                )
                .unwrap(),
            )),
            ..ConfigMergeInput::default()
        });

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
    fn original_image_startup_command_change_changes_hash() {
        let config = resolve_config(ConfigMergeInput::default());
        let first = config_hash(&ConfigHashInput {
            startup_command: Some(StartupCommandHashInput {
                entrypoint: vec!["/docker-entrypoint.sh".to_owned()],
                command: vec!["server".to_owned()],
            }),
            ..ConfigHashInput::new(&config)
        });
        let second = config_hash(&ConfigHashInput {
            startup_command: Some(StartupCommandHashInput {
                entrypoint: vec!["/docker-entrypoint.sh".to_owned()],
                command: vec!["worker".to_owned()],
            }),
            ..ConfigHashInput::new(&config)
        });

        assert_ne!(first, second);
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
