use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use anyhow::{Context, Result, bail};
use serde_json::Value as JsonValue;

use crate::{
    config::types::MountType,
    docker::mounts::{DockerMountSpec, MountBindOptions, MountVolumeOptions},
};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ComposeOverridePatch {
    services: BTreeMap<String, ComposeOverrideServicePatch>,
    networks: BTreeMap<String, ComposeOverrideNetworkPatch>,
    volumes: BTreeMap<String, String>,
    configs: BTreeMap<String, String>,
    secrets: BTreeMap<String, String>,
    forbidden_secret_values: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposeOverrideServicePatch {
    name: String,
    image: Option<String>,
    container_name: Option<String>,
    pull_policy: Option<String>,
    labels: BTreeMap<String, String>,
    environment: BTreeMap<String, ComposeOverrideEnvironmentValue>,
    user: Option<String>,
    init: Option<bool>,
    privileged: Option<bool>,
    cap_add: Vec<String>,
    security_opt: Vec<String>,
    mounts: Vec<ComposeOverrideMount>,
    network_aliases: BTreeMap<String, BTreeSet<String>>,
    ports_override: Vec<ComposeOverridePortEntry>,
    entrypoint: Vec<String>,
    command: Vec<String>,
    forbidden_secret_values: Vec<String>,
}

pub(crate) type ComposeOverridePortEntry = BTreeMap<String, JsonValue>;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct ComposeOverrideNetworkPatch {
    name: Option<String>,
    ipam_config_override: Vec<ComposeOverrideNetworkIpamConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposeOverrideNetworkIpamConfig {
    pub(crate) subnet: String,
    pub(crate) gateway: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ComposeOverrideEnvironmentValue {
    Literal(String),
    Interpolated {
        placeholder: String,
        redactions: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposeOverrideMount {
    source: Option<String>,
    target: String,
    mount_type: MountType,
    read_only: bool,
    consistency: Option<String>,
    bind_options: Option<MountBindOptions>,
    volume_options: Option<MountVolumeOptions>,
}

impl ComposeOverridePatch {
    pub(crate) fn new(primary: ComposeOverrideServicePatch) -> Self {
        let forbidden_secret_values = primary.forbidden_secret_values();
        Self {
            services: BTreeMap::from([(primary.name.clone(), primary)]),
            networks: BTreeMap::new(),
            volumes: BTreeMap::new(),
            configs: BTreeMap::new(),
            secrets: BTreeMap::new(),
            forbidden_secret_values,
        }
    }

    pub(crate) fn service(mut self, service: ComposeOverrideServicePatch) -> Self {
        self.forbidden_secret_values
            .extend(service.forbidden_secret_values());
        self.services.insert(service.name.clone(), service);
        self
    }

    pub(crate) fn service_container_name(
        mut self,
        service_name: &str,
        container_name: &str,
        original_name: &str,
        networks: &[String],
    ) -> Self {
        let service = self
            .services
            .entry(service_name.to_owned())
            .or_insert_with(|| ComposeOverrideServicePatch::new(service_name));
        service.container_name = Some(container_name.to_owned());
        for network in networks {
            service
                .network_aliases
                .entry(network.clone())
                .or_default()
                .insert(original_name.to_owned());
        }
        self
    }

    pub(crate) fn service_environment(
        mut self,
        service_name: &str,
        key: &str,
        value: &str,
    ) -> Self {
        let service = self
            .services
            .entry(service_name.to_owned())
            .or_insert_with(|| ComposeOverrideServicePatch::new(service_name));
        service.environment.insert(
            key.to_owned(),
            ComposeOverrideEnvironmentValue::Literal(value.to_owned()),
        );
        self
    }

    pub(crate) fn network_name(mut self, resource: &str, name: &str) -> Self {
        self.networks.entry(resource.to_owned()).or_default().name = Some(name.to_owned());
        self
    }

    pub(crate) fn network_ipam_override(
        mut self,
        resource: &str,
        config: ComposeOverrideNetworkIpamConfig,
    ) -> Self {
        self.networks
            .entry(resource.to_owned())
            .or_default()
            .ipam_config_override = vec![config];
        self
    }

    pub(crate) fn volume_name(mut self, resource: &str, name: &str) -> Self {
        self.volumes.insert(resource.to_owned(), name.to_owned());
        self
    }

    pub(crate) fn config_name(mut self, resource: &str, name: &str) -> Self {
        self.configs.insert(resource.to_owned(), name.to_owned());
        self
    }

    pub(crate) fn secret_name(mut self, resource: &str, name: &str) -> Self {
        self.secrets.insert(resource.to_owned(), name.to_owned());
        self
    }

    pub(crate) fn to_yaml(&self) -> Result<String> {
        let mut content = String::new();
        content.push_str("services:\n");
        for (service_name, service) in &self.services {
            append_indent(&mut content, 2);
            content.push_str(&yaml_quote(service_name));
            content.push_str(":\n");
            service.append_yaml(&mut content);
        }
        append_yaml_networks(&mut content, &self.networks);
        append_yaml_named_resources(&mut content, "volumes", &self.volumes);
        append_yaml_named_resources(&mut content, "configs", &self.configs);
        append_yaml_named_resources(&mut content, "secrets", &self.secrets);
        self.ensure_no_forbidden_secret_values(&content)?;
        Ok(content)
    }

    fn ensure_no_forbidden_secret_values(&self, content: &str) -> Result<()> {
        for secret in self
            .forbidden_secret_values
            .iter()
            .filter(|secret| !secret.is_empty())
        {
            if content.contains(secret) {
                bail!("Generated Docker Compose override contains a forbidden secret value");
            }
        }
        Ok(())
    }
}

impl ComposeOverrideServicePatch {
    pub(crate) fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            image: None,
            container_name: None,
            pull_policy: None,
            labels: BTreeMap::new(),
            environment: BTreeMap::new(),
            user: None,
            init: None,
            privileged: None,
            cap_add: Vec::new(),
            security_opt: Vec::new(),
            mounts: Vec::new(),
            network_aliases: BTreeMap::new(),
            ports_override: Vec::new(),
            entrypoint: Vec::new(),
            command: Vec::new(),
            forbidden_secret_values: Vec::new(),
        }
    }

    pub(crate) fn image(mut self, image: impl Into<String>) -> Self {
        self.image = Some(image.into());
        self
    }

    pub(crate) fn pull_policy_never(mut self) -> Self {
        self.pull_policy = Some("never".to_owned());
        self
    }

    #[cfg(test)]
    pub(crate) fn label(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        let key = key.into();
        if !key.starts_with("com.docker.compose.") {
            self.labels.insert(key, value.into());
        }
        self
    }

    pub(crate) fn labels(mut self, labels: &BTreeMap<String, String>) -> Self {
        for (key, value) in labels {
            if !key.starts_with("com.docker.compose.") {
                self.labels.insert(key.clone(), value.clone());
            }
        }
        self
    }

    pub(crate) fn environment(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.environment.insert(
            key.into(),
            ComposeOverrideEnvironmentValue::Literal(value.into()),
        );
        self
    }

    pub(crate) fn interpolated_environment(
        mut self,
        key: impl Into<String>,
        placeholder: impl Into<String>,
        redactions: Vec<String>,
    ) -> Self {
        self.environment.insert(
            key.into(),
            ComposeOverrideEnvironmentValue::Interpolated {
                placeholder: placeholder.into(),
                redactions,
            },
        );
        self
    }

    pub(crate) fn user(mut self, user: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self
    }

    pub(crate) const fn init(mut self, init: bool) -> Self {
        self.init = Some(init);
        self
    }

    pub(crate) const fn privileged(mut self, privileged: bool) -> Self {
        self.privileged = Some(privileged);
        self
    }

    pub(crate) fn cap_add(mut self, cap_add: &[String]) -> Self {
        self.cap_add.extend(cap_add.iter().cloned());
        self
    }

    pub(crate) fn security_opt(mut self, security_opt: &[String]) -> Self {
        self.security_opt.extend(security_opt.iter().cloned());
        self
    }

    pub(crate) fn mount(mut self, mount: ComposeOverrideMount) -> Self {
        self.mounts.push(mount);
        self
    }

    pub(crate) fn mounts(mut self, mounts: &[DockerMountSpec]) -> Self {
        self.mounts
            .extend(mounts.iter().cloned().map(ComposeOverrideMount::from));
        self
    }

    pub(crate) fn ports_override(mut self, ports: Vec<ComposeOverridePortEntry>) -> Self {
        self.ports_override = ports;
        self
    }

    pub(crate) fn entrypoint(mut self, entrypoint: Vec<String>) -> Self {
        self.entrypoint = entrypoint;
        self
    }

    pub(crate) fn command(mut self, command: Vec<String>) -> Self {
        self.command = command;
        self
    }

    #[cfg(test)]
    pub(crate) fn keepalive_command(mut self, enabled: bool) -> Self {
        if enabled {
            self.command = vec!["sleep".to_owned(), "infinity".to_owned()];
        }
        self
    }

    #[cfg(test)]
    pub(crate) fn secret_value_forbidden(mut self, value: impl Into<String>) -> Self {
        self.forbidden_secret_values.push(value.into());
        self
    }

    fn forbidden_secret_values(&self) -> Vec<String> {
        let mut values = self.forbidden_secret_values.clone();
        for value in self.environment.values() {
            if let ComposeOverrideEnvironmentValue::Interpolated { redactions, .. } = value {
                values.extend(redactions.clone());
            }
        }
        values
    }

    fn append_yaml(&self, content: &mut String) {
        if let Some(image) = &self.image {
            append_yaml_scalar(content, 4, "image", image);
        }
        if let Some(container_name) = &self.container_name {
            append_yaml_scalar(content, 4, "container_name", container_name);
        }
        if let Some(pull_policy) = &self.pull_policy {
            append_yaml_scalar(content, 4, "pull_policy", pull_policy);
        }
        append_yaml_map(content, 4, "labels", &self.labels);
        append_yaml_environment(content, 4, &self.environment);
        if let Some(user) = &self.user {
            append_yaml_scalar(content, 4, "user", user);
        }
        if let Some(init) = self.init {
            append_yaml_bool(content, 4, "init", init);
        }
        if let Some(privileged) = self.privileged {
            append_yaml_bool(content, 4, "privileged", privileged);
        }
        append_yaml_string_list(content, 4, "cap_add", &self.cap_add);
        append_yaml_string_list(content, 4, "security_opt", &self.security_opt);
        append_yaml_mounts(content, 4, &self.mounts);
        append_yaml_network_aliases(content, 4, &self.network_aliases);
        append_yaml_ports_override(content, 4, &self.ports_override);
        append_yaml_string_list(content, 4, "entrypoint", &self.entrypoint);
        append_yaml_string_list(content, 4, "command", &self.command);
    }
}

impl ComposeOverrideMount {
    #[cfg(test)]
    pub(crate) fn bind(
        source: impl Into<String>,
        target: impl Into<String>,
        read_only: bool,
    ) -> Self {
        Self {
            source: Some(source.into()),
            target: target.into(),
            mount_type: MountType::Bind,
            read_only,
            consistency: None,
            bind_options: None,
            volume_options: None,
        }
    }
}

impl From<DockerMountSpec> for ComposeOverrideMount {
    fn from(mount: DockerMountSpec) -> Self {
        Self {
            source: mount.source,
            target: mount.target,
            mount_type: mount.mount_type,
            read_only: mount.read_only,
            consistency: mount.consistency,
            bind_options: mount.bind_options,
            volume_options: mount.volume_options,
        }
    }
}

pub(crate) fn write_compose_override(
    output_path: &Path,
    override_patch: &ComposeOverridePatch,
) -> Result<()> {
    let content = override_patch.to_yaml()?;
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "Failed to create Docker Compose generated override directory: {}",
                parent.display()
            )
        })?;
    }
    let temporary_path = output_path.with_extension("yaml.tmp");
    fs::write(&temporary_path, content).with_context(|| {
        format!(
            "Failed to write temporary Docker Compose generated override file: {}",
            temporary_path.display()
        )
    })?;
    fs::rename(&temporary_path, output_path).with_context(|| {
        format!(
            "Failed to replace Docker Compose generated override file: {}",
            output_path.display()
        )
    })
}

fn append_yaml_scalar(content: &mut String, indent: usize, key: &str, value: &str) {
    append_indent(content, indent);
    content.push_str(key);
    content.push_str(": ");
    content.push_str(&yaml_quote(value));
    content.push('\n');
}

fn append_yaml_bool(content: &mut String, indent: usize, key: &str, value: bool) {
    append_indent(content, indent);
    content.push_str(key);
    content.push_str(": ");
    content.push_str(if value { "true" } else { "false" });
    content.push('\n');
}

fn append_yaml_map(
    content: &mut String,
    indent: usize,
    key: &str,
    values: &BTreeMap<String, String>,
) {
    if values.is_empty() {
        return;
    }
    append_indent(content, indent);
    content.push_str(key);
    content.push_str(":\n");
    for (name, value) in values {
        append_indent(content, indent + 2);
        content.push_str(&yaml_quote(name));
        content.push_str(": ");
        content.push_str(&yaml_quote(value));
        content.push('\n');
    }
}

fn append_yaml_named_resources(
    content: &mut String,
    section: &str,
    resources: &BTreeMap<String, String>,
) {
    if resources.is_empty() {
        return;
    }
    content.push_str(section);
    content.push_str(":\n");
    for (resource, name) in resources {
        append_indent(content, 2);
        content.push_str(&yaml_quote(resource));
        content.push_str(":\n");
        append_yaml_scalar(content, 4, "name", name);
    }
}

fn append_yaml_networks(
    content: &mut String,
    networks: &BTreeMap<String, ComposeOverrideNetworkPatch>,
) {
    if networks.is_empty() {
        return;
    }
    content.push_str("networks:\n");
    for (resource, patch) in networks {
        append_indent(content, 2);
        content.push_str(&yaml_quote(resource));
        content.push_str(":\n");
        if let Some(name) = &patch.name {
            append_yaml_scalar(content, 4, "name", name);
        }
        if patch.ipam_config_override.is_empty() {
            continue;
        }
        append_indent(content, 4);
        content.push_str("ipam:\n");
        append_indent(content, 6);
        content.push_str("config: !override\n");
        for config in &patch.ipam_config_override {
            append_indent(content, 8);
            content.push_str("- subnet: ");
            content.push_str(&yaml_quote(&config.subnet));
            content.push('\n');
            if let Some(gateway) = &config.gateway {
                append_yaml_scalar(content, 10, "gateway", gateway);
            }
        }
    }
}

fn append_yaml_network_aliases(
    content: &mut String,
    indent: usize,
    networks: &BTreeMap<String, BTreeSet<String>>,
) {
    if networks.is_empty() {
        return;
    }
    append_indent(content, indent);
    content.push_str("networks:\n");
    for (network, aliases) in networks {
        append_indent(content, indent + 2);
        content.push_str(&yaml_quote(network));
        content.push_str(":\n");
        append_indent(content, indent + 4);
        content.push_str("aliases:\n");
        for alias in aliases {
            append_indent(content, indent + 6);
            content.push_str("- ");
            content.push_str(&yaml_quote(alias));
            content.push('\n');
        }
    }
}

fn append_yaml_environment(
    content: &mut String,
    indent: usize,
    values: &BTreeMap<String, ComposeOverrideEnvironmentValue>,
) {
    if values.is_empty() {
        return;
    }
    append_indent(content, indent);
    content.push_str("environment:\n");
    for (name, value) in values {
        append_indent(content, indent + 2);
        content.push_str(&yaml_quote(name));
        content.push_str(": ");
        match value {
            ComposeOverrideEnvironmentValue::Literal(value) => {
                content.push_str(&yaml_quote(value));
            }
            ComposeOverrideEnvironmentValue::Interpolated { placeholder, .. } => {
                content.push_str(&yaml_quote(&format!("${{{placeholder}}}")));
            }
        }
        content.push('\n');
    }
}

fn append_yaml_string_list(content: &mut String, indent: usize, key: &str, values: &[String]) {
    if values.is_empty() {
        return;
    }
    append_indent(content, indent);
    content.push_str(key);
    content.push_str(":\n");
    for value in values {
        append_indent(content, indent + 2);
        content.push_str("- ");
        content.push_str(&yaml_quote(value));
        content.push('\n');
    }
}

fn append_yaml_mounts(content: &mut String, indent: usize, mounts: &[ComposeOverrideMount]) {
    if mounts.is_empty() {
        return;
    }
    append_indent(content, indent);
    content.push_str("volumes:\n");
    for mount in mounts {
        append_indent(content, indent + 2);
        content.push_str("- type: ");
        content.push_str(match mount.mount_type {
            MountType::Bind => "bind",
            MountType::Volume => "volume",
            MountType::Tmpfs => "tmpfs",
        });
        content.push('\n');
        if let Some(source) = &mount.source {
            append_yaml_scalar(content, indent + 4, "source", source);
        }
        append_yaml_scalar(content, indent + 4, "target", &mount.target);
        if mount.read_only {
            append_yaml_bool(content, indent + 4, "read_only", true);
        }
        if let Some(consistency) = &mount.consistency {
            append_yaml_scalar(content, indent + 4, "consistency", consistency);
        }
        match mount.mount_type {
            MountType::Bind => append_yaml_bind_mount_options(content, indent + 4, mount),
            MountType::Volume => append_yaml_volume_mount_options(content, indent + 4, mount),
            MountType::Tmpfs => {}
        }
    }
}

fn append_yaml_ports_override(
    content: &mut String,
    indent: usize,
    ports: &[ComposeOverridePortEntry],
) {
    if ports.is_empty() {
        return;
    }
    append_indent(content, indent);
    content.push_str("ports: !override\n");
    for port in ports {
        append_indent(content, indent + 2);
        content.push_str("- ");
        append_yaml_object_fields_after_prefix(content, indent + 2, port);
    }
}

fn append_yaml_object_fields_after_prefix(
    content: &mut String,
    indent: usize,
    values: &BTreeMap<String, JsonValue>,
) {
    if values.is_empty() {
        content.push_str("{}\n");
        return;
    }

    let mut fields = values.iter();
    let Some((first_key, first_value)) = fields.next() else {
        unreachable!("empty object handled above");
    };
    content.push_str(first_key);
    content.push_str(": ");
    append_yaml_json_value(content, indent + 2, first_value);
    content.push('\n');

    for (key, value) in fields {
        append_indent(content, indent + 2);
        content.push_str(key);
        content.push_str(": ");
        append_yaml_json_value(content, indent + 2, value);
        content.push('\n');
    }
}

fn append_yaml_json_value(content: &mut String, indent: usize, value: &JsonValue) {
    match value {
        JsonValue::Null => content.push_str("null"),
        JsonValue::Bool(value) => content.push_str(if *value { "true" } else { "false" }),
        JsonValue::Number(value) => content.push_str(&value.to_string()),
        JsonValue::String(value) => content.push_str(&yaml_quote(value)),
        JsonValue::Array(values) => {
            if values.is_empty() {
                content.push_str("[]");
                return;
            }
            content.push('\n');
            for value in values {
                append_indent(content, indent);
                content.push_str("- ");
                append_yaml_json_value(content, indent + 2, value);
                content.push('\n');
            }
        }
        JsonValue::Object(values) => {
            if values.is_empty() {
                content.push_str("{}");
                return;
            }
            content.push('\n');
            for (key, value) in values {
                append_indent(content, indent);
                content.push_str(key);
                content.push_str(": ");
                append_yaml_json_value(content, indent + 2, value);
                content.push('\n');
            }
        }
    }
}

fn append_yaml_bind_mount_options(
    content: &mut String,
    indent: usize,
    mount: &ComposeOverrideMount,
) {
    append_indent(content, indent);
    content.push_str("bind:\n");
    if let Some(propagation) = mount
        .bind_options
        .as_ref()
        .and_then(|options| options.propagation)
    {
        append_yaml_scalar(content, indent + 2, "propagation", propagation.as_str());
    }
    let create_host_path = mount
        .bind_options
        .as_ref()
        .and_then(|options| options.create_mountpoint)
        .unwrap_or(false);
    append_yaml_bool(content, indent + 2, "create_host_path", create_host_path);
}

fn append_yaml_volume_mount_options(
    content: &mut String,
    indent: usize,
    mount: &ComposeOverrideMount,
) {
    let Some(volume_options) = &mount.volume_options else {
        return;
    };
    if volume_options.no_copy.is_none() && volume_options.subpath.is_none() {
        return;
    }

    append_indent(content, indent);
    content.push_str("volume:\n");
    if let Some(no_copy) = volume_options.no_copy {
        append_yaml_bool(content, indent + 2, "nocopy", no_copy);
    }
    if let Some(subpath) = &volume_options.subpath {
        append_yaml_scalar(content, indent + 2, "subpath", subpath);
    }
}

fn append_indent(content: &mut String, indent: usize) {
    for _ in 0..indent {
        content.push(' ');
    }
}

fn yaml_quote(value: &str) -> String {
    if value
        .chars()
        .any(|ch| matches!(ch, '\n' | '\r') || ch.is_control())
    {
        return yaml_double_quote(value);
    }

    format!("'{}'", value.replace('\'', "''"))
}

fn yaml_double_quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for ch in value.chars() {
        match ch {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            '\u{08}' => quoted.push_str("\\b"),
            '\u{0c}' => quoted.push_str("\\f"),
            ch if ch.is_control() => {
                push_yaml_unicode_escape(&mut quoted, ch);
            }
            ch => quoted.push(ch),
        }
    }
    quoted.push('"');
    quoted
}

fn push_yaml_unicode_escape(output: &mut String, ch: char) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let value = ch as u32;
    output.push_str("\\u");
    for shift in [12, 8, 4, 0] {
        output.push(HEX[((value >> shift) & 0x0f) as usize] as char);
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs};

    use super::*;

    #[test]
    fn compose_override_yaml_patches_only_primary_service() {
        let patch = ComposeOverridePatch::new(
            ComposeOverrideServicePatch::new("app")
                .label("decune.managed", "true")
                .label("decune.workspace_id", "workspace-id")
                .environment("APP_ENV", "development")
                .user("decune")
                .mount(ComposeOverrideMount::bind(
                    "/host/cache",
                    "/workspaces/cache",
                    true,
                )),
        );

        let yaml = patch.to_yaml().unwrap();

        assert_eq!(
            yaml,
            concat!(
                "services:\n",
                "  'app':\n",
                "    labels:\n",
                "      'decune.managed': 'true'\n",
                "      'decune.workspace_id': 'workspace-id'\n",
                "    environment:\n",
                "      'APP_ENV': 'development'\n",
                "    user: 'decune'\n",
                "    volumes:\n",
                "      - type: bind\n",
                "        source: '/host/cache'\n",
                "        target: '/workspaces/cache'\n",
                "        read_only: true\n",
                "        bind:\n",
                "          create_host_path: false\n",
            )
        );
        assert!(!yaml.contains("sidecar"));
    }

    #[test]
    fn compose_override_yaml_sets_generated_image_and_pull_policy_never() {
        let patch = ComposeOverridePatch::new(
            ComposeOverrideServicePatch::new("app")
                .image("decune/workspace:hash123")
                .pull_policy_never(),
        );

        let yaml = patch.to_yaml().unwrap();

        assert!(yaml.contains("    image: 'decune/workspace:hash123'\n"));
        assert!(yaml.contains("    pull_policy: 'never'\n"));
    }

    #[test]
    fn compose_override_yaml_replaces_ports_with_override_tag() {
        let patch = ComposeOverridePatch::new(
            ComposeOverrideServicePatch::new("app").ports_override(vec![
                BTreeMap::from([
                    ("app_protocol".to_owned(), serde_json::json!("http")),
                    ("host_ip".to_owned(), serde_json::json!("127.0.0.1")),
                    ("mode".to_owned(), serde_json::json!("host")),
                    ("name".to_owned(), serde_json::json!("web")),
                    ("protocol".to_owned(), serde_json::json!("tcp")),
                    ("published".to_owned(), serde_json::json!("3001")),
                    ("target".to_owned(), serde_json::json!(3000)),
                ]),
                BTreeMap::from([
                    ("protocol".to_owned(), serde_json::json!("udp")),
                    ("published".to_owned(), serde_json::json!("8125")),
                    ("target".to_owned(), serde_json::json!(8125)),
                ]),
                BTreeMap::from([
                    ("protocol".to_owned(), serde_json::json!("tcp")),
                    ("target".to_owned(), serde_json::json!(9000)),
                ]),
            ]),
        );

        let yaml = patch.to_yaml().unwrap();

        assert_eq!(
            yaml,
            concat!(
                "services:\n",
                "  'app':\n",
                "    ports: !override\n",
                "      - app_protocol: 'http'\n",
                "        host_ip: '127.0.0.1'\n",
                "        mode: 'host'\n",
                "        name: 'web'\n",
                "        protocol: 'tcp'\n",
                "        published: '3001'\n",
                "        target: 3000\n",
                "      - protocol: 'udp'\n",
                "        published: '8125'\n",
                "        target: 8125\n",
                "      - protocol: 'tcp'\n",
                "        target: 9000\n",
            )
        );
    }

    #[test]
    fn compose_override_yaml_rewrites_fixed_names_and_preserves_container_dns_aliases() {
        let patch = ComposeOverridePatch::new(ComposeOverrideServicePatch::new("app"))
            .service_container_name(
                "app",
                "fixed-app-abc123def456",
                "fixed-app",
                &["backend".to_owned(), "frontend".to_owned()],
            )
            .network_name("backend", "fixed-backend-abc123def456")
            .volume_name("cache", "fixed-cache-abc123def456")
            .config_name("app-config", "fixed-config-abc123def456")
            .secret_name("app-secret", "fixed-secret-abc123def456");

        let yaml = patch.to_yaml().unwrap();

        assert_eq!(
            yaml,
            concat!(
                "services:\n",
                "  'app':\n",
                "    container_name: 'fixed-app-abc123def456'\n",
                "    networks:\n",
                "      'backend':\n",
                "        aliases:\n",
                "          - 'fixed-app'\n",
                "      'frontend':\n",
                "        aliases:\n",
                "          - 'fixed-app'\n",
                "networks:\n",
                "  'backend':\n",
                "    name: 'fixed-backend-abc123def456'\n",
                "volumes:\n",
                "  'cache':\n",
                "    name: 'fixed-cache-abc123def456'\n",
                "configs:\n",
                "  'app-config':\n",
                "    name: 'fixed-config-abc123def456'\n",
                "secrets:\n",
                "  'app-secret':\n",
                "    name: 'fixed-secret-abc123def456'\n",
            )
        );
    }

    #[test]
    fn compose_override_yaml_replaces_network_ipam_config_with_override_tag() {
        let patch = ComposeOverridePatch::new(ComposeOverrideServicePatch::new("app"))
            .network_name("grpc", "fixed-grpc-workspace")
            .network_ipam_override(
                "grpc",
                ComposeOverrideNetworkIpamConfig {
                    subnet: "10.200.42.0/24".to_owned(),
                    gateway: Some("10.200.42.1".to_owned()),
                },
            );

        let yaml = patch.to_yaml().unwrap();

        assert!(yaml.contains(concat!(
            "networks:\n",
            "  'grpc':\n",
            "    name: 'fixed-grpc-workspace'\n",
            "    ipam:\n",
            "      config: !override\n",
            "        - subnet: '10.200.42.0/24'\n",
            "          gateway: '10.200.42.1'\n",
        )));
    }

    #[test]
    fn compose_override_command_is_emitted_only_when_requested() {
        let keepalive = ComposeOverridePatch::new(
            ComposeOverrideServicePatch::new("app").keepalive_command(true),
        )
        .to_yaml()
        .unwrap();
        let original = ComposeOverridePatch::new(
            ComposeOverrideServicePatch::new("app").keepalive_command(false),
        )
        .to_yaml()
        .unwrap();

        assert!(keepalive.contains("    command:\n      - 'sleep'\n      - 'infinity'\n"));
        assert!(!original.contains("command:"));
    }

    #[test]
    fn compose_override_secret_leak_regression_does_not_persist_secret_literals() {
        let temp = tempfile::tempdir().unwrap();
        let override_path = temp.path().join("compose.override.yaml");
        let patch = ComposeOverridePatch::new(
            ComposeOverrideServicePatch::new("app")
                .environment("GH_TOKEN_FILE", "/run/decune/secrets/github-token")
                .mount(ComposeOverrideMount::bind(
                    "/tmp/decune/secrets/github-token",
                    "/run/decune/secrets/github-token",
                    true,
                ))
                .secret_value_forbidden("github-test-secret"),
        );

        write_compose_override(&override_path, &patch).unwrap();

        let yaml = fs::read_to_string(override_path).unwrap();
        assert!(yaml.contains("/run/decune/secrets/github-token"));
        assert!(!yaml.contains("github-test-secret"));
    }

    #[test]
    fn compose_override_rejects_forbidden_secret_values_in_named_resource_sections() {
        let patch = ComposeOverridePatch::new(
            ComposeOverrideServicePatch::new("app").secret_value_forbidden("secret-name-value"),
        )
        .volume_name("cache", "secret-name-value");

        let error = patch.to_yaml().unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Generated Docker Compose override contains a forbidden secret value")
        );
    }

    #[test]
    fn compose_override_yaml_uses_placeholder_for_interpolated_environment() {
        let patch = ComposeOverridePatch::new(
            ComposeOverrideServicePatch::new("app").interpolated_environment(
                "NPM_TOKEN",
                "DECUNE_CONTAINER_ENV_NPM_TOKEN",
                vec!["secret-token".to_owned()],
            ),
        );

        let yaml = patch.to_yaml().unwrap();

        assert!(yaml.contains("'NPM_TOKEN': '${DECUNE_CONTAINER_ENV_NPM_TOKEN}'"));
        assert!(!yaml.contains("secret-token"));
    }
}
