#![allow(dead_code)]

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};
use toml::Value;

use crate::config::merge::{
    Command, DotfileConflict, GitHttpsMode, GithubCredentialsMode, HookLocation, MountCreate,
    MountType, OnAutoForward, PortProtocol, ResolvedConfig, SshAgentMode,
};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ConfigHashInput<'a> {
    pub(crate) config: &'a ResolvedConfig,
    pub(crate) feature_locks: Vec<FeatureLockHashEntry>,
    pub(crate) cli_flags: BTreeMap<String, Value>,
    pub(crate) build: Option<BuildHashInput>,
}

impl<'a> ConfigHashInput<'a> {
    pub(crate) fn new(config: &'a ResolvedConfig) -> Self {
        Self {
            config,
            feature_locks: Vec::new(),
            cli_flags: BTreeMap::new(),
            build: None,
        }
    }
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

    sha256_hex(writer.finish().as_bytes())
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

fn write_hooks(writer: &mut CanonicalWriter, hooks: &[crate::config::merge::ResolvedHook]) {
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

#[derive(Debug, Default)]
struct CanonicalWriter {
    output: String,
}

impl CanonicalWriter {
    fn finish(self) -> String {
        self.output
    }

    fn object<F>(&mut self, name: &str, write_fields: F)
    where
        F: FnOnce(&mut Self),
    {
        self.output.push_str("object");
        self.string(name);
        self.output.push('[');
        write_fields(self);
        self.output.push(']');
    }

    fn field<F>(&mut self, name: &str, write_value: F)
    where
        F: FnOnce(&mut Self),
    {
        self.output.push_str("field");
        self.string(name);
        self.output.push('=');
        write_value(self);
        self.output.push(';');
    }

    fn seq<'a, T, I, F>(&mut self, values: I, mut write_value: F)
    where
        I: IntoIterator<Item = &'a T>,
        T: 'a,
        F: FnMut(&mut Self, &'a T),
    {
        self.output.push_str("seq[");
        for value in values {
            write_value(self, value);
            self.output.push(';');
        }
        self.output.push(']');
    }

    fn map<'a, V, I, F>(&mut self, entries: I, mut write_value: F)
    where
        I: IntoIterator<Item = (&'a String, &'a V)>,
        V: 'a,
        F: FnMut(&mut Self, &'a V),
    {
        let mut entries = entries.into_iter().collect::<Vec<_>>();
        entries.sort_by_key(|(key, _)| *key);

        self.output.push_str("map{");
        for (key, value) in entries {
            self.string(key);
            self.output.push('=');
            write_value(self, value);
            self.output.push(';');
        }
        self.output.push('}');
    }

    fn option_string(&mut self, value: Option<&str>) {
        match value {
            Some(value) => self.string(value),
            None => self.none(),
        }
    }

    fn string(&mut self, value: &str) {
        self.output.push('s');
        self.output.push_str(&value.len().to_string());
        self.output.push(':');
        self.output.push_str(value);
    }

    fn bool(&mut self, value: bool) {
        if value {
            self.output.push_str("b1");
        } else {
            self.output.push_str("b0");
        }
    }

    fn u16(&mut self, value: u16) {
        self.output.push('u');
        self.output.push_str(&value.to_string());
    }

    fn i64(&mut self, value: i64) {
        self.output.push('i');
        self.output.push_str(&value.to_string());
    }

    fn f64(&mut self, value: f64) {
        self.output.push('f');
        push_hex_u64(&mut self.output, value.to_bits());
    }

    fn none(&mut self) {
        self.output.push_str("none");
    }

    fn toml_value(&mut self, value: &Value) {
        match value {
            Value::String(value) => {
                self.output.push_str("toml-string");
                self.string(value);
            }
            Value::Integer(value) => {
                self.output.push_str("toml-integer");
                self.i64(*value);
            }
            Value::Float(value) => {
                self.output.push_str("toml-float");
                self.f64(*value);
            }
            Value::Boolean(value) => {
                self.output.push_str("toml-boolean");
                self.bool(*value);
            }
            Value::Datetime(value) => {
                self.output.push_str("toml-datetime");
                self.string(&value.to_string());
            }
            Value::Array(values) => {
                self.output.push_str("toml-array");
                self.seq(values.iter(), |writer, value| writer.toml_value(value));
            }
            Value::Table(values) => {
                self.output.push_str("toml-table");
                self.map(values.iter(), |writer, value| writer.toml_value(value));
            }
        }
    }
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

fn port_protocol_name(value: PortProtocol) -> &'static str {
    match value {
        PortProtocol::Tcp => "tcp",
    }
}

fn on_auto_forward_name(value: OnAutoForward) -> &'static str {
    match value {
        OnAutoForward::Notify => "notify",
        OnAutoForward::Silent => "silent",
        OnAutoForward::Ignore => "ignore",
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

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(digest.len() * 2);

    for byte in digest {
        push_hex_byte(&mut output, byte);
    }

    output
}

fn push_hex_u64(output: &mut String, value: u64) {
    for byte in value.to_be_bytes() {
        push_hex_byte(output, byte);
    }
}

fn push_hex_byte(output: &mut String, byte: u8) {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    output.push(HEX[(byte >> 4) as usize] as char);
    output.push(HEX[(byte & 0x0f) as usize] as char);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::merge::{ConfigLayer, ConfigMergeInput, OnAutoForward, resolve_config};

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
}
