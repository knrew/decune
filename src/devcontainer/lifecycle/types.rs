use std::collections::BTreeMap;

use anyhow::{Result, anyhow};

use crate::{config::types::HookLocation, devcontainer::metadata::LifecycleProperty};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LifecycleCommand {
    Shell(String),
    Args(Vec<String>),
    Parallel(BTreeMap<String, LifecycleCommand>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LifecycleDefinition {
    commands: BTreeMap<LifecycleStage, Vec<LifecycleCommand>>,
    wait_for: WaitFor,
}

impl LifecycleDefinition {
    #[cfg(test)]
    pub(in crate::devcontainer::lifecycle) fn empty() -> Self {
        Self {
            commands: BTreeMap::new(),
            wait_for: WaitFor::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn command(&self, stage: LifecycleStage) -> Option<&LifecycleCommand> {
        self.commands
            .get(&stage)
            .and_then(|commands| commands.first())
    }

    pub(crate) fn commands(&self, stage: LifecycleStage) -> &[LifecycleCommand] {
        self.commands.get(&stage).map(Vec::as_slice).unwrap_or(&[])
    }

    pub(crate) fn has_commands(&self) -> bool {
        self.commands.values().any(|commands| !commands.is_empty())
    }

    pub(crate) fn wait_for(&self) -> WaitFor {
        self.wait_for
    }

    pub(crate) fn merge_layer(&mut self, layer: LayerLifecycleDefinition) {
        for (stage, commands) in layer.commands {
            self.commands.entry(stage).or_default().extend(commands);
        }
        if let Some(wait_for) = layer.wait_for {
            self.wait_for = wait_for;
        }
    }

    #[cfg(test)]
    pub(crate) fn into_layer(self) -> Option<LayerLifecycleDefinition> {
        if self.commands.is_empty() {
            return None;
        }

        Some(LayerLifecycleDefinition {
            commands: self.commands,
            wait_for: Some(self.wait_for),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LayerLifecycleDefinition {
    commands: BTreeMap<LifecycleStage, Vec<LifecycleCommand>>,
    wait_for: Option<WaitFor>,
}

impl LayerLifecycleDefinition {
    pub(in crate::devcontainer::lifecycle) fn new(
        commands: BTreeMap<LifecycleStage, Vec<LifecycleCommand>>,
        wait_for: Option<WaitFor>,
    ) -> Self {
        Self { commands, wait_for }
    }

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
    #[cfg(test)]
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

    pub(crate) fn property_name(self) -> &'static str {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LifecycleRunPath {
    New,
    Started,
    Running,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HookStage {
    BeforeInitialize,
    AfterInitialize,
    BeforeOnCreate,
    AfterOnCreate,
    BeforeUpdateContent,
    AfterUpdateContent,
    BeforePostCreate,
    AfterPostCreate,
    BeforePostStart,
    AfterPostStart,
    BeforePostAttach,
    AfterPostAttach,
}

impl HookStage {
    pub(in crate::devcontainer::lifecycle) fn property_name(self) -> &'static str {
        match self {
            Self::BeforeInitialize => "before_initialize",
            Self::AfterInitialize => "after_initialize",
            Self::BeforeOnCreate => "before_on_create",
            Self::AfterOnCreate => "after_on_create",
            Self::BeforeUpdateContent => "before_update_content",
            Self::AfterUpdateContent => "after_update_content",
            Self::BeforePostCreate => "before_post_create",
            Self::AfterPostCreate => "after_post_create",
            Self::BeforePostStart => "before_post_start",
            Self::AfterPostStart => "after_post_start",
            Self::BeforePostAttach => "before_post_attach",
            Self::AfterPostAttach => "after_post_attach",
        }
    }

    pub(in crate::devcontainer::lifecycle) fn default_location(self) -> HookLocation {
        match self {
            Self::BeforeInitialize | Self::AfterInitialize => HookLocation::Host,
            Self::BeforeOnCreate
            | Self::AfterOnCreate
            | Self::BeforeUpdateContent
            | Self::AfterUpdateContent
            | Self::BeforePostCreate
            | Self::AfterPostCreate
            | Self::BeforePostStart
            | Self::AfterPostStart
            | Self::BeforePostAttach
            | Self::AfterPostAttach => HookLocation::Container,
        }
    }
}
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LifecycleStep {
    Hooks(HookStage),
    Lifecycle(LifecycleStage),
    ImagePreparation,
    ContainerCreate,
    HostDaemonStart,
    ContainerStart,
    DecuneSetup,
    PortForwardingStart,
    ShellAttach,
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

#[cfg(test)]
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
