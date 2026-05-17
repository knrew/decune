#![allow(dead_code)]

use std::collections::BTreeMap;

use anyhow::{Result, anyhow};
use serde_json::Value;

use crate::devcontainer::metadata::LifecycleProperty;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LifecycleCommand {
    Shell(String),
    Args(Vec<String>),
    Parallel(BTreeMap<String, LifecycleCommand>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LifecycleDefinition {
    commands: BTreeMap<LifecycleStage, LifecycleCommand>,
    wait_for: WaitFor,
}

impl LifecycleDefinition {
    pub(crate) fn command(&self, stage: LifecycleStage) -> Option<&LifecycleCommand> {
        self.commands.get(&stage)
    }

    pub(crate) fn wait_for(&self) -> WaitFor {
        self.wait_for
    }

    pub(crate) fn merge_layer(&mut self, layer: LayerLifecycleDefinition) {
        self.commands.extend(layer.commands);
        if let Some(wait_for) = layer.wait_for {
            self.wait_for = wait_for;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LayerLifecycleDefinition {
    commands: BTreeMap<LifecycleStage, LifecycleCommand>,
    wait_for: Option<WaitFor>,
}

impl LayerLifecycleDefinition {
    pub(crate) fn into_resolved(self) -> LifecycleDefinition {
        LifecycleDefinition {
            commands: self.commands,
            wait_for: self.wait_for.unwrap_or_default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum LifecycleStage {
    Initialize,
    OnCreate,
    UpdateContent,
    PostCreate,
    PostStart,
    PostAttach,
}

impl LifecycleStage {
    pub(crate) fn execution_location(self) -> LifecycleExecutionLocation {
        match self {
            Self::Initialize => LifecycleExecutionLocation::Host,
            Self::OnCreate
            | Self::UpdateContent
            | Self::PostCreate
            | Self::PostStart
            | Self::PostAttach => LifecycleExecutionLocation::Container,
        }
    }

    fn property_name(self) -> &'static str {
        match self {
            Self::Initialize => "initializeCommand",
            Self::OnCreate => "onCreateCommand",
            Self::UpdateContent => "updateContentCommand",
            Self::PostCreate => "postCreateCommand",
            Self::PostStart => "postStartCommand",
            Self::PostAttach => "postAttachCommand",
        }
    }
}

impl TryFrom<LifecycleProperty> for LifecycleStage {
    type Error = anyhow::Error;

    fn try_from(value: LifecycleProperty) -> Result<Self> {
        match value {
            LifecycleProperty::InitializeCommand => Ok(Self::Initialize),
            LifecycleProperty::OnCreateCommand => Ok(Self::OnCreate),
            LifecycleProperty::UpdateContentCommand => Ok(Self::UpdateContent),
            LifecycleProperty::PostCreateCommand => Ok(Self::PostCreate),
            LifecycleProperty::PostStartCommand => Ok(Self::PostStart),
            LifecycleProperty::PostAttachCommand => Ok(Self::PostAttach),
            LifecycleProperty::WaitFor => Err(anyhow!("waitFor is not a lifecycle command")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LifecycleExecutionLocation {
    Host,
    Container,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum WaitFor {
    Initialize,
    OnCreate,
    #[default]
    UpdateContent,
    PostCreate,
    PostStart,
}

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

pub(crate) fn parse_lifecycle_definition(
    values: &BTreeMap<LifecycleProperty, Value>,
) -> Result<LifecycleDefinition> {
    Ok(match parse_lifecycle_layer_definition(values)? {
        Some(layer) => layer.into_resolved(),
        None => LifecycleDefinition {
            commands: BTreeMap::new(),
            wait_for: WaitFor::default(),
        },
    })
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
        commands.insert(stage, parse_lifecycle_command(stage, value)?);
    }

    let wait_for = values
        .get(&LifecycleProperty::WaitFor)
        .map(|value| parse_wait_for(Some(value)))
        .transpose()?;

    if commands.is_empty() && wait_for.is_none() {
        return Ok(None);
    }

    Ok(Some(LayerLifecycleDefinition { commands, wait_for }))
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

    use crate::devcontainer::metadata::LifecycleProperty;

    use super::*;

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
    fn classifies_initialize_as_host_and_other_stages_as_container() {
        assert_eq!(
            LifecycleStage::Initialize.execution_location(),
            LifecycleExecutionLocation::Host
        );

        for stage in [
            LifecycleStage::OnCreate,
            LifecycleStage::UpdateContent,
            LifecycleStage::PostCreate,
            LifecycleStage::PostStart,
            LifecycleStage::PostAttach,
        ] {
            assert_eq!(
                stage.execution_location(),
                LifecycleExecutionLocation::Container
            );
        }
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
}
