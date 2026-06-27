use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

use crate::devcontainer::ports::DevcontainerPort;

pub(super) fn deserialize_build_args<'de, D>(
    deserializer: D,
) -> std::result::Result<BTreeMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = BTreeMap::<String, Value>::deserialize(deserializer)?;
    let mut args = BTreeMap::new();

    for (key, value) in value {
        let value = value.as_str().ok_or_else(|| {
            serde::de::Error::custom(format!("build.args.{key} must be a string"))
        })?;
        args.insert(key, value.to_owned());
    }

    Ok(args)
}

pub(super) fn deserialize_ports<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<DevcontainerPort>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;

    match value {
        None => Ok(Vec::new()),
        Some(Value::Array(values)) => values
            .into_iter()
            .map(serde_json::from_value)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(serde::de::Error::custom),
        Some(value) => serde_json::from_value(value)
            .map(|port| vec![port])
            .map_err(serde::de::Error::custom),
    }
}

pub(super) fn deserialize_string_or_strings<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;

    match value {
        None => Ok(Vec::new()),
        Some(Value::String(value)) => Ok(vec![value]),
        Some(Value::Array(values)) => values
            .into_iter()
            .map(serde_json::from_value)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(serde::de::Error::custom),
        Some(_) => Err(serde::de::Error::custom("expected string or string array")),
    }
}
