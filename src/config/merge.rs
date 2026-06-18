pub(crate) use crate::config::{
    layer::{ConfigLayer, ConfigMergeInput},
    resolved::ResolvedConfig,
};

use crate::config::{
    layer::{
        LayerAutoPorts, LayerCredentials, LayerDevcontainerMetadata, LayerDotfile, LayerFeature,
        LayerForwardPort, LayerMount, LayerPort, LayerPortAttributes, LayerPublishPort,
        feature_merge_identity,
    },
    resolved::{
        ResolvedAutoPorts, ResolvedCredentials, ResolvedDevcontainer, ResolvedDotfile,
        ResolvedDotfileDisable, ResolvedDotfileEntry, ResolvedFeature, ResolvedHooks,
        ResolvedMount, ResolvedPort, ResolvedPorts,
    },
};

pub(crate) fn resolve_config(input: ConfigMergeInput) -> ResolvedConfig {
    let mut accumulator = MergeAccumulator::default();

    for layer in input.image_metadata {
        accumulator.apply_layer(layer, PortSourcePriority::ImageMetadata);
    }

    for layer in input.feature_metadata {
        accumulator.apply_layer(layer, PortSourcePriority::ImageMetadata);
    }

    for (layer, source_priority) in [
        (input.global, PortSourcePriority::Global),
        (input.devcontainer, PortSourcePriority::Devcontainer),
        (input.project, PortSourcePriority::Project),
        (input.cli, PortSourcePriority::Cli),
    ] {
        if let Some(layer) = layer {
            accumulator.apply_layer(layer, source_priority);
        }
    }

    accumulator.into_resolved()
}

#[cfg(test)]
pub(crate) fn merge_feature_metadata_layers(
    mut config: ResolvedConfig,
    layers: Vec<ConfigLayer>,
) -> ResolvedConfig {
    for layer in layers {
        if let Some(devcontainer) = layer.devcontainer {
            merge_feature_devcontainer_metadata_into_resolved(
                &mut config.devcontainer,
                devcontainer,
            );
        }
        config.hooks.append(layer.hooks);
    }

    config
}

#[cfg(test)]
fn merge_feature_devcontainer_metadata_into_resolved(
    target: &mut ResolvedDevcontainer,
    devcontainer: LayerDevcontainerMetadata,
) {
    target.mounts.splice(0..0, devcontainer.mounts);
    for (key, value) in devcontainer.container_env {
        target.container_env.entry(key).or_insert(value);
    }
    for (key, value) in devcontainer.remote_env {
        target.remote_env.entry(key).or_insert(value);
    }
    if target.remote_user.is_none() {
        target.remote_user = devcontainer.remote_user;
    }
    if target.container_user.is_none() {
        target.container_user = devcontainer.container_user;
    }
    for port in devcontainer.publish_ports {
        if !target
            .publish_ports
            .iter()
            .any(|existing| same_publish_port_identity(existing, &port))
        {
            target.publish_ports.insert(0, port);
        }
    }
    for (key, value) in devcontainer.port_attributes {
        target.port_attributes.entry(key).or_insert(value);
    }
    if target.other_ports_attributes.is_none() {
        target.other_ports_attributes = devcontainer.other_ports_attributes;
    }
    target.run_args.splice(0..0, devcontainer.run_args);
    if let Some(init) = devcontainer.init {
        target.init = init;
    }
    if let Some(privileged) = devcontainer.privileged {
        target.privileged = privileged;
    }
    append_unique(&mut target.cap_add, devcontainer.cap_add);
    append_unique(&mut target.security_opt, devcontainer.security_opt);
    target.entrypoints.splice(0..0, devcontainer.entrypoints);
    if let Some(lifecycle) = devcontainer.lifecycle {
        match target.lifecycle.take() {
            Some(existing) => {
                let mut merged = lifecycle.into_resolved();
                if let Some(existing_layer) = existing.into_layer() {
                    merged.merge_layer(existing_layer);
                }
                target.lifecycle = Some(merged);
            }
            None => target.lifecycle = Some(lifecycle.into_resolved()),
        }
    }
}

#[derive(Debug, Default)]
struct MergeAccumulator {
    shell: Option<String>,
    features: Vec<ResolvedFeature>,
    dotfile_entries: Vec<ResolvedDotfileEntry>,
    dotfiles: Vec<ResolvedDotfile>,
    mounts: Vec<ResolvedMount>,
    ports: Vec<MergedPort>,
    auto_ports: ResolvedAutoPorts,
    devcontainer: ResolvedDevcontainer,
    devcontainer_init: Option<bool>,
    devcontainer_privileged: Option<bool>,
    credentials: ResolvedCredentials,
    hooks: ResolvedHooks,
}

#[derive(Debug)]
struct MergedPort {
    port: ResolvedPort,
    forward_attribute_keys: Vec<String>,
    source_priority: PortSourcePriority,
}

impl MergedPort {
    fn plain(port: LayerPort, source_priority: PortSourcePriority) -> Self {
        Self {
            port,
            forward_attribute_keys: Vec::new(),
            source_priority,
        }
    }

    fn forward(port: LayerForwardPort, source_priority: PortSourcePriority) -> Self {
        Self {
            port: port.port,
            forward_attribute_keys: port.attribute_keys,
            source_priority,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum PortSourcePriority {
    ImageMetadata,
    Global,
    Devcontainer,
    Project,
    Cli,
}

impl MergeAccumulator {
    fn apply_layer(&mut self, layer: ConfigLayer, source_priority: PortSourcePriority) {
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
            self.merge_port(port, source_priority);
        }

        for port in layer.forward_ports {
            self.merge_forward_port(port, source_priority);
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
        if feature.enabled {
            let merge_identity = feature_merge_identity(&feature.id);
            if let Some(position) = self
                .features
                .iter()
                .position(|entry| feature_merge_identity(&entry.id) == merge_identity)
            {
                let existing = &mut self.features[position];
                existing.id = feature.id;
                existing.options.extend(feature.options);
                return;
            }

            self.features.push(ResolvedFeature {
                id: feature.id,
                canonical_id: feature.canonical_id,
                options: feature.options,
            });
            return;
        }

        remove_by_identity(&mut self.features, |existing| {
            existing.canonical_id == feature.canonical_id
        });
    }

    fn merge_dotfile(&mut self, dotfile: LayerDotfile) {
        if !dotfile.enabled {
            remove_by_identity(&mut self.dotfiles, |existing| {
                existing.target == dotfile.target
            });
            replace_dotfile_entry_by_target(
                &mut self.dotfile_entries,
                ResolvedDotfileEntry::Disabled(ResolvedDotfileDisable {
                    target: dotfile.target,
                    origin: dotfile.origin,
                }),
            );
            return;
        }

        if let Some(source) = dotfile.source {
            let resolved = ResolvedDotfile {
                source,
                target: dotfile.target,
                read_only: dotfile.read_only,
                resolve_symlink: dotfile.resolve_symlink,
                on_conflict: dotfile.on_conflict,
                origin: dotfile.origin,
            };
            replace_by_identity(&mut self.dotfiles, resolved.clone(), |left, right| {
                left.target == right.target
            });
            replace_dotfile_entry_by_target(
                &mut self.dotfile_entries,
                ResolvedDotfileEntry::Enabled(resolved),
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
                    origin: mount.origin,
                },
                |left, right| left.target == right.target,
            );
        }
    }

    fn merge_port(&mut self, port: LayerPort, source_priority: PortSourcePriority) {
        if !port.enabled {
            remove_by_identity(&mut self.ports, |existing| {
                existing.port.protocol == port.protocol
                    && existing.port.service == port.service
                    && existing.port.container == port.container
                    && existing.port.host_ip == port.host_ip
            });
            return;
        }

        replace_by_identity(
            &mut self.ports,
            MergedPort::plain(port, source_priority),
            same_merged_port_identity,
        );
    }

    fn merge_forward_port(&mut self, port: LayerForwardPort, source_priority: PortSourcePriority) {
        replace_by_identity(
            &mut self.ports,
            MergedPort::forward(port, source_priority),
            same_merged_port_identity,
        );
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
            self.devcontainer.update_remote_user_uid = update_remote_user_uid;
        }
        if let Some(override_command) = devcontainer.override_command {
            self.devcontainer.override_command = override_command;
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
            self.devcontainer_init = Some(init);
        }
        if let Some(privileged) = devcontainer.privileged {
            self.devcontainer_privileged = Some(privileged);
        }
        append_unique(&mut self.devcontainer.cap_add, devcontainer.cap_add);
        append_unique(
            &mut self.devcontainer.security_opt,
            devcontainer.security_opt,
        );
        self.devcontainer
            .entrypoints
            .extend(devcontainer.entrypoints);
        if let Some(shutdown_action) = devcontainer.shutdown_action {
            self.devcontainer.shutdown_action = shutdown_action;
        }
        if let Some(lifecycle) = devcontainer.lifecycle {
            match &mut self.devcontainer.lifecycle {
                Some(target) => target.merge_layer(lifecycle),
                None => self.devcontainer.lifecycle = Some(lifecycle.into_resolved()),
            }
        }
    }

    fn into_resolved(mut self) -> ResolvedConfig {
        self.apply_forward_port_attributes();
        self.devcontainer.init = self.devcontainer_init.unwrap_or(false);
        self.devcontainer.privileged = self.devcontainer_privileged.unwrap_or(false);
        self.ports
            .sort_by_key(|entry| std::cmp::Reverse(entry.source_priority));

        ResolvedConfig {
            shell: self.shell,
            features: self.features,
            dotfile_entries: self.dotfile_entries,
            dotfiles: self.dotfiles,
            mounts: self.mounts,
            ports: ResolvedPorts {
                entries: self.ports.into_iter().map(|entry| entry.port).collect(),
                auto: self.auto_ports,
            },
            devcontainer: self.devcontainer,
            credentials: self.credentials,
            hooks: self.hooks,
        }
    }

    fn apply_forward_port_attributes(&mut self) {
        for entry in &mut self.ports {
            if entry.forward_attribute_keys.is_empty() {
                continue;
            }

            let attributes = attributes_for_keys(
                &self.devcontainer.port_attributes,
                &entry.forward_attribute_keys,
            );
            entry.port.label = attributes.and_then(|attributes| attributes.label.clone());
            entry.port.require_local = attributes
                .and_then(|attributes| attributes.require_local_port)
                .unwrap_or(false);
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

fn append_unique(target: &mut Vec<String>, values: Vec<String>) {
    for value in values {
        if !target.iter().any(|existing| existing == &value) {
            target.push(value);
        }
    }
}

fn replace_dotfile_entry_by_target(
    entries: &mut Vec<ResolvedDotfileEntry>,
    entry: ResolvedDotfileEntry,
) {
    let target = dotfile_entry_target(&entry);
    if let Some(position) = entries
        .iter()
        .position(|existing| dotfile_entry_target(existing) == target)
    {
        entries[position] = entry;
    } else {
        entries.push(entry);
    }
}

fn dotfile_entry_target(entry: &ResolvedDotfileEntry) -> &str {
    match entry {
        ResolvedDotfileEntry::Enabled(dotfile) => &dotfile.target,
        ResolvedDotfileEntry::Disabled(dotfile) => &dotfile.target,
    }
}

fn same_port_identity(left: &ResolvedPort, right: &ResolvedPort) -> bool {
    left.protocol == right.protocol
        && left.service == right.service
        && left.container == right.container
        && left.host_ip == right.host_ip
}

fn same_merged_port_identity(left: &MergedPort, right: &MergedPort) -> bool {
    same_port_identity(&left.port, &right.port)
}

fn attributes_for_keys<'a>(
    attributes: &'a std::collections::BTreeMap<String, LayerPortAttributes>,
    keys: &[String],
) -> Option<&'a LayerPortAttributes> {
    keys.iter().find_map(|key| attributes.get(key))
}

fn same_publish_port_identity(left: &LayerPublishPort, right: &LayerPublishPort) -> bool {
    left.protocol == right.protocol
        && left.container == right.container
        && left.host == right.host
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
            if source.unsupported_protocol.is_some() {
                target.unsupported_protocol = source.unsupported_protocol;
            }
            if source.unsupported_elevate_if_needed.is_some() {
                target.unsupported_elevate_if_needed = source.unsupported_elevate_if_needed;
            }
        }
        None => *target = Some(source),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::config::{
        layer::{LayerDevcontainerMetadata, LayerHook, LayerPublishPort, canonical_feature_id},
        schema::RawDecuneConfig,
        types::{
            Command, DEFAULT_PORT_HOST_IP, DotfileConflict, GitHttpsMode, GithubCredentialsMode,
            OnAutoForward, PortProtocol, SshAgentMode,
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
        assert!(config.devcontainer.update_remote_user_uid);
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
            image_metadata: vec![raw_layer(
                r#"
version = 1

[[hooks.before_initialize]]
command = "image.sh"
"#,
            )],
            feature_metadata: Vec::new(),
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
    fn multiple_image_metadata_layers_keep_label_order_before_global_config() {
        let config = resolve_config(ConfigMergeInput {
            image_metadata: vec![
                ConfigLayer {
                    devcontainer: Some(LayerDevcontainerMetadata {
                        remote_user: Some("first-image-user".to_owned()),
                        ..LayerDevcontainerMetadata::default()
                    }),
                    ..ConfigLayer::default()
                },
                ConfigLayer {
                    devcontainer: Some(LayerDevcontainerMetadata {
                        remote_user: Some("second-image-user".to_owned()),
                        remote_env: [("FROM_IMAGE".to_owned(), "1".to_owned())].into(),
                        ..LayerDevcontainerMetadata::default()
                    }),
                    ..ConfigLayer::default()
                },
            ],
            global: Some(ConfigLayer {
                devcontainer: Some(LayerDevcontainerMetadata {
                    remote_env: [("FROM_GLOBAL".to_owned(), "1".to_owned())].into(),
                    ..LayerDevcontainerMetadata::default()
                }),
                ..ConfigLayer::default()
            }),
            ..ConfigMergeInput::default()
        });

        assert_eq!(
            config.devcontainer.remote_user.as_deref(),
            Some("second-image-user")
        );
        assert_eq!(
            config
                .devcontainer
                .remote_env
                .get("FROM_IMAGE")
                .map(String::as_str),
            Some("1")
        );
        assert_eq!(
            config
                .devcontainer
                .remote_env
                .get("FROM_GLOBAL")
                .map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn feature_metadata_layer_merges_container_env_and_changes_config_hash() {
        let baseline = resolve_config(ConfigMergeInput {
            devcontainer: Some(ConfigLayer {
                devcontainer: Some(LayerDevcontainerMetadata {
                    container_env: [("FROM_DEVCONTAINER".to_owned(), "1".to_owned())].into(),
                    ..LayerDevcontainerMetadata::default()
                }),
                ..ConfigLayer::default()
            }),
            ..ConfigMergeInput::default()
        });
        let merged = merge_feature_metadata_layers(
            baseline.clone(),
            vec![ConfigLayer {
                devcontainer: Some(LayerDevcontainerMetadata {
                    container_env: [("FROM_FEATURE".to_owned(), "1".to_owned())].into(),
                    remote_user: Some("feature-user".to_owned()),
                    ..LayerDevcontainerMetadata::default()
                }),
                ..ConfigLayer::default()
            }],
        );

        assert_eq!(
            merged
                .devcontainer
                .container_env
                .get("FROM_DEVCONTAINER")
                .map(String::as_str),
            Some("1")
        );
        assert_eq!(
            merged
                .devcontainer
                .container_env
                .get("FROM_FEATURE")
                .map(String::as_str),
            Some("1")
        );
        assert_eq!(
            merged.devcontainer.remote_user.as_deref(),
            Some("feature-user")
        );
        assert_ne!(
            crate::config::config_hash(&crate::config::ConfigHashInput::new(&baseline)),
            crate::config::config_hash(&crate::config::ConfigHashInput::new(&merged))
        );
    }

    #[test]
    fn feature_metadata_layer_does_not_override_user_metadata_values() {
        let baseline = resolve_config(ConfigMergeInput {
            devcontainer: Some(ConfigLayer {
                devcontainer: Some(LayerDevcontainerMetadata {
                    container_env: [("SHARED".to_owned(), "from-user".to_owned())].into(),
                    remote_user: Some("user-from-devcontainer".to_owned()),
                    lifecycle: crate::devcontainer::lifecycle::parse_lifecycle_layer_definition(
                        &BTreeMap::from([(
                            crate::devcontainer::metadata::LifecycleProperty::PostStartCommand,
                            serde_json::json!("user-post-start"),
                        )]),
                    )
                    .unwrap(),
                    ..LayerDevcontainerMetadata::default()
                }),
                ..ConfigLayer::default()
            }),
            ..ConfigMergeInput::default()
        });
        let merged = merge_feature_metadata_layers(
            baseline,
            vec![ConfigLayer {
                devcontainer: Some(LayerDevcontainerMetadata {
                    container_env: [
                        ("SHARED".to_owned(), "from-feature".to_owned()),
                        ("FEATURE_ONLY".to_owned(), "1".to_owned()),
                    ]
                    .into(),
                    remote_user: Some("feature-user".to_owned()),
                    lifecycle: crate::devcontainer::lifecycle::parse_lifecycle_layer_definition(
                        &BTreeMap::from([(
                            crate::devcontainer::metadata::LifecycleProperty::PostStartCommand,
                            serde_json::json!("feature-post-start"),
                        )]),
                    )
                    .unwrap(),
                    ..LayerDevcontainerMetadata::default()
                }),
                ..ConfigLayer::default()
            }],
        );

        assert_eq!(
            merged
                .devcontainer
                .container_env
                .get("SHARED")
                .map(String::as_str),
            Some("from-user")
        );
        assert_eq!(
            merged
                .devcontainer
                .container_env
                .get("FEATURE_ONLY")
                .map(String::as_str),
            Some("1")
        );
        assert_eq!(
            merged.devcontainer.remote_user.as_deref(),
            Some("user-from-devcontainer")
        );
        assert_eq!(
            merged
                .devcontainer
                .lifecycle
                .as_ref()
                .unwrap()
                .commands(crate::devcontainer::lifecycle::LifecycleStage::PostStart),
            &[
                crate::devcontainer::lifecycle::LifecycleCommand::Shell(
                    "feature-post-start".to_owned()
                ),
                crate::devcontainer::lifecycle::LifecycleCommand::Shell(
                    "user-post-start".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn devcontainer_security_booleans_use_layer_precedence_and_lists_use_deduped_union() {
        let config = resolve_config(ConfigMergeInput {
            image_metadata: vec![ConfigLayer {
                devcontainer: Some(LayerDevcontainerMetadata {
                    init: Some(true),
                    privileged: Some(true),
                    cap_add: vec!["SYS_PTRACE".to_owned(), "SYS_ADMIN".to_owned()],
                    security_opt: vec!["seccomp=unconfined".to_owned()],
                    ..LayerDevcontainerMetadata::default()
                }),
                ..ConfigLayer::default()
            }],
            feature_metadata: vec![ConfigLayer {
                devcontainer: Some(LayerDevcontainerMetadata {
                    init: Some(false),
                    privileged: Some(false),
                    cap_add: vec!["SYS_PTRACE".to_owned(), "NET_ADMIN".to_owned()],
                    security_opt: vec!["seccomp=unconfined".to_owned(), "label=disable".to_owned()],
                    ..LayerDevcontainerMetadata::default()
                }),
                ..ConfigLayer::default()
            }],
            devcontainer: Some(ConfigLayer {
                devcontainer: Some(LayerDevcontainerMetadata {
                    init: Some(false),
                    privileged: Some(false),
                    cap_add: vec!["NET_ADMIN".to_owned()],
                    security_opt: vec!["label=disable".to_owned()],
                    ..LayerDevcontainerMetadata::default()
                }),
                ..ConfigLayer::default()
            }),
            ..ConfigMergeInput::default()
        });

        assert!(!config.devcontainer.init);
        assert!(!config.devcontainer.privileged);
        assert_eq!(
            config.devcontainer.cap_add,
            vec!["SYS_PTRACE", "SYS_ADMIN", "NET_ADMIN"]
        );
        assert_eq!(
            config.devcontainer.security_opt,
            vec!["seccomp=unconfined", "label=disable"]
        );
    }

    #[test]
    fn devcontainer_security_booleans_keep_lower_layer_value_when_unspecified() {
        let config = resolve_config(ConfigMergeInput {
            image_metadata: vec![ConfigLayer {
                devcontainer: Some(LayerDevcontainerMetadata {
                    init: Some(true),
                    privileged: Some(true),
                    ..LayerDevcontainerMetadata::default()
                }),
                ..ConfigLayer::default()
            }],
            devcontainer: Some(ConfigLayer {
                devcontainer: Some(LayerDevcontainerMetadata::default()),
                ..ConfigLayer::default()
            }),
            ..ConfigMergeInput::default()
        });

        assert!(config.devcontainer.init);
        assert!(config.devcontainer.privileged);
    }

    #[test]
    fn project_devcontainer_security_booleans_override_devcontainer_false() {
        let config = resolve_config(ConfigMergeInput {
            image_metadata: vec![ConfigLayer {
                devcontainer: Some(LayerDevcontainerMetadata {
                    init: Some(true),
                    privileged: Some(true),
                    ..LayerDevcontainerMetadata::default()
                }),
                ..ConfigLayer::default()
            }],
            devcontainer: Some(ConfigLayer {
                devcontainer: Some(LayerDevcontainerMetadata {
                    init: Some(false),
                    privileged: Some(false),
                    ..LayerDevcontainerMetadata::default()
                }),
                ..ConfigLayer::default()
            }),
            project: Some(ConfigLayer {
                devcontainer: Some(LayerDevcontainerMetadata {
                    init: Some(true),
                    privileged: Some(true),
                    ..LayerDevcontainerMetadata::default()
                }),
                ..ConfigLayer::default()
            }),
            ..ConfigMergeInput::default()
        });

        assert!(config.devcontainer.init);
        assert!(config.devcontainer.privileged);
    }

    #[test]
    fn devcontainer_security_booleans_default_to_false_when_unspecified() {
        let config = resolve_config(ConfigMergeInput::default());

        assert!(!config.devcontainer.init);
        assert!(!config.devcontainer.privileged);
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
    fn canonical_feature_id_normalizes_oci_feature_case() {
        assert_eq!(
            canonical_feature_id("GHCR.IO/Example/Features/Tool:1"),
            "ghcr.io/example/features/tool"
        );
        let config = resolve_config(ConfigMergeInput {
            global: Some(raw_layer(
                r#"
version = 1

[features."GHCR.IO/Example/Features/Tool:1"]
version = "global"
"#,
            )),
            project: Some(raw_layer(
                r#"
version = 1

[features."ghcr.io/example/features/tool:2"]
enabled = false
"#,
            )),
            ..ConfigMergeInput::default()
        });

        assert!(config.features.is_empty());
    }

    #[test]
    fn feature_options_merge_by_concrete_feature_ref() {
        let config = resolve_config(ConfigMergeInput {
            global: Some(raw_layer(
                r#"
version = 1

[features."ghcr.io/example/features/tool"]
version = "1"
channel = "stable"
"#,
            )),
            project: Some(raw_layer(
                r#"
version = 1

[features."ghcr.io/example/features/tool:latest"]
version = "2"
"#,
            )),
            ..ConfigMergeInput::default()
        });

        assert_eq!(config.features.len(), 1);
        assert_eq!(
            config.features[0].id,
            "ghcr.io/example/features/tool:latest"
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
    fn feature_entries_with_distinct_tags_are_preserved_across_layers() {
        let config = resolve_config(ConfigMergeInput {
            global: Some(raw_layer(
                r#"
version = 1

[features."ghcr.io/example/features/tool:1"]
version = "one"
"#,
            )),
            project: Some(raw_layer(
                r#"
version = 1

[features."ghcr.io/example/features/tool:2"]
version = "two"
"#,
            )),
            ..ConfigMergeInput::default()
        });

        assert_eq!(
            config
                .features
                .iter()
                .map(|feature| feature.id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "ghcr.io/example/features/tool:1",
                "ghcr.io/example/features/tool:2",
            ]
        );
    }

    #[test]
    fn feature_entries_with_distinct_digests_are_preserved_across_layers() {
        let first_digest =
            "sha256:1111111111111111111111111111111111111111111111111111111111111111";
        let second_digest =
            "sha256:2222222222222222222222222222222222222222222222222222222222222222";
        let config = resolve_config(ConfigMergeInput {
            global: Some(raw_layer(&format!(
                r#"
version = 1

[features."ghcr.io/example/features/tool@{first_digest}"]
version = "one"
"#
            ))),
            project: Some(raw_layer(&format!(
                r#"
version = 1

[features."ghcr.io/example/features/tool@{second_digest}"]
version = "two"
"#
            ))),
            ..ConfigMergeInput::default()
        });

        assert_eq!(
            config
                .features
                .iter()
                .map(|feature| feature.id.as_str())
                .collect::<Vec<_>>(),
            vec![
                format!("ghcr.io/example/features/tool@{first_digest}"),
                format!("ghcr.io/example/features/tool@{second_digest}"),
            ]
        );
    }

    #[test]
    fn disabled_feature_removes_all_concrete_refs_by_canonical_id() {
        let config = resolve_config(ConfigMergeInput {
            global: Some(raw_layer(
                r#"
version = 1

[features."ghcr.io/example/features/tool:1"]
version = "one"

[features."ghcr.io/example/features/tool:2"]
version = "two"
"#,
            )),
            project: Some(raw_layer(
                r#"
version = 1

[features."ghcr.io/example/features/tool"]
enabled = false
"#,
            )),
            ..ConfigMergeInput::default()
        });

        assert!(config.features.is_empty());
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
    fn publish_port_identity_keeps_distinct_host_ports_for_same_container_port() {
        let first = LayerPublishPort {
            container: 80,
            host: Some(8080),
            host_ip: None,
            protocol: PortProtocol::Tcp,
        };
        let second = LayerPublishPort {
            container: 80,
            host: Some(9090),
            host_ip: None,
            protocol: PortProtocol::Tcp,
        };

        let config = resolve_config(ConfigMergeInput {
            image_metadata: vec![ConfigLayer {
                devcontainer: Some(LayerDevcontainerMetadata {
                    publish_ports: vec![first.clone()],
                    ..LayerDevcontainerMetadata::default()
                }),
                ..ConfigLayer::default()
            }],
            devcontainer: Some(ConfigLayer {
                devcontainer: Some(LayerDevcontainerMetadata {
                    publish_ports: vec![second.clone()],
                    ..LayerDevcontainerMetadata::default()
                }),
                ..ConfigLayer::default()
            }),
            ..ConfigMergeInput::default()
        });

        assert_eq!(config.devcontainer.publish_ports, vec![first, second]);
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
            image_metadata: vec![raw_layer("version = 1\nshell = '/bin/image'\n")],
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
    fn manual_port_priority_is_cli_project_devcontainer_global() {
        let devcontainer = crate::devcontainer::metadata::parse_metadata(serde_json::json!({
            "image": "ubuntu:24.04",
            "forwardPorts": [3000]
        }))
        .unwrap()
        .to_config_layer()
        .unwrap();

        let config = resolve_config(ConfigMergeInput {
            global: Some(raw_layer(
                r#"
version = 1

[[ports]]
container = 3000
host = 4000
label = "global"
"#,
            )),
            devcontainer: Some(devcontainer),
            project: Some(raw_layer(
                r#"
version = 1

[[ports]]
container = 3000
host = 5000
label = "project"
"#,
            )),
            cli: Some(ConfigLayer {
                ports: vec![LayerPort {
                    enabled: true,
                    service: None,
                    container: 3000,
                    host: Some(6000),
                    host_ip: DEFAULT_PORT_HOST_IP.to_owned(),
                    protocol: PortProtocol::Tcp,
                    require_local: false,
                    label: None,
                }],
                ..ConfigLayer::default()
            }),
            ..ConfigMergeInput::default()
        });

        assert_eq!(config.ports.entries.len(), 1);
        assert_eq!(config.ports.entries[0].host, Some(6000));
        assert_eq!(config.ports.entries[0].label, None);
    }

    #[test]
    fn manual_port_identity_includes_compose_service() {
        let config = resolve_config(ConfigMergeInput {
            project: Some(raw_layer(
                r#"
version = 1

[[ports]]
service = "app"
container = 5432
host = 15432
label = "app"

[[ports]]
service = "db"
container = 5432
host = 25432
label = "db"
"#,
            )),
            ..ConfigMergeInput::default()
        });

        assert_eq!(config.ports.entries.len(), 2);
        assert_eq!(config.ports.entries[0].service.as_deref(), Some("app"));
        assert_eq!(config.ports.entries[0].label.as_deref(), Some("app"));
        assert_eq!(config.ports.entries[1].service.as_deref(), Some("db"));
        assert_eq!(config.ports.entries[1].label.as_deref(), Some("db"));
    }

    #[test]
    fn manual_host_port_conflicts_are_resolved_by_source_priority() {
        let devcontainer = crate::devcontainer::metadata::parse_metadata(serde_json::json!({
            "image": "ubuntu:24.04",
            "forwardPorts": [3002]
        }))
        .unwrap()
        .to_config_layer()
        .unwrap();

        let config = resolve_config(ConfigMergeInput {
            global: Some(raw_layer(
                r#"
version = 1

[[ports]]
container = 3000
host = 8080
"#,
            )),
            devcontainer: Some(devcontainer),
            project: Some(raw_layer(
                r#"
version = 1

[[ports]]
container = 3001
host = 8080
"#,
            )),
            cli: Some(ConfigLayer {
                ports: vec![LayerPort {
                    enabled: true,
                    service: None,
                    container: 3003,
                    host: Some(8080),
                    host_ip: DEFAULT_PORT_HOST_IP.to_owned(),
                    protocol: PortProtocol::Tcp,
                    require_local: false,
                    label: None,
                }],
                ..ConfigLayer::default()
            }),
            ..ConfigMergeInput::default()
        });

        let resolved =
            crate::docker::ports::resolve_forward_ports_with(&config.ports.entries, |_, _| {
                Ok(true)
            })
            .unwrap();
        let ports = resolved
            .iter()
            .map(|port| (port.container, port.host))
            .collect::<Vec<_>>();

        assert_eq!(
            ports,
            vec![(3003, 8080), (3001, 8081), (3002, 3002), (3000, 8082)]
        );
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
