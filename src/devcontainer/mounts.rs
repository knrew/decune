use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

use crate::config::layer::LayerDevcontainerMount;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub(crate) enum DevcontainerMount {
    String(String),
    Object(BTreeMap<String, Value>),
}

impl DevcontainerMount {
    pub(crate) fn to_layer(&self) -> LayerDevcontainerMount {
        match self {
            Self::String(value) => LayerDevcontainerMount::String(value.clone()),
            Self::Object(values) => LayerDevcontainerMount::Object(values.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parses_string_mount() {
        let mount: DevcontainerMount =
            serde_json::from_value(json!("source=cache,target=/cache,type=volume")).unwrap();

        assert_eq!(
            mount,
            DevcontainerMount::String("source=cache,target=/cache,type=volume".to_owned())
        );
    }

    #[test]
    fn parses_object_mount() {
        let mount: DevcontainerMount = serde_json::from_value(json!({
            "type": "bind",
            "source": "/host",
            "target": "/container",
            "readonly": true
        }))
        .unwrap();

        match mount.to_layer() {
            LayerDevcontainerMount::Object(values) => {
                assert_eq!(values.get("type"), Some(&json!("bind")));
                assert_eq!(values.get("source"), Some(&json!("/host")));
                assert_eq!(values.get("target"), Some(&json!("/container")));
                assert_eq!(values.get("readonly"), Some(&json!(true)));
            }
            LayerDevcontainerMount::String(_) => panic!("expected object mount"),
        }
    }
}
