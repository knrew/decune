use std::collections::BTreeMap;

use serde::{
    Deserialize, Deserializer,
    de::{self, Error as _},
};
use toml::Value;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawDecuneConfig {
    pub(crate) version: Option<u32>,
    pub(crate) shell: Option<String>,
    #[serde(default)]
    pub(crate) features: BTreeMap<String, RawFeatureConfig>,
    #[serde(default)]
    pub(crate) dotfiles: Vec<RawDotfileConfig>,
    #[serde(default)]
    pub(crate) mounts: Vec<RawMountConfig>,
    #[serde(default)]
    pub(crate) ports: RawPortsConfig,
    #[serde(default)]
    pub(crate) credentials: RawCredentialsConfig,
    #[serde(default)]
    pub(crate) hooks: RawHooksConfig,
}

impl RawDecuneConfig {
    pub(crate) fn empty() -> Self {
        Self {
            version: None,
            shell: None,
            features: BTreeMap::new(),
            dotfiles: Vec::new(),
            mounts: Vec::new(),
            ports: RawPortsConfig::default(),
            credentials: RawCredentialsConfig::default(),
            hooks: RawHooksConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RawFeatureConfig {
    pub(crate) enabled: Option<bool>,
    pub(crate) options: BTreeMap<String, Value>,
}

impl<'de> Deserialize<'de> for RawFeatureConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let mut table = toml::Table::deserialize(deserializer)?;
        let enabled = match table.remove("enabled") {
            Some(value) => Some(bool_from_value(value)?),
            None => None,
        };

        Ok(Self {
            enabled,
            options: table.into_iter().collect(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawDotfileConfig {
    pub(crate) enabled: Option<bool>,
    pub(crate) source: Option<String>,
    pub(crate) target: String,
    pub(crate) read_only: Option<bool>,
    pub(crate) resolve_symlink: Option<bool>,
    pub(crate) on_conflict: Option<RawDotfileConflict>,
}

impl<'de> Deserialize<'de> for RawDotfileConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Helper {
            enabled: Option<bool>,
            source: Option<String>,
            target: String,
            read_only: Option<bool>,
            resolve_symlink: Option<bool>,
            on_conflict: Option<RawDotfileConflict>,
        }

        let helper = Helper::deserialize(deserializer)?;
        let enabled = helper.enabled.unwrap_or(true);
        if enabled && helper.source.is_none() {
            return Err(D::Error::missing_field("source"));
        }

        Ok(Self {
            enabled: helper.enabled,
            source: helper.source,
            target: helper.target,
            read_only: helper.read_only,
            resolve_symlink: helper.resolve_symlink,
            on_conflict: helper.on_conflict,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum RawDotfileConflict {
    Fail,
    ReplaceSymlink,
    Backup,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawMountConfig {
    pub(crate) enabled: Option<bool>,
    pub(crate) source: Option<String>,
    pub(crate) target: String,
    pub(crate) mount_type: Option<RawMountType>,
    pub(crate) read_only: Option<bool>,
    pub(crate) resolve_symlink: Option<bool>,
    pub(crate) create: Option<RawMountCreate>,
}

impl<'de> Deserialize<'de> for RawMountConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Helper {
            enabled: Option<bool>,
            source: Option<String>,
            target: String,
            #[serde(rename = "type")]
            mount_type: Option<RawMountType>,
            read_only: Option<bool>,
            resolve_symlink: Option<bool>,
            #[serde(default, deserialize_with = "deserialize_mount_create")]
            create: Option<RawMountCreate>,
        }

        let helper = Helper::deserialize(deserializer)?;
        let enabled = helper.enabled.unwrap_or(true);
        if enabled && helper.mount_type.is_none() {
            return Err(D::Error::missing_field("type"));
        }
        if enabled
            && matches!(helper.mount_type, Some(RawMountType::Bind))
            && helper.source.is_none()
        {
            return Err(D::Error::missing_field("source"));
        }

        Ok(Self {
            enabled: helper.enabled,
            source: helper.source,
            target: helper.target,
            mount_type: helper.mount_type,
            read_only: helper.read_only,
            resolve_symlink: helper.resolve_symlink,
            create: helper.create,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum RawMountType {
    Bind,
    Volume,
    Tmpfs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RawMountCreate {
    Directory,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct RawPortsConfig {
    pub(crate) entries: Vec<RawPortConfig>,
    pub(crate) auto: Option<RawAutoPortsConfig>,
}

impl<'de> Deserialize<'de> for RawPortsConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;

        match value {
            Value::Array(values) => ports_from_array(values),
            Value::Table(mut table) => {
                let auto = table
                    .remove("auto")
                    .map(parse_value::<RawAutoPortsConfig, D::Error>)
                    .transpose()?;

                if table.is_empty() {
                    Ok(Self {
                        entries: Vec::new(),
                        auto,
                    })
                } else {
                    Err(D::Error::custom(
                        "expected [ports.auto] or [[ports]] entries",
                    ))
                }
            }
            _ => Err(D::Error::custom(
                "expected [ports.auto] or [[ports]] entries",
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawPortConfig {
    pub(crate) enabled: Option<bool>,
    pub(crate) service: Option<String>,
    pub(crate) container: u16,
    pub(crate) host: Option<u16>,
    pub(crate) host_ip: Option<String>,
    pub(crate) protocol: Option<RawPortProtocol>,
    pub(crate) require_local: Option<bool>,
    pub(crate) label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum RawPortProtocol {
    Tcp,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawAutoPortsConfig {
    pub(crate) enabled: Option<bool>,
    pub(crate) min: Option<u16>,
    pub(crate) max: Option<u16>,
    pub(crate) ignore: Option<Vec<u16>>,
    pub(crate) on_auto_forward: Option<RawOnAutoForward>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum RawOnAutoForward {
    Notify,
    Silent,
    Ignore,
    #[serde(rename = "openBrowser")]
    OpenBrowser,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawCredentialsConfig {
    pub(crate) git: Option<RawGitCredentialsConfig>,
    pub(crate) github: Option<RawGithubCredentialsConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawGitCredentialsConfig {
    pub(crate) enabled: Option<bool>,
    pub(crate) copy_user: Option<bool>,
    pub(crate) copy_global_config: Option<bool>,
    pub(crate) https: Option<RawGitHttpsMode>,
    pub(crate) ssh_agent: Option<RawSshAgentMode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum RawGitHttpsMode {
    Off,
    HostHelper,
    HostHelperReadOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum RawSshAgentMode {
    Off,
    Auto,
    Required,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawGithubCredentialsConfig {
    pub(crate) enabled: Option<bool>,
    pub(crate) mode: Option<RawGithubCredentialsMode>,
    pub(crate) install_feature_if_missing: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum RawGithubCredentialsMode {
    Off,
    GhTokenFile,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawHooksConfig {
    #[serde(default)]
    pub(crate) before_initialize: Vec<RawHookConfig>,
    #[serde(default)]
    pub(crate) after_initialize: Vec<RawHookConfig>,
    #[serde(default)]
    pub(crate) before_on_create: Vec<RawHookConfig>,
    #[serde(default)]
    pub(crate) after_on_create: Vec<RawHookConfig>,
    #[serde(default)]
    pub(crate) before_update_content: Vec<RawHookConfig>,
    #[serde(default)]
    pub(crate) after_update_content: Vec<RawHookConfig>,
    #[serde(default)]
    pub(crate) before_post_create: Vec<RawHookConfig>,
    #[serde(default)]
    pub(crate) after_post_create: Vec<RawHookConfig>,
    #[serde(default)]
    pub(crate) before_post_start: Vec<RawHookConfig>,
    #[serde(default)]
    pub(crate) after_post_start: Vec<RawHookConfig>,
    #[serde(default)]
    pub(crate) before_post_attach: Vec<RawHookConfig>,
    #[serde(default)]
    pub(crate) after_post_attach: Vec<RawHookConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawHookConfig {
    pub(crate) command: RawCommand,
    #[serde(rename = "where")]
    pub(crate) location: Option<RawHookLocation>,
    pub(crate) user: Option<String>,
    pub(crate) shell: Option<bool>,
    pub(crate) workdir: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum RawHookLocation {
    Host,
    Container,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RawCommand {
    Shell(String),
    Args(Vec<String>),
}

impl<'de> Deserialize<'de> for RawCommand {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match Value::deserialize(deserializer)? {
            Value::String(command) => Ok(Self::Shell(command)),
            Value::Array(values) => {
                let mut args = Vec::with_capacity(values.len());

                for value in values {
                    args.push(parse_value::<String, D::Error>(value)?);
                }

                if args.is_empty() {
                    return Err(D::Error::custom("command array must not be empty"));
                }

                Ok(Self::Args(args))
            }
            _ => Err(D::Error::custom("expected string or string array command")),
        }
    }
}

fn deserialize_mount_create<'de, D>(deserializer: D) -> Result<Option<RawMountCreate>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;

    match value {
        None | Some(Value::Boolean(false)) => Ok(None),
        Some(Value::String(value)) if value == "directory" => Ok(Some(RawMountCreate::Directory)),
        Some(_) => Err(D::Error::custom(
            r#"expected false or "directory" for mount create"#,
        )),
    }
}

fn ports_from_array<E>(values: Vec<Value>) -> Result<RawPortsConfig, E>
where
    E: de::Error,
{
    let mut entries = Vec::with_capacity(values.len());
    let mut auto = None;

    for value in values {
        let mut table = match value {
            Value::Table(table) => table,
            _ => return Err(E::custom("expected table entry in [[ports]]")),
        };

        if let Some(auto_value) = table.remove("auto") {
            if auto.is_some() {
                return Err(E::custom("duplicate [ports.auto] table"));
            }
            auto = Some(parse_value::<RawAutoPortsConfig, E>(auto_value)?);
        }

        entries.push(parse_value::<RawPortConfig, E>(Value::Table(table))?);
    }

    Ok(RawPortsConfig { entries, auto })
}

fn parse_value<T, E>(value: Value) -> Result<T, E>
where
    T: for<'de> Deserialize<'de>,
    E: de::Error,
{
    value.try_into().map_err(E::custom)
}

fn bool_from_value<E>(value: Value) -> Result<bool, E>
where
    E: de::Error,
{
    parse_value(value)
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[test]
    fn command_accepts_string_and_array() {
        let shell: RawCommand = toml::from_str(r#"command = "scripts/setup.sh""#)
            .map(|wrapper: CommandWrapper| wrapper.command)
            .unwrap();
        let args: RawCommand = toml::from_str(r#"command = ["bash", "scripts/setup.sh"]"#)
            .map(|wrapper: CommandWrapper| wrapper.command)
            .unwrap();

        assert_eq!(shell, RawCommand::Shell("scripts/setup.sh".to_owned()));
        assert_eq!(
            args,
            RawCommand::Args(vec!["bash".to_owned(), "scripts/setup.sh".to_owned()])
        );
    }

    #[test]
    fn ports_auto_is_extracted_from_spec_example_shape() {
        let config: RawDecuneConfig = toml::from_str(
            r#"
version = 1

[[ports]]
container = 3000

[ports.auto]
enabled = true
min = 1024
"#,
        )
        .unwrap();

        assert_eq!(config.ports.entries.len(), 1);
        assert_eq!(config.ports.entries[0].container, 3000);
        assert_eq!(config.ports.auto.unwrap().min, Some(1024));
    }

    #[rstest]
    #[case(
        r#"
[[mounts]]
source = "/tmp/work"
target = "/work"
type = "bind"
"#
    )]
    #[case(
        r#"
[[mounts]]
target = "/work"
type = "bind"
enabled = false
"#
    )]
    fn mount_source_rules_accept_valid_shapes(#[case] mount_toml: &str) {
        let config: RawDecuneConfig = toml::from_str(&format!("version = 1\n{mount_toml}"))
            .expect("test config should parse");

        assert_eq!(config.mounts.len(), 1);
    }

    #[test]
    fn enabled_bind_mount_requires_source() {
        let error = toml::from_str::<RawDecuneConfig>(
            r#"
version = 1

[[mounts]]
target = "/work"
type = "bind"
"#,
        )
        .unwrap_err();

        assert!(error.to_string().contains("source"));
    }

    #[test]
    fn empty_command_array_is_rejected() {
        let error = toml::from_str::<CommandWrapper>("command = []").unwrap_err();

        assert!(
            error
                .to_string()
                .contains("command array must not be empty")
        );
    }

    #[rstest]
    #[case("fail", RawDotfileConflict::Fail)]
    #[case("replace-symlink", RawDotfileConflict::ReplaceSymlink)]
    #[case("backup", RawDotfileConflict::Backup)]
    fn dotfile_conflict_values_are_supported(
        #[case] input: &str,
        #[case] expected: RawDotfileConflict,
    ) {
        let config: RawDecuneConfig = toml::from_str(&format!(
            r#"
version = 1

[[dotfiles]]
source = "~/.config/nvim"
target = ".config/nvim"
on_conflict = "{input}"
"#
        ))
        .expect("test config should parse");

        assert_eq!(config.dotfiles[0].on_conflict, Some(expected));
    }

    #[rstest]
    #[case("notify", RawOnAutoForward::Notify)]
    #[case("silent", RawOnAutoForward::Silent)]
    #[case("ignore", RawOnAutoForward::Ignore)]
    #[case("openBrowser", RawOnAutoForward::OpenBrowser)]
    fn auto_forward_values_are_supported(#[case] input: &str, #[case] expected: RawOnAutoForward) {
        let config: RawDecuneConfig = toml::from_str(&format!(
            r#"
version = 1

[ports.auto]
on_auto_forward = "{input}"
"#
        ))
        .expect("test config should parse");

        assert_eq!(config.ports.auto.unwrap().on_auto_forward, Some(expected));
    }

    #[derive(Debug, Deserialize)]
    struct CommandWrapper {
        command: RawCommand,
    }
}
