use std::{
    fs::{self, OpenOptions},
    io::ErrorKind,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

pub(crate) const FEATURE_LOCK_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FeatureRef {
    Oci(OciFeatureRef),
    Local(LocalFeatureRef),
}

impl FeatureRef {
    pub(crate) fn canonical_id(&self) -> &str {
        match self {
            Self::Oci(reference) => &reference.canonical_id,
            Self::Local(reference) => &reference.canonical_id,
        }
    }

    pub(crate) fn original(&self) -> &str {
        match self {
            Self::Oci(reference) => &reference.original,
            Self::Local(reference) => &reference.original,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OciFeatureRef {
    pub(crate) original: String,
    pub(crate) registry: String,
    pub(crate) repository: String,
    pub(crate) feature_id: String,
    pub(crate) tag: Option<String>,
    pub(crate) digest: Option<String>,
    pub(crate) canonical_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalFeatureRef {
    pub(crate) original: String,
    pub(crate) path: PathBuf,
    pub(crate) canonical_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FeatureLockFile {
    pub(crate) version: u32,
    #[serde(default)]
    pub(crate) features: Vec<FeatureLockEntry>,
}

impl FeatureLockFile {
    pub(crate) fn empty() -> Self {
        Self {
            version: FEATURE_LOCK_VERSION,
            features: Vec::new(),
        }
    }

    pub(crate) fn sorted(&self) -> Self {
        let mut sorted = self.clone();
        sorted.features.sort_by(|left, right| {
            left.id
                .cmp(&right.id)
                .then_with(|| left.reference.cmp(&right.reference))
                .then_with(|| left.digest.cmp(&right.digest))
        });
        sorted
    }

    pub(crate) fn digest_for(&self, feature_id: &str) -> Option<&str> {
        self.features
            .iter()
            .find(|entry| entry.id == feature_id)
            .map(|entry| entry.digest.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FeatureLockEntry {
    pub(crate) id: String,
    #[serde(rename = "ref")]
    pub(crate) reference: String,
    pub(crate) digest: String,
}

pub(crate) fn parse_feature_ref(value: &str) -> Result<FeatureRef> {
    parse_oci_feature_ref(value).map(FeatureRef::Oci)
}

pub(crate) fn parse_feature_ref_from_devcontainer_dir(
    value: &str,
    devcontainer_dir: &Path,
) -> Result<FeatureRef> {
    if value.starts_with("./") {
        return Ok(FeatureRef::Local(parse_local_feature_ref(
            value,
            devcontainer_dir,
        )?));
    }

    parse_feature_ref(value)
}

pub(crate) fn read_feature_lock_file(path: &Path) -> Result<FeatureLockFile> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(FeatureLockFile::empty()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to read feature lock file: {}", path.display()));
        }
    };

    let lock: FeatureLockFile = toml::from_str(&content)
        .with_context(|| format!("Failed to parse feature lock file: {}", path.display()))?;
    if lock.version != FEATURE_LOCK_VERSION {
        bail!(
            "Unsupported feature lock version {} in {}",
            lock.version,
            path.display()
        );
    }

    Ok(lock.sorted())
}

#[allow(dead_code)]
pub(crate) fn write_feature_lock_file(path: &Path, lock: &FeatureLockFile) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("Feature lock path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "Failed to create feature lock directory: {}",
            parent.display()
        )
    })?;

    let sorted = lock.sorted();
    let content = toml::to_string(&sorted)
        .with_context(|| format!("Failed to serialize feature lock file: {}", path.display()))?;
    let temp_path = create_temp_lock_file(path, content.as_bytes())?;
    fs::rename(&temp_path, path).with_context(|| {
        format!(
            "Failed to replace feature lock file {} with {}",
            path.display(),
            temp_path.display()
        )
    })?;

    Ok(())
}

pub(crate) fn resolve_locked_feature_ref(
    feature: &FeatureRef,
    lock: &FeatureLockFile,
    update_features: bool,
) -> String {
    if update_features {
        return feature.original().to_owned();
    }

    match feature {
        FeatureRef::Oci(reference) => {
            if let Some(digest) = lock.digest_for(&reference.canonical_id) {
                format!("{}@{}", reference.canonical_id, digest)
            } else {
                reference.original.clone()
            }
        }
        FeatureRef::Local(reference) => reference.path.display().to_string(),
    }
}

fn parse_oci_feature_ref(value: &str) -> Result<OciFeatureRef> {
    let (without_digest, digest) = split_digest(value)?;
    let (without_tag, tag) = split_tag(without_digest);
    let (registry, path) = without_tag
        .split_once('/')
        .ok_or_else(|| invalid_feature_ref(value, "missing registry or repository"))?;
    let last_slash = path
        .rfind('/')
        .ok_or_else(|| invalid_feature_ref(value, "missing repository or feature id"))?;
    let repository = &path[..last_slash];
    let feature_id = &path[last_slash + 1..];

    if registry.is_empty()
        || repository.is_empty()
        || feature_id.is_empty()
        || tag.is_some_and(str::is_empty)
        || (tag.is_none() && digest.is_none())
    {
        return Err(invalid_feature_ref(
            value,
            "expected <registry>/<repository>/<feature-id>:<tag> or @<digest>",
        ));
    }

    let canonical_id = format!("{registry}/{repository}/{feature_id}");

    Ok(OciFeatureRef {
        original: value.to_owned(),
        registry: registry.to_owned(),
        repository: repository.to_owned(),
        feature_id: feature_id.to_owned(),
        tag: tag.map(str::to_owned),
        digest: digest.map(str::to_owned),
        canonical_id,
    })
}

fn parse_local_feature_ref(value: &str, devcontainer_dir: &Path) -> Result<LocalFeatureRef> {
    let relative = value
        .strip_prefix("./")
        .filter(|path| !path.is_empty())
        .ok_or_else(|| invalid_feature_ref(value, "local feature path is empty"))?;

    Ok(LocalFeatureRef {
        original: value.to_owned(),
        path: devcontainer_dir.join(relative),
        canonical_id: format!("local:{relative}"),
    })
}

fn split_digest(value: &str) -> Result<(&str, Option<&str>)> {
    match value.split_once('@') {
        Some((base, digest)) if !base.is_empty() && !digest.is_empty() => Ok((base, Some(digest))),
        Some(_) => Err(invalid_feature_ref(value, "invalid digest")),
        None => Ok((value, None)),
    }
}

fn split_tag(value: &str) -> (&str, Option<&str>) {
    let last_slash = value.rfind('/');
    let last_colon = value.rfind(':');

    match (last_slash, last_colon) {
        (_, Some(colon)) if last_slash.is_none_or(|slash| colon > slash) => {
            (&value[..colon], Some(&value[colon + 1..]))
        }
        _ => (value, None),
    }
}

fn invalid_feature_ref(value: &str, reason: &str) -> anyhow::Error {
    anyhow!("Invalid feature ref `{value}`: {reason}")
}

fn create_temp_lock_file(path: &Path, content: &[u8]) -> Result<PathBuf> {
    for attempt in 0..100 {
        let temp_path = path.with_extension(format!("lock.tmp.{}.{}", std::process::id(), attempt));
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "Failed to create temporary lock file: {}",
                        temp_path.display()
                    )
                });
            }
        };
        std::io::Write::write_all(&mut file, content).with_context(|| {
            format!(
                "Failed to write temporary lock file: {}",
                temp_path.display()
            )
        })?;
        file.sync_all().with_context(|| {
            format!(
                "Failed to sync temporary lock file: {}",
                temp_path.display()
            )
        })?;
        return Ok(temp_path);
    }

    bail!(
        "Failed to create temporary feature lock file for {}",
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use super::*;

    #[test]
    fn parses_tagged_oci_feature_ref() {
        let reference = parse_feature_ref("ghcr.io/devcontainers/features/go:1").unwrap();

        assert_eq!(
            reference,
            FeatureRef::Oci(OciFeatureRef {
                original: "ghcr.io/devcontainers/features/go:1".to_owned(),
                registry: "ghcr.io".to_owned(),
                repository: "devcontainers/features".to_owned(),
                feature_id: "go".to_owned(),
                tag: Some("1".to_owned()),
                digest: None,
                canonical_id: "ghcr.io/devcontainers/features/go".to_owned(),
            })
        );
    }

    #[test]
    fn parses_digest_oci_feature_ref_with_registry_port() {
        let reference =
            parse_feature_ref("localhost:5000/devcontainers/features/tool@sha256:abcd").unwrap();

        assert_eq!(
            reference,
            FeatureRef::Oci(OciFeatureRef {
                original: "localhost:5000/devcontainers/features/tool@sha256:abcd".to_owned(),
                registry: "localhost:5000".to_owned(),
                repository: "devcontainers/features".to_owned(),
                feature_id: "tool".to_owned(),
                tag: None,
                digest: Some("sha256:abcd".to_owned()),
                canonical_id: "localhost:5000/devcontainers/features/tool".to_owned(),
            })
        );
    }

    #[test]
    fn invalid_feature_ref_error_includes_ref() {
        let error = parse_feature_ref("ghcr.io/features").unwrap_err();

        assert!(error.to_string().contains("ghcr.io/features"), "{error:#}");

        let error = parse_feature_ref("ghcr.io/example/features/tool:").unwrap_err();

        assert!(
            error.to_string().contains("ghcr.io/example/features/tool:"),
            "{error:#}"
        );
    }

    #[test]
    fn local_feature_path_is_resolved_from_devcontainer_dir() {
        let devcontainer_dir = Path::new("/workspace/.devcontainer");
        let reference =
            parse_feature_ref_from_devcontainer_dir("./features/local", devcontainer_dir).unwrap();

        assert_eq!(
            reference,
            FeatureRef::Local(LocalFeatureRef {
                original: "./features/local".to_owned(),
                path: devcontainer_dir.join("features/local"),
                canonical_id: "local:features/local".to_owned(),
            })
        );
    }

    #[test]
    fn lock_file_round_trip_is_deterministic() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".decune/features.lock.toml");
        let lock = FeatureLockFile {
            version: FEATURE_LOCK_VERSION,
            features: vec![
                FeatureLockEntry {
                    id: "ghcr.io/example/features/b".to_owned(),
                    reference: "ghcr.io/example/features/b:1".to_owned(),
                    digest: "sha256:bbbb".to_owned(),
                },
                FeatureLockEntry {
                    id: "ghcr.io/example/features/a".to_owned(),
                    reference: "ghcr.io/example/features/a:1".to_owned(),
                    digest: "sha256:aaaa".to_owned(),
                },
            ],
        };

        write_feature_lock_file(&path, &lock).unwrap();
        let first = fs::read_to_string(&path).unwrap();
        write_feature_lock_file(&path, &lock).unwrap();
        let second = fs::read_to_string(&path).unwrap();

        assert_eq!(first, second);
        assert_eq!(read_feature_lock_file(&path).unwrap(), lock.sorted());
    }

    #[test]
    fn lock_digest_takes_precedence_unless_features_are_updated() {
        let feature = parse_feature_ref("ghcr.io/example/features/tool:1").unwrap();
        let lock = FeatureLockFile {
            version: FEATURE_LOCK_VERSION,
            features: vec![FeatureLockEntry {
                id: "ghcr.io/example/features/tool".to_owned(),
                reference: "ghcr.io/example/features/tool:1".to_owned(),
                digest: "sha256:locked".to_owned(),
            }],
        };

        assert_eq!(
            resolve_locked_feature_ref(&feature, &lock, false),
            "ghcr.io/example/features/tool@sha256:locked"
        );
        assert_eq!(
            resolve_locked_feature_ref(&feature, &lock, true),
            "ghcr.io/example/features/tool:1"
        );
    }
}
