use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Deserializer, de};
use serde_json::Value as JsonValue;

use crate::runtime::compose_ports::ComposePortEntry;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ComposeConfigOutput {
    pub(crate) model: ComposeConfigModel,
    pub(crate) canonical_model: JsonValue,
    pub(crate) published_port_entries: Vec<ComposePortEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct ComposeConfigModel {
    #[serde(default)]
    services: std::collections::BTreeMap<String, ComposeConfigService>,
}

impl ComposeConfigModel {
    pub(crate) fn has_service(&self, service: &str) -> bool {
        self.services.contains_key(service)
    }

    pub(crate) fn service(&self, service: &str) -> Option<&ComposeConfigService> {
        self.services.get(service)
    }

    pub(crate) fn services(&self) -> impl Iterator<Item = (&String, &ComposeConfigService)> {
        self.services.iter()
    }

    pub(crate) fn validate_services(
        &self,
        validation: &ComposeServiceValidation<'_>,
    ) -> Result<()> {
        validate_absolute_workspace_folder(validation.workspace_folder)?;
        if !self.has_service(validation.primary_service) {
            return Err(missing_compose_service_error(
                validation.project_name,
                "primary service",
                validation.primary_service,
            ));
        }

        if let Some(run_services) = validation.run_services {
            for service in run_services {
                if !self.has_service(service) {
                    return Err(missing_compose_service_error(
                        validation.project_name,
                        "runServices service",
                        service,
                    ));
                }
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposePrimaryImage {
    pub(crate) base_image: String,
    pub(crate) has_build: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ComposePrimaryImageResolver<'a> {
    pub(crate) project_name: &'a str,
    pub(crate) service: &'a str,
}

impl ComposePrimaryImageResolver<'_> {
    pub(crate) fn resolve(self, model: &ComposeConfigModel) -> Result<ComposePrimaryImage> {
        let Some(service_model) = model.service(self.service) else {
            bail!(
                "Docker Compose project {} primary service `{}` is missing",
                self.project_name,
                self.service
            );
        };
        let has_build = service_model.build.is_some();
        if let Some(image) = service_model
            .image
            .as_ref()
            .filter(|image| !image.trim().is_empty())
        {
            return Ok(ComposePrimaryImage {
                base_image: image.clone(),
                has_build,
            });
        }
        if has_build {
            return Ok(ComposePrimaryImage {
                base_image: format!("{}-{}", self.project_name, self.service),
                has_build,
            });
        }

        bail!(
            "Docker Compose project {} primary service `{}` did not resolve an image or build",
            self.project_name,
            self.service
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct ComposeConfigService {
    #[serde(default)]
    pub(crate) image: Option<String>,
    #[serde(default)]
    pub(crate) build: Option<serde_json::Value>,
    #[serde(default)]
    pub(crate) user: Option<String>,
    #[serde(default)]
    pub(crate) working_dir: Option<String>,
    #[serde(default)]
    pub(crate) network_mode: Option<String>,
    #[serde(default)]
    pub(crate) scale: Option<u64>,
    #[serde(default)]
    pub(crate) deploy: ComposeConfigDeploy,
    #[serde(default, deserialize_with = "deserialize_compose_startup_value")]
    pub(crate) entrypoint: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_compose_startup_value")]
    pub(crate) command: Option<Vec<String>>,
    #[serde(default)]
    pub(crate) ports: Vec<JsonValue>,
}

impl ComposeConfigService {
    pub(crate) fn effective_replica_count(&self) -> u64 {
        self.scale.or(self.deploy.replicas).unwrap_or(1)
    }

    pub(crate) fn uses_host_network(&self) -> bool {
        self.network_mode.as_deref() == Some("host")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct ComposeConfigDeploy {
    #[serde(default)]
    pub(crate) replicas: Option<u64>,
}

fn deserialize_compose_startup_value<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    let Some(value) = Option::<JsonValue>::deserialize(deserializer)? else {
        return Ok(None);
    };
    match value {
        JsonValue::Null => Ok(None),
        JsonValue::String(value) => {
            if value.is_empty() {
                Ok(Some(Vec::new()))
            } else {
                Ok(Some(vec![value]))
            }
        }
        JsonValue::Array(values) => values
            .into_iter()
            .map(|value| match value {
                JsonValue::String(value) => Ok(value),
                other => Err(de::Error::custom(format!(
                    "Docker Compose startup value must contain only strings: {other}"
                ))),
            })
            .collect::<std::result::Result<Vec<_>, _>>()
            .map(Some),
        other => Err(de::Error::custom(format!(
            "Docker Compose startup value must be null, string, or string array: {other}"
        ))),
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ComposeServiceValidation<'a> {
    pub(crate) primary_service: &'a str,
    pub(crate) run_services: Option<&'a [String]>,
    pub(crate) workspace_folder: &'a str,
    pub(crate) project_name: &'a str,
}

fn missing_compose_service_error(project_name: &str, role: &str, service: &str) -> anyhow::Error {
    anyhow!(
        "Docker Compose project {project_name} does not contain {role} `{service}`. The service may be disabled by Compose profiles"
    )
}

fn validate_absolute_workspace_folder(workspace_folder: &str) -> Result<()> {
    if workspace_folder.starts_with('/') {
        return Ok(());
    }

    Err(anyhow!(
        "workspaceFolder must be an absolute container path: {workspace_folder}"
    ))
}
