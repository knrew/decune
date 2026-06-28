use std::collections::BTreeMap;

use anyhow::{Result, anyhow};
use serde_json::Value;

use crate::devcontainer::metadata::LifecycleProperty;

#[cfg(test)]
use super::types::LifecycleDefinition;
use super::types::{LayerLifecycleDefinition, LifecycleCommand, LifecycleStage, WaitFor};

pub(crate) fn parse_lifecycle_command(
    stage: LifecycleStage,
    value: &Value,
) -> Result<LifecycleCommand> {
    match value {
        Value::String(command) => Ok(LifecycleCommand::Shell(command.clone())),
        Value::Array(values) => parse_args(stage, values),
        Value::Object(entries) => parse_parallel(stage, entries),
        _ => Err(anyhow!(
            "{} must be a string, string array, or object command",
            stage.property_name()
        )),
    }
}
#[cfg(test)]
pub(crate) fn parse_lifecycle_definition(
    values: &BTreeMap<LifecycleProperty, Value>,
) -> Result<LifecycleDefinition> {
    Ok(parse_lifecycle_layer_definition(values)?.map_or_else(
        LifecycleDefinition::empty,
        LayerLifecycleDefinition::into_resolved,
    ))
}

pub(crate) fn parse_lifecycle_layer_definition(
    values: &BTreeMap<LifecycleProperty, Value>,
) -> Result<Option<LayerLifecycleDefinition>> {
    let mut commands = BTreeMap::new();

    for (property, value) in values {
        if *property == LifecycleProperty::WaitFor {
            continue;
        }

        let stage = LifecycleStage::try_from(*property)?;
        commands.insert(stage, vec![parse_lifecycle_command(stage, value)?]);
    }

    let wait_for = values
        .get(&LifecycleProperty::WaitFor)
        .map(|value| parse_wait_for(Some(value)))
        .transpose()?;

    if commands.is_empty() && wait_for.is_none() {
        return Ok(None);
    }

    Ok(Some(LayerLifecycleDefinition::new(commands, wait_for)))
}

pub(crate) fn parse_wait_for(value: Option<&Value>) -> Result<WaitFor> {
    match value {
        None => Ok(WaitFor::default()),
        Some(Value::String(stage)) => match stage.as_str() {
            "initializeCommand" => Ok(WaitFor::Initialize),
            "onCreateCommand" => Ok(WaitFor::OnCreate),
            "updateContentCommand" => Ok(WaitFor::UpdateContent),
            "postCreateCommand" => Ok(WaitFor::PostCreate),
            "postStartCommand" => Ok(WaitFor::PostStart),
            _ => Err(anyhow!("Unsupported waitFor lifecycle stage: {stage}")),
        },
        Some(_) => Err(anyhow!("waitFor must be a lifecycle stage string")),
    }
}

fn parse_args(stage: LifecycleStage, values: &[Value]) -> Result<LifecycleCommand> {
    if values.is_empty() {
        return Err(anyhow!(
            "{} command array must not be empty",
            stage.property_name()
        ));
    }

    let args = values
        .iter()
        .map(|value| match value {
            Value::String(arg) => Ok(arg.clone()),
            _ => Err(anyhow!(
                "{} command array entries must be strings",
                stage.property_name()
            )),
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(LifecycleCommand::Args(args))
}

fn parse_parallel(
    stage: LifecycleStage,
    entries: &serde_json::Map<String, Value>,
) -> Result<LifecycleCommand> {
    if entries.is_empty() {
        return Err(anyhow!(
            "{} command object must not be empty",
            stage.property_name()
        ));
    }

    let mut commands = BTreeMap::new();
    for (name, value) in entries {
        let command = match value {
            Value::String(_) | Value::Array(_) => parse_lifecycle_command(stage, value)?,
            _ => {
                return Err(anyhow!(
                    "{} parallel command entry {name} must be a string or string array",
                    stage.property_name()
                ));
            }
        };
        commands.insert(name.clone(), command);
    }

    Ok(LifecycleCommand::Parallel(commands))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::*;
    use crate::devcontainer::{
        lifecycle::{LifecycleCommand, LifecycleStage, WaitFor},
        metadata::LifecycleProperty,
    };

    #[test]
    fn parses_string_command_as_shell_command() {
        let command =
            parse_lifecycle_command(LifecycleStage::PostCreate, &json!("npm install")).unwrap();

        assert_eq!(command, LifecycleCommand::Shell("npm install".to_owned()));
    }

    #[test]
    fn parses_array_command_as_exec_args() {
        let command = parse_lifecycle_command(
            LifecycleStage::PostStart,
            &json!(["bash", "-lc", "echo ready"]),
        )
        .unwrap();

        assert_eq!(
            command,
            LifecycleCommand::Args(vec![
                "bash".to_owned(),
                "-lc".to_owned(),
                "echo ready".to_owned()
            ])
        );
    }

    #[test]
    fn rejects_empty_array_command() {
        let error = parse_lifecycle_command(LifecycleStage::OnCreate, &json!([])).unwrap_err();

        assert!(error.to_string().contains("onCreateCommand"));
        assert!(error.to_string().contains("must not be empty"));
    }

    #[test]
    fn rejects_non_string_array_entries() {
        let error =
            parse_lifecycle_command(LifecycleStage::PostCreate, &json!(["echo", 1])).unwrap_err();

        assert!(error.to_string().contains("postCreateCommand"));
        assert!(error.to_string().contains("entries must be strings"));
    }

    #[test]
    fn parses_object_command_as_parallel_entries() {
        let command = parse_lifecycle_command(
            LifecycleStage::UpdateContent,
            &json!({
                "frontend": "npm install",
                "backend": ["cargo", "fetch"]
            }),
        )
        .unwrap();

        assert_eq!(
            command,
            LifecycleCommand::Parallel(
                [
                    (
                        "backend".to_owned(),
                        LifecycleCommand::Args(vec!["cargo".to_owned(), "fetch".to_owned()])
                    ),
                    (
                        "frontend".to_owned(),
                        LifecycleCommand::Shell("npm install".to_owned())
                    )
                ]
                .into()
            )
        );
    }

    #[test]
    fn rejects_empty_object_command() {
        let error = parse_lifecycle_command(LifecycleStage::PostStart, &json!({})).unwrap_err();

        assert!(error.to_string().contains("postStartCommand"));
        assert!(error.to_string().contains("object must not be empty"));
    }

    #[test]
    fn rejects_invalid_parallel_entry_type() {
        let error = parse_lifecycle_command(
            LifecycleStage::UpdateContent,
            &json!({"api": {"nested": true}}),
        )
        .unwrap_err();

        assert!(error.to_string().contains("updateContentCommand"));
        assert!(error.to_string().contains("parallel command entry api"));
    }

    #[test]
    fn parses_wait_for_with_update_content_default() {
        assert_eq!(WaitFor::default(), WaitFor::UpdateContent);
        assert_eq!(parse_wait_for(None).unwrap(), WaitFor::UpdateContent);
        assert_eq!(
            parse_wait_for(Some(&json!("postCreateCommand"))).unwrap(),
            WaitFor::PostCreate
        );
    }

    #[test]
    fn rejects_unknown_wait_for_stage() {
        let error = parse_wait_for(Some(&json!("postAttachCommand"))).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Unsupported waitFor lifecycle stage")
        );
    }

    #[test]
    fn rejects_non_string_wait_for() {
        let error = parse_wait_for(Some(&json!(["postCreateCommand"]))).unwrap_err();

        assert!(error.to_string().contains("waitFor must be"));
    }

    #[test]
    fn parses_lifecycle_definition_from_metadata_values() {
        let lifecycle = parse_lifecycle_definition(&BTreeMap::from([
            (
                LifecycleProperty::InitializeCommand,
                json!("scripts/init.sh"),
            ),
            (
                LifecycleProperty::PostStartCommand,
                json!(["bash", "-lc", "echo ready"]),
            ),
            (LifecycleProperty::WaitFor, json!("postStartCommand")),
        ]))
        .unwrap();

        assert_eq!(lifecycle.wait_for(), WaitFor::PostStart);
        assert_eq!(
            lifecycle.command(LifecycleStage::Initialize),
            Some(&LifecycleCommand::Shell("scripts/init.sh".to_owned()))
        );
        assert_eq!(
            lifecycle.command(LifecycleStage::PostStart),
            Some(&LifecycleCommand::Args(vec![
                "bash".to_owned(),
                "-lc".to_owned(),
                "echo ready".to_owned()
            ]))
        );
    }

    #[test]
    fn lifecycle_merge_collects_same_stage_commands_in_order() {
        let mut lifecycle = parse_lifecycle_definition(&BTreeMap::from([(
            LifecycleProperty::PostStartCommand,
            json!("feature-one"),
        )]))
        .unwrap();
        let second = parse_lifecycle_layer_definition(&BTreeMap::from([(
            LifecycleProperty::PostStartCommand,
            json!("feature-two"),
        )]))
        .unwrap()
        .unwrap();
        let user = parse_lifecycle_layer_definition(&BTreeMap::from([(
            LifecycleProperty::PostStartCommand,
            json!("user-command"),
        )]))
        .unwrap()
        .unwrap();

        lifecycle.merge_layer(second);
        lifecycle.merge_layer(user);

        assert_eq!(
            lifecycle.commands(LifecycleStage::PostStart),
            &[
                LifecycleCommand::Shell("feature-one".to_owned()),
                LifecycleCommand::Shell("feature-two".to_owned()),
                LifecycleCommand::Shell("user-command".to_owned()),
            ]
        );
    }
}
