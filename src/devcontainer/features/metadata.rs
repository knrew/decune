use std::{collections::BTreeMap, fs, path::Path};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::Value as JsonValue;

use crate::{config::layer::ConfigLayer, devcontainer::metadata::parse_metadata_layer};

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub(crate) struct FeatureMetadata {
    pub(crate) id: String,
    pub(crate) version: String,
    pub(crate) name: String,
    #[serde(default, rename = "legacyIds")]
    pub(crate) legacy_ids: Vec<String>,
    #[serde(default, rename = "dependsOn")]
    pub(crate) depends_on: BTreeMap<String, serde_json::Value>,
    #[serde(default, rename = "installsAfter")]
    pub(crate) installs_after: Vec<String>,
    #[serde(default)]
    pub(crate) options: BTreeMap<String, FeatureOptionSchema>,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FeatureOptionSchema {
    #[serde(default, rename = "type")]
    pub(crate) option_type: Option<String>,
    #[serde(default)]
    pub(crate) default: Option<serde_json::Value>,
    #[serde(default, rename = "enum")]
    pub(crate) enum_values: Vec<String>,
    #[serde(default)]
    pub(crate) proposals: Vec<String>,
    #[serde(default)]
    pub(crate) description: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FeatureMetadataDocument {
    pub(crate) metadata: FeatureMetadata,
    pub(crate) layer: ConfigLayer,
}

#[allow(dead_code)]
pub(crate) fn read_feature_metadata(path: &Path) -> Result<FeatureMetadata> {
    read_feature_metadata_document(path).map(|document| document.metadata)
}

pub(crate) fn read_feature_metadata_document(path: &Path) -> Result<FeatureMetadataDocument> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read Feature metadata: {}", path.display()))?;
    let raw: JsonValue = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse Feature metadata: {}", path.display()))?;
    validate_feature_metadata_document(&raw)?;
    let metadata = serde_json::from_value(raw.clone())
        .with_context(|| format!("Failed to parse Feature metadata: {}", path.display()))?;
    validate_feature_metadata_schema(&metadata)?;
    let layer = parse_metadata_layer(raw.clone())
        .and_then(|metadata| metadata.to_config_layer_without_forward_ports())
        .with_context(|| {
            format!(
                "Failed to convert Feature metadata to devcontainer metadata layer: {}",
                path.display()
            )
        })?;

    Ok(FeatureMetadataDocument { metadata, layer })
}

fn validate_feature_metadata_document(raw: &JsonValue) -> Result<()> {
    let Some(object) = raw.as_object() else {
        bail!("Feature metadata must be a JSON object");
    };

    for required in ["id", "version", "name"] {
        if !object.contains_key(required) {
            bail!("Feature metadata must specify {required}");
        }
    }

    for property in object.keys() {
        if !feature_metadata_property_is_supported(property) {
            bail!("Unsupported Feature metadata property: {property}");
        }
    }

    if let Some(customizations) = object.get("customizations")
        && !customizations.is_object()
    {
        bail!("Feature metadata customizations must be an object");
    }
    for property in [
        "id",
        "version",
        "name",
        "description",
        "documentationURL",
        "licenseURL",
    ] {
        if let Some(value) = object.get(property)
            && !value.is_string()
        {
            bail!("Feature metadata {property} must be a string");
        }
    }
    for property in ["keywords", "legacyIds"] {
        if let Some(value) = object.get(property)
            && !json_array_is_strings(value)
        {
            bail!("Feature metadata {property} must be an array of strings");
        }
    }
    if let Some(deprecated) = object.get("deprecated")
        && !deprecated.is_boolean()
    {
        bail!("Feature metadata deprecated must be a boolean");
    }

    Ok(())
}

fn json_array_is_strings(value: &JsonValue) -> bool {
    value
        .as_array()
        .is_some_and(|values| values.iter().all(JsonValue::is_string))
}

fn feature_metadata_property_is_supported(property: &str) -> bool {
    matches!(
        property,
        "id" | "version"
            | "name"
            | "description"
            | "documentationURL"
            | "licenseURL"
            | "keywords"
            | "legacyIds"
            | "deprecated"
            | "options"
            | "dependsOn"
            | "installsAfter"
            | "containerEnv"
            | "customizations"
            | "entrypoint"
            | "init"
            | "privileged"
            | "capAdd"
            | "securityOpt"
            | "mounts"
            | "onCreateCommand"
            | "updateContentCommand"
            | "postCreateCommand"
            | "postStartCommand"
            | "postAttachCommand"
    )
}

fn validate_feature_metadata_schema(metadata: &FeatureMetadata) -> Result<()> {
    if metadata.id.trim().is_empty() {
        bail!("Feature metadata id must not be empty");
    }
    if metadata.version.trim().is_empty() {
        bail!("Feature metadata version must not be empty");
    }
    if metadata.name.trim().is_empty() {
        bail!("Feature metadata name must not be empty");
    }
    for (option, schema) in &metadata.options {
        validate_feature_option_schema(&metadata.id, option, schema)?;
    }

    Ok(())
}

pub(super) fn validate_feature_option_schema(
    feature_id: &str,
    option: &str,
    schema: &FeatureOptionSchema,
) -> Result<()> {
    let Some(option_type) = schema.option_type.as_deref() else {
        bail!("Feature option {feature_id}.{option} must specify type");
    };

    match option_type {
        "string" => {
            let default = schema.default.as_ref().ok_or_else(|| {
                anyhow!("Feature option {feature_id}.{option} must specify default")
            })?;
            if !default.is_string() {
                bail!("Feature option default {feature_id}.{option} must be a string");
            }
            if !schema.enum_values.is_empty() && !schema.proposals.is_empty() {
                bail!(
                    "Feature option {feature_id}.{option} must not declare both enum and proposals"
                );
            }
            Ok(())
        }
        "boolean" => {
            let default = schema.default.as_ref().ok_or_else(|| {
                anyhow!("Feature option {feature_id}.{option} must specify default")
            })?;
            if !default.is_boolean() {
                bail!("Feature option default {feature_id}.{option} must be a boolean");
            }
            if !schema.enum_values.is_empty() || !schema.proposals.is_empty() {
                bail!(
                    "Feature option {feature_id}.{option} boolean schema must not declare enum or proposals"
                );
            }
            Ok(())
        }
        _ => bail!("Unsupported Feature option type for {feature_id}.{option}: {option_type}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_metadata_rejects_initialize_command_and_wait_for() {
        let temp = tempfile::tempdir().unwrap();
        let metadata_path = temp.path().join("devcontainer-feature.json");

        for (property, content) in [
            (
                "initializeCommand",
                r#"{"id":"tool","version":"1.0.0","name":"Tool","initializeCommand":"echo host"}"#,
            ),
            (
                "waitFor",
                r#"{"id":"tool","version":"1.0.0","name":"Tool","waitFor":"postCreateCommand"}"#,
            ),
        ] {
            fs::write(&metadata_path, content).unwrap();
            let error = read_feature_metadata_document(&metadata_path).unwrap_err();

            assert!(error.to_string().contains(property), "{error:#}");
        }
    }

    #[test]
    fn feature_metadata_rejects_properties_outside_feature_schema() {
        let temp = tempfile::tempdir().unwrap();
        let metadata_path = temp.path().join("devcontainer-feature.json");

        for (property, content) in [
            (
                "remoteUser",
                r#"{"id":"tool","version":"1.0.0","name":"Tool","remoteUser":"vscode"}"#,
            ),
            (
                "workspaceFolder",
                r#"{"id":"tool","version":"1.0.0","name":"Tool","workspaceFolder":"/workspace"}"#,
            ),
            (
                "runArgs",
                r#"{"id":"tool","version":"1.0.0","name":"Tool","runArgs":["--init"]}"#,
            ),
            (
                "image",
                r#"{"id":"tool","version":"1.0.0","name":"Tool","image":"alpine:3.20"}"#,
            ),
            (
                "x-extra",
                r#"{"id":"tool","version":"1.0.0","name":"Tool","x-extra":true}"#,
            ),
        ] {
            fs::write(&metadata_path, content).unwrap();
            let error = read_feature_metadata_document(&metadata_path).unwrap_err();

            assert!(error.to_string().contains(property), "{error:#}");
        }
    }

    #[test]
    fn feature_metadata_requires_id_version_name_and_option_defaults() {
        let temp = tempfile::tempdir().unwrap();
        let metadata_path = temp.path().join("devcontainer-feature.json");

        for (expected, content) in [
            ("id", r#"{"version":"1.0.0","name":"Tool"}"#),
            ("version", r#"{"id":"tool","name":"Tool"}"#),
            ("name", r#"{"id":"tool","version":"1.0.0"}"#),
            (
                "default",
                r#"{"id":"tool","version":"1.0.0","name":"Tool","options":{"version":{"type":"string"}}}"#,
            ),
        ] {
            fs::write(&metadata_path, content).unwrap();
            let error = read_feature_metadata_document(&metadata_path).unwrap_err();

            assert!(error.to_string().contains(expected), "{error:#}");
        }
    }
}
