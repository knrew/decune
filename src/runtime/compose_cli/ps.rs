use anyhow::{Result, anyhow};
use serde::{Deserialize, Deserializer};
use serde_json::Value as JsonValue;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct ComposePsContainer {
    #[serde(alias = "Id", rename = "ID")]
    pub(crate) id: String,
    #[serde(default, rename = "Name")]
    pub(crate) name: Option<String>,
    #[serde(rename = "Service")]
    pub(crate) service: String,
    #[serde(default, rename = "State")]
    pub(crate) state: Option<String>,
    #[serde(
        default,
        rename = "Publishers",
        deserialize_with = "deserialize_null_as_empty_vec"
    )]
    pub(crate) published_ports: Vec<ComposePublishedPort>,
}

pub(super) fn parse_compose_ps_json(
    stdout: &[u8],
    project_name: &str,
    service: &str,
) -> Result<Vec<ComposePsContainer>> {
    if stdout.iter().all(u8::is_ascii_whitespace) {
        return Ok(Vec::new());
    }

    match serde_json::from_slice::<JsonValue>(stdout) {
        Ok(JsonValue::Array(values)) => values
            .into_iter()
            .map(serde_json::from_value)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| compose_ps_parse_error(project_name, service, error)),
        Ok(JsonValue::Object(_)) => serde_json::from_slice(stdout)
            .map(|container| vec![container])
            .map_err(|error| compose_ps_parse_error(project_name, service, error)),
        Ok(other) => Err(anyhow!(
            "Failed to parse Docker Compose ps JSON for project {} service `{service}`: expected object or array, got {other}",
            project_name
        )),
        Err(first_error) => {
            let lines = String::from_utf8_lossy(stdout);
            let containers = lines
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(serde_json::from_str::<ComposePsContainer>)
                .collect::<std::result::Result<Vec<_>, _>>();
            containers.map_err(|line_error| {
                anyhow!(
                    "Failed to parse Docker Compose ps JSON for project {} service `{service}`: {first_error}; JSON Lines parse failed: {line_error}",
                    project_name
                )
            })
        }
    }
}

fn compose_ps_parse_error(
    project_name: &str,
    service: &str,
    error: serde_json::Error,
) -> anyhow::Error {
    anyhow!(
        "Failed to parse Docker Compose ps JSON for project {} service `{service}`: {error}",
        project_name
    )
}

fn deserialize_null_as_empty_vec<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct ComposePublishedPort {
    #[serde(default, rename = "URL")]
    pub(crate) url: Option<String>,
    #[serde(default, rename = "TargetPort")]
    pub(crate) target_port: Option<u16>,
    #[serde(default, rename = "PublishedPort")]
    pub(crate) published_port: Option<u16>,
    #[serde(default, rename = "Protocol")]
    pub(crate) protocol: Option<String>,
}

pub(crate) fn resolve_compose_container(
    project_name: &str,
    service: &str,
    containers: Vec<ComposePsContainer>,
) -> Result<ComposePsContainer> {
    match containers.len() {
        0 => Err(anyhow!(
            "Docker Compose project {project_name} service `{service}` has no running container"
        )),
        1 => Ok(containers
            .into_iter()
            .next()
            .expect("container length checked before extraction")),
        count => Err(anyhow!(
            "Docker Compose project {project_name} service `{service}` has {count} containers; expected exactly one"
        )),
    }
}
