use std::collections::BTreeMap;

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
    services: BTreeMap<String, ComposeConfigService>,
    #[serde(default)]
    networks: BTreeMap<String, ComposeConfigResource>,
    #[serde(default)]
    volumes: BTreeMap<String, ComposeConfigResource>,
    #[serde(default)]
    configs: BTreeMap<String, ComposeConfigResource>,
    #[serde(default)]
    secrets: BTreeMap<String, ComposeConfigResource>,
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

    pub(crate) fn networks(&self) -> impl Iterator<Item = (&String, &ComposeConfigResource)> {
        self.networks.iter()
    }

    pub(crate) fn volumes(&self) -> impl Iterator<Item = (&String, &ComposeConfigResource)> {
        self.volumes.iter()
    }

    pub(crate) fn configs(&self) -> impl Iterator<Item = (&String, &ComposeConfigResource)> {
        self.configs.iter()
    }

    pub(crate) fn secrets(&self) -> impl Iterator<Item = (&String, &ComposeConfigResource)> {
        self.secrets.iter()
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
    pub(crate) container_name: Option<String>,
    #[serde(default, deserialize_with = "deserialize_compose_service_networks")]
    pub(crate) networks: BTreeMap<String, JsonValue>,
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

    pub(crate) fn network_names(&self) -> impl Iterator<Item = &String> {
        self.networks.keys()
    }

    pub(crate) fn network_config(&self, network: &str) -> Option<&JsonValue> {
        self.networks.get(network)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct ComposeConfigDeploy {
    #[serde(default)]
    pub(crate) replicas: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct ComposeConfigResource {
    #[serde(default)]
    pub(crate) name: Option<String>,
    #[serde(default)]
    pub(crate) driver: Option<String>,
    #[serde(default)]
    pub(crate) external: ComposeConfigExternal,
    #[serde(default)]
    pub(crate) ipam: Option<ComposeConfigIpam>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct ComposeConfigIpam {
    #[serde(default)]
    pub(crate) driver: Option<String>,
    #[serde(default)]
    pub(crate) config: Vec<ComposeConfigIpamConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct ComposeConfigIpamConfig {
    #[serde(default)]
    pub(crate) subnet: Option<String>,
    #[serde(default)]
    pub(crate) gateway: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct ComposeConfigExternal {
    external: bool,
}

impl ComposeConfigExternal {
    pub(crate) const fn is_external(self) -> bool {
        self.external
    }
}

impl<'de> Deserialize<'de> for ComposeConfigExternal {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Option::<JsonValue>::deserialize(deserializer)?;
        let external = match value {
            Some(JsonValue::Bool(value)) => value,
            Some(JsonValue::Object(_)) => true,
            Some(
                JsonValue::Null | JsonValue::Number(_) | JsonValue::String(_) | JsonValue::Array(_),
            )
            | None => false,
        };
        Ok(Self { external })
    }
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
                other @ (JsonValue::Null
                | JsonValue::Bool(_)
                | JsonValue::Number(_)
                | JsonValue::Array(_)
                | JsonValue::Object(_)) => Err(de::Error::custom(format!(
                    "Docker Compose startup value must contain only strings: {other}"
                ))),
            })
            .collect::<std::result::Result<Vec<_>, _>>()
            .map(Some),
        other @ (JsonValue::Bool(_) | JsonValue::Number(_) | JsonValue::Object(_)) => {
            Err(de::Error::custom(format!(
                "Docker Compose startup value must be null, string, or string array: {other}"
            )))
        }
    }
}

fn deserialize_compose_service_networks<'de, D>(
    deserializer: D,
) -> std::result::Result<BTreeMap<String, JsonValue>, D::Error>
where
    D: Deserializer<'de>,
{
    match JsonValue::deserialize(deserializer)? {
        JsonValue::Object(values) => Ok(values.into_iter().collect()),
        JsonValue::Array(values) => values
            .into_iter()
            .map(|value| match value {
                JsonValue::String(network) => Ok((network, JsonValue::Null)),
                other @ (JsonValue::Null
                | JsonValue::Bool(_)
                | JsonValue::Number(_)
                | JsonValue::Array(_)
                | JsonValue::Object(_)) => Err(de::Error::custom(format!(
                    "Docker Compose service networks list must contain only strings: {other}"
                ))),
            })
            .collect(),
        JsonValue::Null => Ok(BTreeMap::new()),
        other @ (JsonValue::Bool(_) | JsonValue::Number(_) | JsonValue::String(_)) => {
            Err(de::Error::custom(format!(
                "Docker Compose service networks must be an object or string array: {other}"
            )))
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_config_service_deserializes_network_shapes() {
        let model: ComposeConfigModel = serde_json::from_value(serde_json::json!({
            "services": {
                "missing": {"image": "alpine:3.20"},
                "null": {"image": "alpine:3.20", "networks": null},
                "list": {"image": "alpine:3.20", "networks": ["backend"]},
                "map": {
                    "image": "alpine:3.20",
                    "networks": {
                        "backend": null,
                        "frontend": {"aliases": ["app"]}
                    }
                }
            }
        }))
        .unwrap();

        assert!(model.service("missing").unwrap().networks.is_empty());
        assert!(model.service("null").unwrap().networks.is_empty());
        assert_eq!(
            model
                .service("list")
                .unwrap()
                .network_names()
                .collect::<Vec<_>>(),
            [&"backend".to_owned()]
        );
        assert_eq!(
            model
                .service("map")
                .unwrap()
                .network_names()
                .collect::<Vec<_>>(),
            [&"backend".to_owned(), &"frontend".to_owned()]
        );
    }

    #[test]
    fn compose_config_model_preserves_service_user() {
        let model: ComposeConfigModel = serde_json::from_value(serde_json::json!({
            "services": {
                "app": {
                    "image": "alpine:3.20",
                    "user": "1001:1002"
                }
            }
        }))
        .unwrap();

        assert_eq!(
            model
                .service("app")
                .and_then(|service| service.user.as_deref()),
            Some("1001:1002")
        );
    }

    #[test]
    fn compose_config_model_preserves_port_policy_service_context() {
        let model: ComposeConfigModel = serde_json::from_value(serde_json::json!({
            "services": {
                "scaled": {
                    "image": "alpine:3.20",
                    "scale": 2
                },
                "deployed": {
                    "image": "alpine:3.20",
                    "deploy": {"replicas": 3}
                },
                "hostnet": {
                    "image": "alpine:3.20",
                    "network_mode": "host"
                }
            }
        }))
        .unwrap();

        assert_eq!(
            model.service("scaled").unwrap().effective_replica_count(),
            2
        );
        assert_eq!(
            model.service("deployed").unwrap().effective_replica_count(),
            3
        );
        assert!(model.service("hostnet").unwrap().uses_host_network());
    }

    #[test]
    fn compose_primary_image_resolver_uses_service_image_without_build() {
        let model: ComposeConfigModel = serde_json::from_value(serde_json::json!({
            "services": {
                "app": {
                    "image": "example/app:dev"
                }
            }
        }))
        .unwrap();

        let image = ComposePrimaryImageResolver {
            project_name: "decune-project-abc123def456",
            service: "app",
        }
        .resolve(&model)
        .unwrap();

        assert_eq!(image.base_image, "example/app:dev");
        assert!(!image.has_build);
    }

    #[test]
    fn compose_primary_image_resolver_uses_compose_build_default_tag_without_image() {
        let model: ComposeConfigModel = serde_json::from_value(serde_json::json!({
            "services": {
                "app": {
                    "build": {"context": ".", "dockerfile": "Dockerfile"}
                }
            }
        }))
        .unwrap();

        let image = ComposePrimaryImageResolver {
            project_name: "decune-project-abc123def456",
            service: "app",
        }
        .resolve(&model)
        .unwrap();

        assert_eq!(image.base_image, "decune-project-abc123def456-app");
        assert!(image.has_build);
    }

    #[test]
    fn compose_primary_image_resolver_uses_canonical_image_when_build_is_tagged() {
        let model: ComposeConfigModel = serde_json::from_value(serde_json::json!({
            "services": {
                "app": {
                    "image": "example/app:dev",
                    "build": {"context": ".", "dockerfile": "Dockerfile"}
                }
            }
        }))
        .unwrap();

        let image = ComposePrimaryImageResolver {
            project_name: "decune-project-abc123def456",
            service: "app",
        }
        .resolve(&model)
        .unwrap();

        assert_eq!(image.base_image, "example/app:dev");
        assert!(image.has_build);
    }

    #[test]
    fn compose_primary_image_resolver_rejects_service_without_image_or_build() {
        let model: ComposeConfigModel = serde_json::from_value(serde_json::json!({
            "services": {
                "app": {}
            }
        }))
        .unwrap();

        let error = ComposePrimaryImageResolver {
            project_name: "decune-project-abc123def456",
            service: "app",
        }
        .resolve(&model)
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("did not resolve an image or build")
        );
    }

    #[test]
    fn compose_config_fixture_parses_services_without_rejecting_unknown_fields() {
        let model: ComposeConfigModel = serde_json::from_str(
            r#"
                {
                  "name": "ignored",
                  "services": {
                    "app": {
                      "image": "alpine:3.20",
                      "working_dir": "/workspace",
                      "x-compose-version-dependent": true
                    },
                    "db": {
                      "build": {"context": ".", "dockerfile": "Dockerfile"}
                    }
                  },
                  "networks": {"default": {"name": "example_default"}}
                }
                "#,
        )
        .unwrap();

        assert!(model.has_service("app"));
        assert!(model.has_service("db"));
        assert_eq!(
            model
                .service("app")
                .and_then(|service| service.image.as_deref()),
            Some("alpine:3.20")
        );
    }

    #[test]
    fn compose_introspection_validation_rejects_missing_primary_service() {
        let model: ComposeConfigModel =
            serde_json::from_str(r#"{"services":{"db":{"image":"postgres:16"}}}"#).unwrap();
        let validation = ComposeServiceValidation {
            primary_service: "app",
            run_services: None,
            workspace_folder: "/workspace",
            project_name: "decune-project-abc123",
        };

        let error = model.validate_services(&validation).unwrap_err();

        assert_eq!(
            error.to_string(),
            "Docker Compose project decune-project-abc123 does not contain primary service `app`. The service may be disabled by Compose profiles"
        );
    }

    #[test]
    fn compose_introspection_validation_rejects_missing_run_service() {
        let model: ComposeConfigModel =
            serde_json::from_str(r#"{"services":{"app":{"image":"alpine:3.20"}}}"#).unwrap();
        let run_services = vec!["app".to_owned(), "db".to_owned()];
        let validation = ComposeServiceValidation {
            primary_service: "app",
            run_services: Some(&run_services),
            workspace_folder: "/workspace",
            project_name: "decune-project-abc123",
        };

        let error = model.validate_services(&validation).unwrap_err();

        assert_eq!(
            error.to_string(),
            "Docker Compose project decune-project-abc123 does not contain runServices service `db`. The service may be disabled by Compose profiles"
        );
    }

    #[test]
    fn compose_introspection_validation_rejects_profile_disabled_primary_service() {
        let model: ComposeConfigModel =
            serde_json::from_str(r#"{"services":{"db":{"image":"postgres:16"}}}"#).unwrap();
        let validation = ComposeServiceValidation {
            primary_service: "app",
            run_services: None,
            workspace_folder: "/workspace",
            project_name: "decune-project-abc123",
        };

        let error = model.validate_services(&validation).unwrap_err();

        assert!(error.to_string().contains("disabled by Compose profiles"));
    }

    #[test]
    fn compose_introspection_validation_rejects_relative_workspace_folder() {
        let model: ComposeConfigModel =
            serde_json::from_str(r#"{"services":{"app":{"image":"alpine:3.20"}}}"#).unwrap();
        let validation = ComposeServiceValidation {
            primary_service: "app",
            run_services: None,
            workspace_folder: "workspace",
            project_name: "decune-project-abc123",
        };

        let error = model.validate_services(&validation).unwrap_err();

        assert_eq!(
            error.to_string(),
            "workspaceFolder must be an absolute container path: workspace"
        );
    }
    #[test]
    fn compose_config_service_deserializes_startup_values() {
        let service: ComposeConfigService = serde_json::from_value(serde_json::json!({
            "image": "alpine:3.20",
            "entrypoint": ["/entrypoint.sh", "--flag"],
            "command": "server --port 3000"
        }))
        .unwrap();

        assert_eq!(
            service.entrypoint,
            Some(vec!["/entrypoint.sh".to_owned(), "--flag".to_owned()])
        );
        assert_eq!(service.command, Some(vec!["server --port 3000".to_owned()]));
    }

    #[test]
    fn compose_config_service_treats_null_startup_as_image_default() {
        let service: ComposeConfigService = serde_json::from_value(serde_json::json!({
            "image": "alpine:3.20",
            "entrypoint": null,
            "command": null
        }))
        .unwrap();

        assert_eq!(service.entrypoint, None);
        assert_eq!(service.command, None);
    }

    #[test]
    fn compose_config_service_preserves_empty_startup_override() {
        let service: ComposeConfigService = serde_json::from_value(serde_json::json!({
            "image": "alpine:3.20",
            "entrypoint": [],
            "command": ""
        }))
        .unwrap();

        assert_eq!(service.entrypoint, Some(Vec::new()));
        assert_eq!(service.command, Some(Vec::new()));
    }
}
