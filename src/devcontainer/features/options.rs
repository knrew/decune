use std::collections::BTreeMap;

use anyhow::{Result, bail};

use super::metadata::validate_feature_option_schema;
use super::{FeatureMetadata, FeatureOptionSchema};

pub(crate) fn feature_option_env(
    feature: &crate::config::resolved::ResolvedFeature,
    metadata: &FeatureMetadata,
) -> Result<BTreeMap<String, String>> {
    let mut env = BTreeMap::new();
    let mut env_sources = BTreeMap::new();

    for (option, schema) in &metadata.options {
        if option == "enabled" {
            continue;
        }
        validate_feature_option_schema(&feature.id, option, schema)?;
        if feature.options.contains_key(option) {
            continue;
        }
        if let Some(default) = &schema.default {
            let value = feature_option_json_value(&feature.id, option, default, schema)?;
            insert_feature_option_env(&mut env, &mut env_sources, &feature.id, option, value)?;
        }
    }

    for (option, value) in &feature.options {
        if option == "enabled" {
            continue;
        }
        let schema = metadata.options.get(option);
        if let Some(schema) = schema {
            validate_feature_option_schema(&feature.id, option, schema)?;
        }
        let value = feature_option_toml_value(&feature.id, option, value, schema)?;
        insert_feature_option_env(&mut env, &mut env_sources, &feature.id, option, value)?;
    }

    Ok(env)
}

fn insert_feature_option_env(
    env: &mut BTreeMap<String, String>,
    env_sources: &mut BTreeMap<String, String>,
    feature_id: &str,
    option: &str,
    value: String,
) -> Result<()> {
    let key = feature_option_env_name(option);
    if let Some(existing_option) = env_sources.get(&key) {
        bail!(
            "Feature option environment variable collision for {feature_id}: options `{existing_option}` and `{option}` both map to {key}"
        );
    }
    env_sources.insert(key.clone(), option.to_owned());
    env.insert(key, value);
    Ok(())
}

pub(super) fn feature_options_sort_key(
    options: &BTreeMap<String, toml::Value>,
) -> Vec<(String, String)> {
    options
        .iter()
        .filter(|(key, _)| key.as_str() != "enabled")
        .map(|(key, value)| (key.clone(), feature_option_sort_value(value)))
        .collect()
}

fn feature_option_sort_value(value: &toml::Value) -> String {
    match value {
        toml::Value::String(value) => value.clone(),
        toml::Value::Boolean(value) => value.to_string(),
        toml::Value::Integer(value) => value.to_string(),
        toml::Value::Float(value) => value.to_string(),
        toml::Value::Datetime(value) => value.to_string(),
        toml::Value::Array(values) => values
            .iter()
            .map(feature_option_sort_value)
            .collect::<Vec<_>>()
            .join(","),
        toml::Value::Table(values) => values
            .iter()
            .map(|(key, value)| format!("{key}={}", feature_option_sort_value(value)))
            .collect::<Vec<_>>()
            .join(","),
    }
}

fn feature_option_toml_value(
    feature_id: &str,
    option: &str,
    value: &toml::Value,
    schema: Option<&FeatureOptionSchema>,
) -> Result<String> {
    let resolved = match value {
        toml::Value::String(value) => {
            if matches!(
                schema.and_then(|schema| schema.option_type.as_deref()),
                Some("boolean")
            ) {
                bail!("Feature option {feature_id}.{option} must be a boolean");
            }
            value.clone()
        }
        toml::Value::Boolean(value) => {
            if matches!(
                schema.and_then(|schema| schema.option_type.as_deref()),
                Some("string")
            ) {
                bail!("Feature option {feature_id}.{option} must be a string");
            }
            value.to_string()
        }
        toml::Value::Integer(_)
        | toml::Value::Float(_)
        | toml::Value::Datetime(_)
        | toml::Value::Array(_)
        | toml::Value::Table(_) => {
            bail!("Unsupported Feature option value for {feature_id}.{option}");
        }
    };

    validate_feature_option_enum(feature_id, option, &resolved, schema)?;
    Ok(resolved)
}

fn feature_option_json_value(
    feature_id: &str,
    option: &str,
    value: &serde_json::Value,
    schema: &FeatureOptionSchema,
) -> Result<String> {
    let resolved = match value {
        serde_json::Value::String(value) => {
            if matches!(schema.option_type.as_deref(), Some("boolean")) {
                bail!("Feature option default {feature_id}.{option} must be a boolean");
            }
            value.clone()
        }
        serde_json::Value::Bool(value) => {
            if matches!(schema.option_type.as_deref(), Some("string")) {
                bail!("Feature option default {feature_id}.{option} must be a string");
            }
            value.to_string()
        }
        serde_json::Value::Null
        | serde_json::Value::Number(_)
        | serde_json::Value::Array(_)
        | serde_json::Value::Object(_) => {
            bail!("Unsupported Feature option default for {feature_id}.{option}");
        }
    };

    validate_feature_option_enum(feature_id, option, &resolved, Some(schema))?;
    Ok(resolved)
}

fn validate_feature_option_enum(
    feature_id: &str,
    option: &str,
    value: &str,
    schema: Option<&FeatureOptionSchema>,
) -> Result<()> {
    if let Some(schema) = schema
        && !schema.enum_values.is_empty()
        && !schema.enum_values.iter().any(|allowed| allowed == value)
    {
        bail!("Feature option {feature_id}.{option} must be one of the declared enum values");
    }

    Ok(())
}

fn feature_option_env_name(option: &str) -> String {
    let mut sanitized = option
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();

    let prefix_len = sanitized
        .char_indices()
        .find(|(_, ch)| !ch.is_ascii_digit() && *ch != '_')
        .map_or(sanitized.len(), |(index, _)| index);
    if prefix_len > 0 {
        sanitized.replace_range(..prefix_len, "_");
    } else if sanitized.is_empty() {
        sanitized.push('_');
    }

    sanitized
        .chars()
        .map(|ch| ch.to_ascii_uppercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::devcontainer::features::{
        FeatureInstallInput, FeatureMetadata, FeatureOptionSchema, parse_feature_ref,
    };
    #[test]
    fn feature_option_env_uses_defaults_and_skips_reserved_enabled() {
        let feature = feature_install_input(
            "ghcr.io/example/features/tool:1",
            FeatureMetadata {
                options: BTreeMap::from([
                    (
                        "version".to_owned(),
                        FeatureOptionSchema {
                            option_type: Some("string".to_owned()),
                            default: Some(serde_json::json!("latest")),
                            enum_values: vec!["latest".to_owned(), "1.2".to_owned()],
                            ..FeatureOptionSchema::default()
                        },
                    ),
                    (
                        "installTools".to_owned(),
                        FeatureOptionSchema {
                            option_type: Some("boolean".to_owned()),
                            default: Some(serde_json::json!(true)),
                            enum_values: Vec::new(),
                            ..FeatureOptionSchema::default()
                        },
                    ),
                ]),
                ..FeatureMetadata::default()
            },
        )
        .with_options([
            ("version", toml::Value::String("1.2".to_owned())),
            ("installTools", toml::Value::Boolean(false)),
            ("enabled", toml::Value::Boolean(true)),
        ]);

        let env = feature_option_env(&feature.feature, &feature.metadata).unwrap();

        assert_eq!(env.get("VERSION").map(String::as_str), Some("1.2"));
        assert_eq!(env.get("INSTALLTOOLS").map(String::as_str), Some("false"));
        assert!(!env.contains_key("ENABLED"));
    }

    #[test]
    fn feature_option_env_skips_reserved_enabled_metadata_default() {
        let feature = feature_install_input(
            "ghcr.io/example/features/tool:1",
            FeatureMetadata {
                options: BTreeMap::from([
                    (
                        "enabled".to_owned(),
                        FeatureOptionSchema {
                            option_type: Some("boolean".to_owned()),
                            default: Some(serde_json::json!(true)),
                            enum_values: Vec::new(),
                            ..FeatureOptionSchema::default()
                        },
                    ),
                    (
                        "version".to_owned(),
                        FeatureOptionSchema {
                            option_type: Some("string".to_owned()),
                            default: Some(serde_json::json!("latest")),
                            enum_values: Vec::new(),
                            ..FeatureOptionSchema::default()
                        },
                    ),
                ]),
                ..FeatureMetadata::default()
            },
        );

        let env = feature_option_env(&feature.feature, &feature.metadata).unwrap();

        assert_eq!(env.get("VERSION").map(String::as_str), Some("latest"));
        assert!(!env.contains_key("ENABLED"));
    }

    #[test]
    fn feature_option_env_uses_feature_spec_name_conversion() {
        assert_eq!(feature_option_env_name("version"), "VERSION");
        assert_eq!(feature_option_env_name("install-zsh"), "INSTALL_ZSH");
        assert_eq!(feature_option_env_name("node.version"), "NODE_VERSION");
        assert_eq!(feature_option_env_name("1password"), "_PASSWORD");

        let feature = feature_install_input(
            "ghcr.io/example/features/tool:1",
            FeatureMetadata {
                options: BTreeMap::from([
                    (
                        "1password".to_owned(),
                        FeatureOptionSchema {
                            option_type: Some("string".to_owned()),
                            default: Some(serde_json::json!("secret")),
                            enum_values: Vec::new(),
                            ..FeatureOptionSchema::default()
                        },
                    ),
                    (
                        "_debug".to_owned(),
                        FeatureOptionSchema {
                            option_type: Some("boolean".to_owned()),
                            default: Some(serde_json::json!(true)),
                            enum_values: Vec::new(),
                            ..FeatureOptionSchema::default()
                        },
                    ),
                    (
                        "foo-bar".to_owned(),
                        FeatureOptionSchema {
                            option_type: Some("string".to_owned()),
                            default: Some(serde_json::json!("dash")),
                            enum_values: Vec::new(),
                            ..FeatureOptionSchema::default()
                        },
                    ),
                    (
                        "has space".to_owned(),
                        FeatureOptionSchema {
                            option_type: Some("string".to_owned()),
                            default: Some(serde_json::json!("space")),
                            enum_values: Vec::new(),
                            ..FeatureOptionSchema::default()
                        },
                    ),
                ]),
                ..FeatureMetadata::default()
            },
        );

        let env = feature_option_env(&feature.feature, &feature.metadata).unwrap();

        assert_eq!(env.get("_PASSWORD").map(String::as_str), Some("secret"));
        assert_eq!(env.get("_DEBUG").map(String::as_str), Some("true"));
        assert_eq!(env.get("FOO_BAR").map(String::as_str), Some("dash"));
        assert_eq!(env.get("HAS_SPACE").map(String::as_str), Some("space"));
    }

    #[test]
    fn feature_option_env_rejects_converted_env_key_collision() {
        let feature = feature_install_input(
            "ghcr.io/example/features/tool:1",
            FeatureMetadata {
                options: BTreeMap::from([
                    (
                        "foo-bar".to_owned(),
                        FeatureOptionSchema {
                            option_type: Some("string".to_owned()),
                            default: Some(serde_json::json!("dash")),
                            enum_values: Vec::new(),
                            ..FeatureOptionSchema::default()
                        },
                    ),
                    (
                        "foo_bar".to_owned(),
                        FeatureOptionSchema {
                            option_type: Some("string".to_owned()),
                            default: Some(serde_json::json!("underscore")),
                            enum_values: Vec::new(),
                            ..FeatureOptionSchema::default()
                        },
                    ),
                ]),
                ..FeatureMetadata::default()
            },
        );

        let error = feature_option_env(&feature.feature, &feature.metadata).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Feature option environment variable collision"),
            "{error:#}"
        );
        assert!(error.to_string().contains("FOO_BAR"), "{error:#}");
        assert!(error.to_string().contains("foo-bar"), "{error:#}");
        assert!(error.to_string().contains("foo_bar"), "{error:#}");
    }

    #[test]
    fn feature_option_env_rejects_unsupported_schema_type() {
        let feature = feature_install_input(
            "ghcr.io/example/features/tool:1",
            FeatureMetadata {
                options: BTreeMap::from([(
                    "items".to_owned(),
                    FeatureOptionSchema {
                        option_type: Some("array".to_owned()),
                        default: None,
                        enum_values: Vec::new(),
                        ..FeatureOptionSchema::default()
                    },
                )]),
                ..FeatureMetadata::default()
            },
        );

        let error = feature_option_env(&feature.feature, &feature.metadata).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Unsupported Feature option type")
        );
        assert!(error.to_string().contains("items"));
    }

    #[test]
    fn feature_option_env_rejects_string_schema_with_enum_and_proposals() {
        let feature = feature_install_input(
            "ghcr.io/example/features/tool:1",
            FeatureMetadata {
                options: BTreeMap::from([(
                    "version".to_owned(),
                    FeatureOptionSchema {
                        option_type: Some("string".to_owned()),
                        default: Some(serde_json::json!("latest")),
                        enum_values: vec!["latest".to_owned()],
                        proposals: vec!["preview".to_owned()],
                        ..FeatureOptionSchema::default()
                    },
                )]),
                ..FeatureMetadata::default()
            },
        );

        let error = feature_option_env(&feature.feature, &feature.metadata).unwrap_err();

        assert!(
            error.to_string().contains(
                "Feature option ghcr.io/example/features/tool:1.version must not declare both enum and proposals"
            ),
            "{error:#}"
        );
    }

    fn feature_install_input(id: &str, metadata: FeatureMetadata) -> FeatureInstallInput {
        let reference = parse_feature_ref(id).unwrap();
        let canonical_id = reference.canonical_id().to_owned();
        FeatureInstallInput {
            feature: crate::config::resolved::ResolvedFeature {
                id: id.to_owned(),
                canonical_id: canonical_id.clone(),
                options: BTreeMap::new(),
            },
            reference,
            metadata,
            source_key: id.to_owned(),
            instance_key: format!("test\x1e{canonical_id}\x1e{id}"),
        }
    }

    fn feature_install_input_instance_key(input: &FeatureInstallInput) -> String {
        let options = feature_options_sort_key(&input.feature.options)
            .into_iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join("\x1f");
        format!(
            "test\x1e{}\x1e{}\x1e{options}",
            input.feature.canonical_id, input.feature.id
        )
    }

    trait FeatureInstallInputTestExt {
        fn with_options<const N: usize>(
            self,
            options: [(&'static str, toml::Value); N],
        ) -> FeatureInstallInput;
    }

    impl FeatureInstallInputTestExt for FeatureInstallInput {
        fn with_options<const N: usize>(
            mut self,
            options: [(&'static str, toml::Value); N],
        ) -> FeatureInstallInput {
            self.feature.options = options
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value))
                .collect();
            self.instance_key = feature_install_input_instance_key(&self);
            self
        }
    }
}
