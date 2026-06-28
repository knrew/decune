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
            .map_err(|error| compose_ps_parse_error(project_name, service, &error)),
        Ok(JsonValue::Object(_)) => serde_json::from_slice(stdout)
            .map(|container| vec![container])
            .map_err(|error| compose_ps_parse_error(project_name, service, &error)),
        Ok(other) => Err(anyhow!(
            "Failed to parse Docker Compose ps JSON for project {project_name} service `{service}`: expected object or array, got {other}"
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
                    "Failed to parse Docker Compose ps JSON for project {project_name} service `{service}`: {first_error}; JSON Lines parse failed: {line_error}"
                )
            })
        }
    }
}

fn compose_ps_parse_error(
    project_name: &str,
    service: &str,
    error: &serde_json::Error,
) -> anyhow::Error {
    anyhow!(
        "Failed to parse Docker Compose ps JSON for project {project_name} service `{service}`: {error}"
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
        1 => containers.into_iter().next().ok_or_else(|| {
            anyhow!(
                "Docker Compose project {project_name} service `{service}` has no running container"
            )
        }),
        count => Err(anyhow!(
            "Docker Compose project {project_name} service `{service}` has {count} containers; expected exactly one"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_ps_json_accepts_single_object_output() {
        let containers = parse_compose_ps_json(
                br#"{"ID":"app-id","Name":"project-app-1","Service":"app","State":"running","Publishers":null}"#,
                "decune-project-abc123",
                "app",
            )
            .unwrap();

        assert_eq!(containers.len(), 1);
        assert_eq!(containers[0].id, "app-id");
        assert_eq!(containers[0].service, "app");
        assert!(containers[0].published_ports.is_empty());
    }

    #[test]
    fn compose_ps_json_accepts_array_output() {
        let containers = parse_compose_ps_json(
                br#"[{"ID":"app-id","Name":"project-app-1","Service":"app","State":"running","Publishers":[]}]"#,
                "decune-project-abc123",
                "app",
            )
            .unwrap();

        assert_eq!(containers.len(), 1);
        assert_eq!(containers[0].id, "app-id");
    }

    #[test]
    fn compose_ps_json_accepts_json_lines_output() {
        let containers = parse_compose_ps_json(
                b"{\"ID\":\"app-id\",\"Name\":\"project-app-1\",\"Service\":\"app\",\"State\":\"running\",\"Publishers\":[]}\n{\"ID\":\"sidecar-id\",\"Name\":\"project-sidecar-1\",\"Service\":\"sidecar\",\"State\":\"running\",\"Publishers\":[]}\n",
                "decune-project-abc123",
                "app",
            )
            .unwrap();

        assert_eq!(containers.len(), 2);
        assert_eq!(containers[0].id, "app-id");
        assert_eq!(containers[1].service, "sidecar");
    }
    #[test]
    fn compose_ps_fixture_resolves_single_container_id() {
        let containers = serde_json::from_str(
            r#"
                [
                  {
                    "ID": "abc123",
                    "Name": "project-app-1",
                    "Service": "app",
                    "State": "running",
                    "Publishers": [
                      {"URL": "127.0.0.1", "TargetPort": 3000, "PublishedPort": 3000, "Protocol": "tcp"}
                    ]
                  }
                ]
                "#,
        )
        .unwrap();

        let container =
            resolve_compose_container("decune-project-abc123", "app", containers).unwrap();

        assert_eq!(container.id, "abc123");
        assert_eq!(container.service, "app");
        assert_eq!(container.state.as_deref(), Some("running"));
        assert_eq!(container.published_ports.len(), 1);
    }

    #[test]
    fn compose_ps_fixture_treats_null_publishers_as_empty_ports() {
        let containers = serde_json::from_str(
            r#"
                [
                  {
                    "ID": "abc123",
                    "Name": "project-app-1",
                    "Service": "app",
                    "State": "running",
                    "Publishers": null
                  }
                ]
                "#,
        )
        .unwrap();

        let container =
            resolve_compose_container("decune-project-abc123", "app", containers).unwrap();

        assert_eq!(container.id, "abc123");
        assert!(container.published_ports.is_empty());
    }

    #[test]
    fn compose_ps_resolution_rejects_zero_containers() {
        let containers = serde_json::from_str("[]").unwrap();

        let error =
            resolve_compose_container("decune-project-abc123", "app", containers).unwrap_err();

        assert_eq!(
            error.to_string(),
            "Docker Compose project decune-project-abc123 service `app` has no running container"
        );
    }

    #[test]
    fn compose_ps_resolution_rejects_multiple_containers() {
        let containers = serde_json::from_str(
            r#"
                [
                  {"ID": "abc123", "Name": "project-app-1", "Service": "app"},
                  {"ID": "def456", "Name": "project-app-2", "Service": "app"}
                ]
                "#,
        )
        .unwrap();

        let error =
            resolve_compose_container("decune-project-abc123", "app", containers).unwrap_err();

        assert_eq!(
            error.to_string(),
            "Docker Compose project decune-project-abc123 service `app` has 2 containers; expected exactly one"
        );
    }
}
