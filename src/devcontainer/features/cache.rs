use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::hex::hex_lower;

use super::{
    archive::{extract_feature_archive, find_required_feature_file},
    cache_lock::FeatureCacheLock,
    reference::{OciFeatureRef, validate_oci_digest},
    registry::{OciLayerDescriptor, OciManifestResponse, OciRegistryClient},
};

const DEVCONTAINER_FEATURE_LAYER_MEDIA_TYPE: &str = "application/vnd.devcontainers.layer.v1+tar";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct FeatureCacheMetadata {
    pub(super) manifest_digest: String,
    pub(super) layer_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OciFeatureArtifact {
    pub(crate) digest: String,
    pub(crate) archive_path: PathBuf,
    pub(crate) extracted_dir: PathBuf,
    pub(crate) install_script_path: PathBuf,
    pub(crate) metadata_path: PathBuf,
    pub(crate) cache_hit: bool,
}

pub(crate) fn pull_oci_feature_with_client(
    reference: &OciFeatureRef,
    cache_root: &Path,
    extract_root: &Path,
    registry: &dyn OciRegistryClient,
) -> Result<OciFeatureArtifact> {
    let _lock = FeatureCacheLock::acquire_shared(cache_root)?;
    fs::create_dir_all(cache_root).with_context(|| {
        format!(
            "Failed to create feature cache directory: {}",
            cache_root.display()
        )
    })?;

    if let Some(digest) = &reference.digest {
        validate_oci_digest(digest).with_context(|| {
            format!(
                "Invalid OCI manifest digest for feature {}: {digest}",
                reference.original
            )
        })?;
        let archive_path = feature_cache_archive_path(cache_root, digest);
        if cached_feature_archive_is_valid(&archive_path, digest)?.is_some() {
            return extract_cached_feature(reference, digest, archive_path, extract_root, true);
        }
    }

    let manifest = registry.fetch_manifest(reference).with_context(|| {
        format!(
            "Failed to resolve OCI manifest for feature {}",
            reference.original
        )
    })?;
    validate_oci_digest(&manifest.digest).with_context(|| {
        format!(
            "Invalid OCI manifest digest for feature {}: {}",
            reference.original, manifest.digest
        )
    })?;
    let archive_path = feature_cache_archive_path(cache_root, &manifest.digest);
    if cached_feature_archive_is_valid(&archive_path, &manifest.digest)?.is_some() {
        return extract_cached_feature(
            reference,
            &manifest.digest,
            archive_path,
            extract_root,
            true,
        );
    }

    let layer = select_feature_archive_layer(&manifest).with_context(|| {
        format!(
            "OCI manifest for feature {} ({}) does not include a feature archive layer",
            reference.original, manifest.digest
        )
    })?;
    validate_oci_digest(&layer.digest).with_context(|| {
        format!(
            "Invalid OCI feature layer digest for {}: {}",
            reference.original, layer.digest
        )
    })?;
    let blob = registry
        .fetch_blob(reference, &layer.digest)
        .with_context(|| {
            format!(
                "Failed to fetch OCI feature layer for {} ({})",
                reference.original, layer.digest
            )
        })?;
    verify_digest(&blob, &layer.digest).with_context(|| {
        format!(
            "OCI feature layer digest mismatch for {} ({})",
            reference.original, layer.digest
        )
    })?;
    write_cache_archive(
        &archive_path,
        &blob,
        &FeatureCacheMetadata {
            manifest_digest: manifest.digest.clone(),
            layer_digest: layer.digest.clone(),
        },
    )?;

    extract_cached_feature(
        reference,
        &manifest.digest,
        archive_path,
        extract_root,
        false,
    )
}

fn extract_cached_feature(
    reference: &OciFeatureRef,
    digest: &str,
    archive_path: PathBuf,
    extract_root: &Path,
    cache_hit: bool,
) -> Result<OciFeatureArtifact> {
    let extracted_dir = extract_root.join(cache_safe_digest(digest));
    extract_feature_archive(&archive_path, &extracted_dir).with_context(|| {
        format!(
            "Failed to extract cached feature archive for {} ({digest})",
            reference.original
        )
    })?;
    let install_script_path = find_required_feature_file(&extracted_dir, "install.sh")?;
    let metadata_path = find_required_feature_file(&extracted_dir, "devcontainer-feature.json")?;

    Ok(OciFeatureArtifact {
        digest: digest.to_owned(),
        archive_path,
        extracted_dir,
        install_script_path,
        metadata_path,
        cache_hit,
    })
}

pub(super) fn feature_cache_archive_path(cache_root: &Path, digest: &str) -> PathBuf {
    cache_root.join(format!("{}.tgz", cache_safe_digest(digest)))
}

fn feature_cache_metadata_path(archive_path: &Path) -> PathBuf {
    archive_path.with_extension("tgz.toml")
}

pub(super) fn select_feature_archive_layer(
    manifest: &OciManifestResponse,
) -> Option<&OciLayerDescriptor> {
    manifest
        .layers
        .iter()
        .find(|layer| layer.media_type == DEVCONTAINER_FEATURE_LAYER_MEDIA_TYPE)
}

fn cached_feature_archive_is_valid(
    archive_path: &Path,
    manifest_digest: &str,
) -> Result<Option<FeatureCacheMetadata>> {
    if !archive_path.exists() {
        return Ok(None);
    }
    let metadata_path = feature_cache_metadata_path(archive_path);
    if !metadata_path.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(&metadata_path).with_context(|| {
        format!(
            "Failed to read feature cache metadata: {}",
            metadata_path.display()
        )
    })?;
    let metadata: FeatureCacheMetadata = toml::from_str(&content).with_context(|| {
        format!(
            "Failed to parse feature cache metadata: {}",
            metadata_path.display()
        )
    })?;
    if metadata.manifest_digest != manifest_digest {
        return Ok(None);
    }
    let archive = fs::read(archive_path).with_context(|| {
        format!(
            "Failed to read feature cache archive: {}",
            archive_path.display()
        )
    })?;
    verify_digest(&archive, &metadata.layer_digest).with_context(|| {
        format!(
            "Feature cache archive digest mismatch: {}",
            archive_path.display()
        )
    })?;

    Ok(Some(metadata))
}

pub(super) fn write_cache_archive(
    path: &Path,
    content: &[u8],
    metadata: &FeatureCacheMetadata,
) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        anyhow!(
            "Feature cache archive path has no parent: {}",
            path.display()
        )
    })?;
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "Failed to create feature cache directory: {}",
            parent.display()
        )
    })?;
    let temp_path = path.with_extension(format!("tgz.tmp.{}", std::process::id()));
    fs::write(&temp_path, content).with_context(|| {
        format!(
            "Failed to write temporary feature cache archive: {}",
            temp_path.display()
        )
    })?;
    fs::rename(&temp_path, path).with_context(|| {
        format!(
            "Failed to replace feature cache archive {} with {}",
            path.display(),
            temp_path.display()
        )
    })?;

    let metadata_path = feature_cache_metadata_path(path);
    let metadata_content = toml::to_string(metadata).with_context(|| {
        format!(
            "Failed to serialize feature cache metadata: {}",
            metadata_path.display()
        )
    })?;
    let temp_metadata_path =
        metadata_path.with_extension(format!("toml.tmp.{}", std::process::id()));
    fs::write(&temp_metadata_path, metadata_content).with_context(|| {
        format!(
            "Failed to write temporary feature cache metadata: {}",
            temp_metadata_path.display()
        )
    })?;
    fs::rename(&temp_metadata_path, &metadata_path).with_context(|| {
        format!(
            "Failed to replace feature cache metadata {} with {}",
            metadata_path.display(),
            temp_metadata_path.display()
        )
    })?;

    Ok(())
}

fn verify_digest(content: &[u8], digest: &str) -> Result<()> {
    validate_oci_digest(digest)?;
    let expected = digest
        .strip_prefix("sha256:")
        .expect("sha256 digest was validated");
    let actual = hex_lower(&Sha256::digest(content));
    if actual != expected {
        bail!("Expected {digest}, got sha256:{actual}");
    }

    Ok(())
}

fn cache_safe_digest(digest: &str) -> String {
    digest
        .chars()
        .map(|value| {
            if value.is_ascii_alphanumeric() {
                value
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, io::Write};

    use flate2::{Compression, write::GzEncoder};
    use sha2::{Digest, Sha256};
    use tar::{Builder, Header};

    use super::*;
    use crate::devcontainer::features::{
        OciFeatureRef,
        reference::parse_oci_feature_ref,
        registry::{HttpOciRegistryClient, OciManifestResponse, OciRegistryClient},
    };

    const MANIFEST_DIGEST_HEX: &str =
        "1111111111111111111111111111111111111111111111111111111111111111";

    #[test]
    fn feature_archive_requires_feature_files_at_archive_root() {
        let temp = tempfile::tempdir().unwrap();
        let cache_root = temp.path().join("cache");
        fs::create_dir_all(&cache_root).unwrap();
        let digest = sha256_digest(MANIFEST_DIGEST_HEX);
        let archive = feature_cache_archive_path(&cache_root, &digest);
        let source_archive = temp.path().join("source.tgz");
        write_feature_archive(
            &source_archive,
            &[
                ("feature/install.sh", b"#!/bin/sh\n".as_slice()),
                (
                    "feature/devcontainer-feature.json",
                    br#"{"id":"tool","version":"1.0.0","name":"Tool"}"#.as_slice(),
                ),
            ],
        );
        let blob = fs::read(&source_archive).unwrap();
        let layer_digest = format!("sha256:{}", hex_lower(&Sha256::digest(&blob)));
        write_cache_archive(
            &archive,
            &blob,
            &FeatureCacheMetadata {
                manifest_digest: digest.clone(),
                layer_digest,
            },
        )
        .unwrap();
        let reference =
            parse_oci_feature_ref(&format!("ghcr.io/example/features/tool@{digest}")).unwrap();

        let error = pull_oci_feature_with_client(
            &reference,
            &cache_root,
            &temp.path().join("extract"),
            &PanicRegistryClient,
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Feature archive must contain install.sh at its root"),
            "{error:#}"
        );
    }

    #[test]
    fn digest_cache_hit_skips_registry_access() {
        let temp = tempfile::tempdir().unwrap();
        let cache_root = temp.path().join("cache");
        fs::create_dir_all(&cache_root).unwrap();
        let digest = sha256_digest(MANIFEST_DIGEST_HEX);
        let archive = feature_cache_archive_path(&cache_root, &digest);
        let source_archive = temp.path().join("source.tgz");
        write_feature_archive(
            &source_archive,
            &[
                ("install.sh", b"#!/bin/sh\n".as_slice()),
                (
                    "devcontainer-feature.json",
                    br#"{"id":"tool","version":"1.0.0","name":"Tool"}"#.as_slice(),
                ),
            ],
        );
        let blob = fs::read(&source_archive).unwrap();
        let layer_digest = format!("sha256:{}", hex_lower(&Sha256::digest(&blob)));
        write_cache_archive(
            &archive,
            &blob,
            &FeatureCacheMetadata {
                manifest_digest: digest.clone(),
                layer_digest,
            },
        )
        .unwrap();
        let reference =
            parse_oci_feature_ref(&format!("ghcr.io/example/features/tool@{digest}")).unwrap();
        let registry = PanicRegistryClient;

        let artifact = pull_oci_feature_with_client(
            &reference,
            &cache_root,
            &temp.path().join("extract"),
            &registry,
        )
        .unwrap();

        assert!(artifact.cache_hit);
        assert_eq!(artifact.digest, digest);
        assert_eq!(
            artifact
                .archive_path
                .file_name()
                .and_then(|name| name.to_str()),
            Some(format!("sha256_{MANIFEST_DIGEST_HEX}.tgz").as_str())
        );
        assert!(artifact.install_script_path.exists());
        assert!(artifact.metadata_path.exists());
    }

    #[test]
    fn cache_archive_without_integrity_metadata_is_refreshed_from_registry() {
        let temp = tempfile::tempdir().unwrap();
        let cache_root = temp.path().join("cache");
        fs::create_dir_all(&cache_root).unwrap();
        let digest = sha256_digest(MANIFEST_DIGEST_HEX);
        let archive = feature_cache_archive_path(&cache_root, &digest);
        write_feature_archive(
            &archive,
            &[
                ("install.sh", b"#!/bin/sh\n".as_slice()),
                (
                    "devcontainer-feature.json",
                    br#"{"id":"stale","version":"1.0.0","name":"Stale"}"#.as_slice(),
                ),
            ],
        );
        let fresh_archive = temp.path().join("fresh.tgz");
        write_feature_archive(
            &fresh_archive,
            &[
                ("install.sh", b"#!/bin/sh\n".as_slice()),
                (
                    "devcontainer-feature.json",
                    br#"{"id":"fresh","version":"1.0.0","name":"Fresh"}"#.as_slice(),
                ),
            ],
        );
        let blob = fs::read(&fresh_archive).unwrap();
        let layer_digest = format!("sha256:{}", hex_lower(&Sha256::digest(&blob)));
        let registry = FakeRegistryClient {
            manifest_calls: Cell::new(0),
            blob_calls: Cell::new(0),
            manifest: OciManifestResponse {
                digest: digest.clone(),
                layers: vec![OciLayerDescriptor {
                    digest: layer_digest,
                    media_type: "application/vnd.devcontainers.layer.v1+tar".to_owned(),
                    size: blob.len() as u64,
                }],
            },
            blob,
        };
        let reference =
            parse_oci_feature_ref(&format!("ghcr.io/example/features/tool@{digest}")).unwrap();

        let artifact = pull_oci_feature_with_client(
            &reference,
            &cache_root,
            &temp.path().join("extract"),
            &registry,
        )
        .unwrap();

        assert!(!artifact.cache_hit);
        assert_eq!(registry.manifest_calls.get(), 1);
        assert_eq!(registry.blob_calls.get(), 1);
        assert!(
            fs::read_to_string(artifact.metadata_path)
                .unwrap()
                .contains("fresh")
        );
    }

    #[test]
    fn cache_archive_with_integrity_mismatch_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let cache_root = temp.path().join("cache");
        fs::create_dir_all(&cache_root).unwrap();
        let digest = sha256_digest(MANIFEST_DIGEST_HEX);
        let archive = feature_cache_archive_path(&cache_root, &digest);
        let source_archive = temp.path().join("source.tgz");
        write_feature_archive(
            &source_archive,
            &[
                ("install.sh", b"#!/bin/sh\n".as_slice()),
                (
                    "devcontainer-feature.json",
                    br#"{"id":"tool","version":"1.0.0","name":"Tool"}"#.as_slice(),
                ),
            ],
        );
        let blob = fs::read(&source_archive).unwrap();
        write_cache_archive(
            &archive,
            &blob,
            &FeatureCacheMetadata {
                manifest_digest: digest.clone(),
                layer_digest: sha256_digest(MANIFEST_DIGEST_HEX),
            },
        )
        .unwrap();
        let reference =
            parse_oci_feature_ref(&format!("ghcr.io/example/features/tool@{digest}")).unwrap();
        let registry = PanicRegistryClient;

        let error = pull_oci_feature_with_client(
            &reference,
            &cache_root,
            &temp.path().join("extract"),
            &registry,
        )
        .unwrap_err();

        assert!(error.to_string().contains("digest mismatch"), "{error:#}");
    }

    #[test]
    fn feature_cache_archive_path_uses_safe_digest_filename() {
        let cache_root = Path::new("/tmp/cache");
        let digest = sha256_digest(MANIFEST_DIGEST_HEX);

        assert_eq!(
            feature_cache_archive_path(cache_root, &digest),
            cache_root.join(format!("sha256_{MANIFEST_DIGEST_HEX}.tgz"))
        );
    }

    #[test]
    fn feature_archive_layer_requires_devcontainer_layer_media_type() {
        let manifest = OciManifestResponse {
            digest: sha256_digest(MANIFEST_DIGEST_HEX),
            layers: vec![OciLayerDescriptor {
                digest: sha256_digest(
                    "1111111111111111111111111111111111111111111111111111111111111111",
                ),
                media_type: "application/vnd.oci.image.layer.v1.tar+gzip".to_owned(),
                size: 12,
            }],
        };

        assert!(select_feature_archive_layer(&manifest).is_none());
    }

    #[test]
    fn feature_archive_layer_ignores_non_feature_tar_layers() {
        let image_layer_digest =
            sha256_digest("1111111111111111111111111111111111111111111111111111111111111111");
        let feature_layer_digest =
            sha256_digest("2222222222222222222222222222222222222222222222222222222222222222");
        let manifest = OciManifestResponse {
            digest: sha256_digest(MANIFEST_DIGEST_HEX),
            layers: vec![
                OciLayerDescriptor {
                    digest: image_layer_digest,
                    media_type: "application/vnd.oci.image.layer.v1.tar+gzip".to_owned(),
                    size: 12,
                },
                OciLayerDescriptor {
                    digest: feature_layer_digest.clone(),
                    media_type: "application/vnd.devcontainers.layer.v1+tar".to_owned(),
                    size: 34,
                },
            ],
        };

        assert_eq!(
            select_feature_archive_layer(&manifest).map(|layer| layer.digest.as_str()),
            Some(feature_layer_digest.as_str())
        );
    }

    #[test]
    fn registry_pull_caches_and_extracts_feature_archive() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("source.tgz");
        write_feature_archive(
            &archive,
            &[
                ("install.sh", b"#!/bin/sh\n".as_slice()),
                (
                    "devcontainer-feature.json",
                    br#"{"id":"tool","version":"1.0.0","name":"Tool"}"#.as_slice(),
                ),
            ],
        );
        let blob = fs::read(&archive).unwrap();
        let layer_digest = format!("sha256:{}", hex_lower(&Sha256::digest(&blob)));
        let registry = FakeRegistryClient {
            manifest_calls: Cell::new(0),
            blob_calls: Cell::new(0),
            manifest: OciManifestResponse {
                digest: sha256_digest(MANIFEST_DIGEST_HEX),
                layers: vec![OciLayerDescriptor {
                    digest: layer_digest,
                    media_type: "application/vnd.devcontainers.layer.v1+tar".to_owned(),
                    size: blob.len() as u64,
                }],
            },
            blob,
        };
        let reference = parse_oci_feature_ref("ghcr.io/example/features/tool:1").unwrap();

        let artifact = pull_oci_feature_with_client(
            &reference,
            &temp.path().join("cache"),
            &temp.path().join("extract"),
            &registry,
        )
        .unwrap();

        assert!(!artifact.cache_hit);
        assert_eq!(artifact.digest, sha256_digest(MANIFEST_DIGEST_HEX));
        assert_eq!(registry.manifest_calls.get(), 1);
        assert_eq!(registry.blob_calls.get(), 1);
        assert!(artifact.archive_path.exists());
        assert!(artifact.install_script_path.exists());
        assert!(artifact.metadata_path.exists());
    }

    #[test]
    #[ignore = "requires public OCI registry access"]
    fn pulls_public_devcontainer_feature_from_ghcr() {
        let temp = tempfile::tempdir().unwrap();
        let reference =
            parse_oci_feature_ref("ghcr.io/devcontainers/features/common-utils:2").unwrap();
        let registry = HttpOciRegistryClient::from_docker_config().unwrap();

        let artifact = pull_oci_feature_with_client(
            &reference,
            &temp.path().join("cache"),
            &temp.path().join("extract"),
            &registry,
        )
        .unwrap();

        assert!(artifact.install_script_path.exists());
        assert!(artifact.metadata_path.exists());
    }

    struct PanicRegistryClient;

    impl OciRegistryClient for PanicRegistryClient {
        fn fetch_manifest(&self, _reference: &OciFeatureRef) -> Result<OciManifestResponse> {
            panic!("manifest should not be fetched for cache hit");
        }

        fn fetch_blob(&self, _reference: &OciFeatureRef, _digest: &str) -> Result<Vec<u8>> {
            panic!("blob should not be fetched for cache hit");
        }
    }

    struct FakeRegistryClient {
        manifest_calls: Cell<usize>,
        blob_calls: Cell<usize>,
        manifest: OciManifestResponse,
        blob: Vec<u8>,
    }

    impl OciRegistryClient for FakeRegistryClient {
        fn fetch_manifest(&self, _reference: &OciFeatureRef) -> Result<OciManifestResponse> {
            self.manifest_calls.set(self.manifest_calls.get() + 1);
            Ok(self.manifest.clone())
        }

        fn fetch_blob(&self, _reference: &OciFeatureRef, digest: &str) -> Result<Vec<u8>> {
            self.blob_calls.set(self.blob_calls.get() + 1);
            assert_eq!(digest, self.manifest.layers[0].digest);
            Ok(self.blob.clone())
        }
    }

    fn write_feature_archive(path: &Path, entries: &[(&str, &[u8])]) {
        let file = fs::File::create(path).unwrap();
        let encoder = GzEncoder::new(file, Compression::default());
        let mut builder = Builder::new(encoder);
        for (path, content) in entries {
            let mut header = Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, *path, &mut &content[..])
                .unwrap();
        }
        let encoder = builder.into_inner().unwrap();
        let mut file = encoder.finish().unwrap();
        file.flush().unwrap();
    }

    fn sha256_digest(hex: &str) -> String {
        format!("sha256:{hex}")
    }
}
