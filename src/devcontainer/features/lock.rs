use std::{
    fs::{self, OpenOptions},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use super::{FeatureRef, OciFeatureRef, reference::parse_feature_ref};

pub(crate) const FEATURE_LOCK_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FeatureLockFile {
    pub(crate) version: u32,
    #[serde(default)]
    pub(crate) features: Vec<FeatureLockEntry>,
}

impl FeatureLockFile {
    pub(crate) const fn empty() -> Self {
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

    pub(crate) fn digest_for_reference(&self, reference: &OciFeatureRef) -> Option<&str> {
        self.features
            .iter()
            .find(|entry| {
                crate::config::layer::canonical_feature_id(&entry.id) == reference.canonical_id
                    && feature_lock_reference_matches(&entry.reference, reference)
            })
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

pub(crate) fn remove_feature_lock_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("Failed to remove feature lock file: {}", path.display())),
    }
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
            if let Some(digest) = lock.digest_for_reference(reference) {
                format!("{}@{}", reference.canonical_id, digest)
            } else {
                reference.original.clone()
            }
        }
        FeatureRef::Local(reference) => reference.path.display().to_string(),
    }
}

fn feature_lock_reference_matches(locked: &str, requested: &OciFeatureRef) -> bool {
    let Ok(FeatureRef::Oci(locked)) = parse_feature_ref(locked) else {
        return false;
    };

    oci_feature_ref_lock_key(&locked) == oci_feature_ref_lock_key(requested)
}

fn oci_feature_ref_lock_key(reference: &OciFeatureRef) -> String {
    if let Some(digest) = &reference.digest {
        format!("{}@{digest}", reference.canonical_id)
    } else {
        format!(
            "{}:{}",
            reference.canonical_id,
            reference.tag.as_deref().unwrap_or("latest")
        )
    }
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
        file.write_all(content).with_context(|| {
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
    use super::*;
    use crate::devcontainer::features::reference::parse_oci_feature_ref;

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
        let feature = parse_oci_feature_ref("ghcr.io/example/features/tool:1").unwrap();
        let feature = FeatureRef::Oci(feature);
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

    #[test]
    fn lock_digest_is_ignored_when_reference_changed() {
        let feature = parse_oci_feature_ref("ghcr.io/example/features/tool:2").unwrap();
        let feature = FeatureRef::Oci(feature);
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
            "ghcr.io/example/features/tool:2"
        );
    }
}
