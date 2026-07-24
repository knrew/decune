use std::{
    collections::{BTreeMap, BTreeSet},
    net::IpAddr,
};

use serde::{
    Deserialize, Deserializer,
    de::{self, Error as _},
};
use toml::Value;

use crate::runtime::compose_ports::COMPOSE_PUBLISHED_PORT_MAPPING_INVALID;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawDecuneConfig {
    pub(crate) version: Option<u32>,
    pub(crate) use_global_config: Option<bool>,
    pub(crate) shell: Option<String>,
    #[serde(default)]
    pub(crate) features: BTreeMap<String, RawFeatureConfig>,
    #[serde(default)]
    pub(crate) container_env: BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) remote_env: BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) dotfiles: Vec<RawDotfileConfig>,
    #[serde(default)]
    pub(crate) mounts: Vec<RawMountConfig>,
    #[serde(default)]
    pub(crate) ports: RawPortsConfig,
    #[serde(default)]
    pub(crate) compose: RawComposeConfig,
    #[serde(default)]
    pub(crate) container: RawContainerConfig,
    #[serde(default)]
    pub(crate) credentials: RawCredentialsConfig,
    #[serde(default)]
    pub(crate) hooks: RawHooksConfig,
}

impl RawDecuneConfig {
    pub(crate) fn empty() -> Self {
        Self {
            version: None,
            use_global_config: None,
            shell: None,
            features: BTreeMap::new(),
            container_env: BTreeMap::new(),
            remote_env: BTreeMap::new(),
            dotfiles: Vec::new(),
            mounts: Vec::new(),
            ports: RawPortsConfig::default(),
            compose: RawComposeConfig::default(),
            container: RawContainerConfig::default(),
            credentials: RawCredentialsConfig::default(),
            hooks: RawHooksConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawContainerConfig {
    #[serde(default)]
    pub(crate) cli: RawContainerCliConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawContainerCliConfig {
    pub(crate) enabled: Option<bool>,
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
            Value::String(_)
            | Value::Integer(_)
            | Value::Float(_)
            | Value::Boolean(_)
            | Value::Datetime(_) => Err(D::Error::custom(
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
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
pub(crate) struct RawComposeConfig {
    pub(crate) published_ports: Option<RawComposePublishedPortsConfig>,
    pub(crate) clone_isolation: Option<RawComposeCloneIsolationConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawComposePublishedPortsConfig {
    pub(crate) automatic_relocation: Option<bool>,
    pub(crate) warn_on_relocation: Option<bool>,
    pub(crate) mappings: Vec<RawComposePublishedPortMappingConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawComposePublishedPortMappingConfig {
    pub(crate) enabled: Option<bool>,
    pub(crate) service: String,
    pub(crate) target: u16,
    #[serde(
        default,
        deserialize_with = "deserialize_compose_published_port_mapping_protocol"
    )]
    pub(crate) protocol: Option<RawPortProtocol>,
    pub(crate) host: Option<u16>,
    pub(crate) host_ip: Option<String>,
}

fn deserialize_compose_published_port_mapping_protocol<'de, D>(
    deserializer: D,
) -> Result<Option<RawPortProtocol>, D::Error>
where
    D: Deserializer<'de>,
{
    let protocol = Option::<String>::deserialize(deserializer)?;
    match protocol.as_deref() {
        None => Ok(None),
        Some("tcp") => Ok(Some(RawPortProtocol::Tcp)),
        Some(protocol) => Err(D::Error::custom(format!(
            "{COMPOSE_PUBLISHED_PORT_MAPPING_INVALID}: Compose published port mapping protocol `{protocol}` is unsupported; expected `tcp`"
        ))),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawComposePublishedPortsConfigFields {
    automatic_relocation: Option<bool>,
    warn_on_relocation: Option<bool>,
    #[serde(default)]
    mappings: Vec<RawComposePublishedPortMappingConfig>,
}

impl<'de> Deserialize<'de> for RawComposePublishedPortsConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let fields = RawComposePublishedPortsConfigFields::deserialize(deserializer)?;
        validate_compose_published_port_mappings::<D::Error>(&fields.mappings)?;
        Ok(Self {
            automatic_relocation: fields.automatic_relocation,
            warn_on_relocation: fields.warn_on_relocation,
            mappings: fields.mappings,
        })
    }
}

fn validate_compose_published_port_mappings<E>(
    mappings: &[RawComposePublishedPortMappingConfig],
) -> Result<(), E>
where
    E: de::Error,
{
    let mut identities = BTreeSet::new();
    for mapping in mappings {
        let protocol = mapping.protocol.unwrap_or(RawPortProtocol::Tcp);
        let identity = (mapping.service.clone(), protocol, mapping.target);
        if !identities.insert(identity) {
            return Err(E::custom(format!(
                "{COMPOSE_PUBLISHED_PORT_MAPPING_INVALID}: duplicate Compose published port mapping for service `{}`, target {}/tcp",
                mapping.service, mapping.target
            )));
        }
        if mapping.service.trim().is_empty() {
            return Err(E::custom(format!(
                "{COMPOSE_PUBLISHED_PORT_MAPPING_INVALID}: Compose published port mapping service must not be empty"
            )));
        }
        if mapping.target == 0 {
            return Err(E::custom(format!(
                "{COMPOSE_PUBLISHED_PORT_MAPPING_INVALID}: Compose published port mapping target must be in 1..=65535 for service `{}`",
                mapping.service
            )));
        }
        if !mapping.enabled.unwrap_or(true) {
            if mapping.host.is_some() || mapping.host_ip.is_some() {
                return Err(E::custom(format!(
                    "{COMPOSE_PUBLISHED_PORT_MAPPING_INVALID}: disabled Compose published port mapping must contain only its service, target, protocol, and enabled fields"
                )));
            }
            continue;
        }
        let Some(host) = mapping.host else {
            return Err(E::custom(format!(
                "{COMPOSE_PUBLISHED_PORT_MAPPING_INVALID}: Compose published port mapping host is required for service `{}`, target {}/tcp",
                mapping.service, mapping.target
            )));
        };
        if host == 0 {
            return Err(E::custom(format!(
                "{COMPOSE_PUBLISHED_PORT_MAPPING_INVALID}: Compose published port mapping host must be in 1..=65535 for service `{}`, target {}/tcp",
                mapping.service, mapping.target
            )));
        }
        if let Some(host_ip) = &mapping.host_ip
            && host_ip.parse::<IpAddr>().is_err()
        {
            return Err(E::custom(format!(
                "{COMPOSE_PUBLISHED_PORT_MAPPING_INVALID}: Compose published port mapping host_ip is not a valid IP address for service `{}`, target {}/tcp: {host_ip}",
                mapping.service, mapping.target
            )));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawComposeCloneIsolationConfig {
    pub(crate) enabled: Option<bool>,
    pub(crate) networks: Option<RawComposeCloneIsolationNetworksConfig>,
    pub(crate) names: Option<RawComposeCloneIsolationNamesConfig>,
    #[serde(default)]
    pub(crate) endpoints: Vec<RawComposeCloneIsolationEndpointConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawComposeCloneIsolationNetworksConfig {
    pub(crate) relocation: Option<bool>,
    pub(crate) subnet_pool: Option<String>,
    pub(crate) subnet_prefix: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawComposeCloneIsolationNamesConfig {
    pub(crate) rewrite_container_names: Option<bool>,
    pub(crate) rewrite_resource_names: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawComposeCloneIsolationEndpointConfig {
    pub(crate) service: String,
    pub(crate) env: String,
    pub(crate) value: String,
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
            Value::Integer(_)
            | Value::Float(_)
            | Value::Boolean(_)
            | Value::Datetime(_)
            | Value::Table(_) => Err(D::Error::custom("expected string or string array command")),
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
        let Value::Table(mut table) = value else {
            return Err(E::custom("expected table entry in [[ports]]"));
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
            r"
version = 1

[[ports]]
container = 3000

[ports.auto]
enabled = true
min = 1024
",
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

    #[test]
    fn compose_published_ports_config_is_supported() {
        let config: RawDecuneConfig = toml::from_str(
            r#"
version = 1

[compose.published_ports]
automatic_relocation = true
warn_on_relocation = true

[[compose.published_ports.mappings]]
service = "app"
target = 502
host = 1502
host_ip = "127.0.0.1"
"#,
        )
        .expect("test config should parse");

        let published_ports = config.compose.published_ports.unwrap();
        assert_eq!(published_ports.automatic_relocation, Some(true));
        assert_eq!(published_ports.warn_on_relocation, Some(true));
        assert_eq!(published_ports.mappings.len(), 1);
        assert_eq!(published_ports.mappings[0].service, "app");
        assert_eq!(published_ports.mappings[0].target, 502);
        assert_eq!(published_ports.mappings[0].protocol, None);
        assert_eq!(published_ports.mappings[0].host, Some(1502));
        assert_eq!(
            published_ports.mappings[0].host_ip.as_deref(),
            Some("127.0.0.1")
        );
    }

    #[test]
    fn compose_published_ports_legacy_relocation_key_is_rejected() {
        let error = toml::from_str::<RawDecuneConfig>(
            r"
version = 1

[compose.published_ports]
relocation = true
",
        )
        .expect_err("legacy published port relocation key must be rejected");

        assert!(error.to_string().contains("unknown field `relocation`"));
    }

    #[rstest]
    #[case(
        r#"
[[compose.published_ports.mappings]]
service = ""
target = 502
host = 1502
"#,
        "service must not be empty"
    )]
    #[case(
        r#"
[[compose.published_ports.mappings]]
service = "app"
target = 0
host = 1502
"#,
        "target must be in 1..=65535"
    )]
    #[case(
        r#"
[[compose.published_ports.mappings]]
service = "app"
target = 502
"#,
        "host is required"
    )]
    #[case(
        r#"
[[compose.published_ports.mappings]]
service = "app"
target = 502
host = 1502

[[compose.published_ports.mappings]]
service = "app"
target = 502
host = 2502
"#,
        "duplicate Compose published port mapping"
    )]
    #[case(
        r#"
[[compose.published_ports.mappings]]
service = "app"
target = 502
enabled = false
host = 1502
"#,
        "disabled Compose published port mapping"
    )]
    #[case(
        r#"
[[compose.published_ports.mappings]]
service = "app"
target = 502
host = 0
"#,
        "host must be in 1..=65535"
    )]
    #[case(
        r#"
[[compose.published_ports.mappings]]
service = "app"
target = 502
host = 1502
host_ip = "localhost"
"#,
        "host_ip is not a valid IP address"
    )]
    fn compose_published_port_mapping_rejects_invalid_config(
        #[case] mapping: &str,
        #[case] expected: &str,
    ) {
        let error = toml::from_str::<RawDecuneConfig>(&format!(
            "version = 1\n\n[compose.published_ports]\n{mapping}"
        ))
        .expect_err("invalid mapping should be rejected");

        let message = error.to_string();
        assert!(
            message.contains(COMPOSE_PUBLISHED_PORT_MAPPING_INVALID),
            "unexpected error: {message}"
        );
        assert!(message.contains(expected), "unexpected error: {message}");
    }

    #[rstest]
    #[case(
        r#"
[[compose.published_ports.mappings]]
service = "app"
target = 502
protocol = "udp"
host = 1502
"#
    )]
    #[case(
        r#"
[[compose.published_ports.mappings]]
service = "app"
target = 502
protocol = "udp"
enabled = false
"#
    )]
    fn compose_published_port_mapping_rejects_udp_protocol(#[case] mapping: &str) {
        let error = toml::from_str::<RawDecuneConfig>(&format!(
            "version = 1\n\n[compose.published_ports]\n{mapping}"
        ))
        .expect_err("UDP mapping protocol should be rejected during deserialization");

        let message = error.to_string();
        assert!(
            message.contains(COMPOSE_PUBLISHED_PORT_MAPPING_INVALID),
            "unexpected error: {message}"
        );
        assert!(message.contains("udp"), "unexpected error: {message}");
        assert!(message.contains("tcp"), "unexpected error: {message}");
    }

    #[test]
    fn compose_clone_isolation_config_is_supported() {
        let config: RawDecuneConfig = toml::from_str(
            r#"
version = 1

[compose.clone_isolation]
enabled = true

[compose.clone_isolation.networks]
relocation = true
subnet_pool = "10.200.0.0/16"
subnet_prefix = 24

[compose.clone_isolation.names]
rewrite_container_names = false
rewrite_resource_names = true

[[compose.clone_isolation.endpoints]]
service = "app"
env = "HOST_AGENT_ENDPOINT"
value = "grpc://${decune.network.fixed_net.gateway}:50051"
"#,
        )
        .expect("test config should parse");

        let clone_isolation = config.compose.clone_isolation.unwrap();
        assert_eq!(clone_isolation.enabled, Some(true));
        let networks = clone_isolation.networks.unwrap();
        assert_eq!(networks.relocation, Some(true));
        assert_eq!(networks.subnet_pool.as_deref(), Some("10.200.0.0/16"));
        assert_eq!(networks.subnet_prefix, Some(24));
        let names = clone_isolation.names.unwrap();
        assert_eq!(names.rewrite_container_names, Some(false));
        assert_eq!(names.rewrite_resource_names, Some(true));
        assert_eq!(clone_isolation.endpoints.len(), 1);
        assert_eq!(clone_isolation.endpoints[0].service, "app");
        assert_eq!(clone_isolation.endpoints[0].env, "HOST_AGENT_ENDPOINT");
        assert_eq!(
            clone_isolation.endpoints[0].value,
            "grpc://${decune.network.fixed_net.gateway}:50051"
        );
    }

    #[test]
    fn compose_clone_isolation_unknown_key_is_rejected() {
        let error = toml::from_str::<RawDecuneConfig>(
            r#"
version = 1

[compose.clone_isolation.networks]
subnet_poo1 = "10.200.0.0/16"
"#,
        )
        .unwrap_err();

        assert!(error.to_string().contains("subnet_poo1"));
    }

    #[test]
    fn container_cli_config_is_strictly_parsed() {
        let config = toml::from_str::<RawDecuneConfig>(
            r"
version = 1

[container.cli]
enabled = false
",
        )
        .unwrap();

        assert_eq!(config.container.cli.enabled, Some(false));

        let error = toml::from_str::<RawDecuneConfig>(
            r"
version = 1

[container.cli]
enable = false
",
        )
        .unwrap_err();

        assert!(error.to_string().contains("enable"));
    }

    #[derive(Debug, Deserialize)]
    struct CommandWrapper {
        command: RawCommand,
    }
}
