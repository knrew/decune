use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DevcontainerSource {
    Image(String),
    Dockerfile(DevcontainerBuild),
    Compose(DevcontainerCompose),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DevcontainerBuild {
    pub(crate) dockerfile: String,
    pub(crate) context: Option<String>,
    pub(crate) args: std::collections::BTreeMap<String, String>,
    pub(crate) options: Vec<String>,
    pub(crate) target: Option<String>,
    pub(crate) cache_from: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DevcontainerCompose {
    pub(crate) files: Vec<String>,
    pub(crate) service: String,
    pub(crate) run_services: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DevcontainerRunArg {
    Init,
    Privileged,
    CapAdd(String),
    SecurityOpt(String),
    AddHost(String),
    Dns(String),
    DnsSearch(String),
    Passthrough { option: String, value: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum DevcontainerShutdownAction {
    None,
    StopContainer,
    StopCompose,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum UserEnvProbe {
    None,
    LoginShell,
    InteractiveShell,
    LoginInteractiveShell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum LifecycleProperty {
    InitializeCommand,
    OnCreateCommand,
    UpdateContentCommand,
    PostCreateCommand,
    PostStartCommand,
    PostAttachCommand,
    WaitFor,
}
