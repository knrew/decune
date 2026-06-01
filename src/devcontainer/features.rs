// M10-T02 exposes OCI pull/cache primitives that M10-T04 wires into image build.
#![allow(dead_code)]

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs::{self, OpenOptions},
    io::{ErrorKind, Read, Seek, SeekFrom},
    os::unix::fs::PermissionsExt,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use flate2::read::GzDecoder;
use reqwest::{
    StatusCode,
    blocking::{Client as HttpClient, RequestBuilder, Response},
    header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, WWW_AUTHENTICATE},
};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};
use tar::{Archive, EntryType};

use crate::{
    config::{FeatureLockHashEntry, layer::ConfigLayer, resolved::ResolvedFeature},
    devcontainer::metadata::parse_metadata_layer,
};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OciFeatureArtifact {
    pub(crate) digest: String,
    pub(crate) archive_path: PathBuf,
    pub(crate) extracted_dir: PathBuf,
    pub(crate) install_script_path: PathBuf,
    pub(crate) metadata_path: PathBuf,
    pub(crate) cache_hit: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OciManifestResponse {
    pub(crate) digest: String,
    pub(crate) layers: Vec<OciLayerDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OciLayerDescriptor {
    pub(crate) digest: String,
    pub(crate) media_type: String,
    pub(crate) size: u64,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub(crate) struct FeatureMetadata {
    #[serde(default, rename = "dependsOn")]
    pub(crate) depends_on: BTreeMap<String, serde_json::Value>,
    #[serde(default, rename = "installsAfter")]
    pub(crate) installs_after: Vec<String>,
    #[serde(default)]
    pub(crate) options: BTreeMap<String, FeatureOptionSchema>,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub(crate) struct FeatureOptionSchema {
    #[serde(default, rename = "type")]
    pub(crate) option_type: Option<String>,
    #[serde(default)]
    pub(crate) default: Option<serde_json::Value>,
    #[serde(default, rename = "enum")]
    pub(crate) enum_values: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FeatureMetadataDocument {
    pub(crate) metadata: FeatureMetadata,
    pub(crate) layer: ConfigLayer,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FeatureInstallInput {
    pub(crate) feature: crate::config::resolved::ResolvedFeature,
    pub(crate) metadata: FeatureMetadata,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PreparedFeatureInstallPlan {
    pub(crate) entries: Vec<PreparedFeatureInstallEntry>,
    pub(crate) metadata_layers: Vec<ConfigLayer>,
    pub(crate) lock_entries: Vec<FeatureLockHashEntry>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PreparedFeatureInstallEntry {
    pub(crate) feature: ResolvedFeature,
    pub(crate) source_dir: PathBuf,
    pub(crate) option_env: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
struct FeatureSource {
    source_dir: PathBuf,
    metadata: FeatureMetadata,
    layer: ConfigLayer,
    lock_entry: Option<FeatureLockHashEntry>,
    lock_file_entry: Option<FeatureLockEntry>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FeatureInstallPlanEntry {
    pub(crate) feature: crate::config::resolved::ResolvedFeature,
    pub(crate) metadata: FeatureMetadata,
    pub(crate) option_env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FeatureDependencyRequest<'a> {
    pub(crate) parent_canonical_id: &'a str,
    pub(crate) dependency: &'a str,
    pub(crate) canonical_id: String,
    pub(crate) options: BTreeMap<String, toml::Value>,
}

pub(crate) trait OciRegistryClient {
    fn fetch_manifest(&self, reference: &OciFeatureRef) -> Result<OciManifestResponse>;

    fn fetch_blob(&self, reference: &OciFeatureRef, digest: &str) -> Result<Vec<u8>>;
}

#[derive(Debug, Clone)]
pub(crate) struct HttpOciRegistryClient {
    client: HttpClient,
    auth: DockerConfigAuthStore,
}

impl HttpOciRegistryClient {
    pub(crate) fn from_docker_config() -> Result<Self> {
        Ok(Self {
            client: HttpClient::new(),
            auth: DockerConfigAuthStore::from_default_config()?,
        })
    }
}

impl OciRegistryClient for HttpOciRegistryClient {
    fn fetch_manifest(&self, reference: &OciFeatureRef) -> Result<OciManifestResponse> {
        let selector = reference
            .digest
            .as_ref()
            .or(reference.tag.as_ref())
            .ok_or_else(|| {
                anyhow!(
                    "Feature ref must include a tag or digest before registry fetch: {}",
                    reference.original
                )
            })?;
        let url = registry_url(reference, &format!("manifests/{selector}"));
        let response = self
            .send_registry_request(
                reference,
                self.client.get(&url).header(
                    ACCEPT,
                    HeaderValue::from_static(
                        "application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json",
                    ),
                ),
            )
            .with_context(|| {
                format!(
                    "Failed to fetch OCI manifest for feature {} ({selector})",
                    reference.original
                )
            })?;
        let digest = response
            .headers()
            .get("Docker-Content-Digest")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = response
            .bytes()
            .with_context(|| {
                format!(
                    "Failed to read OCI manifest response for feature {}",
                    reference.original
                )
            })?
            .to_vec();

        parse_registry_manifest_response_body(
            &reference.original,
            reference.digest.as_deref(),
            digest.as_deref(),
            &body,
        )
    }

    fn fetch_blob(&self, reference: &OciFeatureRef, digest: &str) -> Result<Vec<u8>> {
        validate_oci_digest(digest).with_context(|| {
            format!(
                "Invalid OCI blob digest for feature {}: {digest}",
                reference.original
            )
        })?;
        let url = registry_url(reference, &format!("blobs/{digest}"));
        let response = self
            .send_registry_request(reference, self.client.get(&url))
            .with_context(|| {
                format!(
                    "Failed to fetch OCI blob for feature {} ({digest})",
                    reference.original
                )
            })?;

        Ok(response
            .bytes()
            .with_context(|| {
                format!(
                    "Failed to read OCI blob response for feature {} ({digest})",
                    reference.original
                )
            })?
            .to_vec())
    }
}

impl HttpOciRegistryClient {
    fn send_registry_request(
        &self,
        reference: &OciFeatureRef,
        request: RequestBuilder,
    ) -> Result<Response> {
        let request = self.apply_registry_auth(reference, request);
        let response = request.send().with_context(|| {
            format!(
                "Failed to send OCI registry request for feature {}",
                reference.original
            )
        })?;

        if response.status() != StatusCode::UNAUTHORIZED {
            return response.error_for_status().with_context(|| {
                format!(
                    "OCI registry returned an error for feature {}",
                    reference.original
                )
            });
        }

        let Some(challenge) = bearer_challenge(response.headers()) else {
            return response.error_for_status().with_context(|| {
                format!(
                    "OCI registry authentication failed for feature {}",
                    reference.original
                )
            });
        };
        let token = self.fetch_bearer_token(reference, &challenge)?;
        let response = self
            .apply_bearer_auth(
                reference,
                self.client
                    .get(response.url().clone())
                    .header(ACCEPT, registry_accept_header()),
                &token,
            )
            .send()
            .with_context(|| {
                format!(
                    "Failed to retry OCI registry request for feature {}",
                    reference.original
                )
            })?;

        response.error_for_status().with_context(|| {
            format!(
                "OCI registry returned an error for feature {}",
                reference.original
            )
        })
    }

    fn fetch_bearer_token(
        &self,
        reference: &OciFeatureRef,
        challenge: &BearerChallenge,
    ) -> Result<String> {
        let mut request = self.client.get(&challenge.realm);
        if let Some(service) = &challenge.service {
            request = request.query(&[("service", service)]);
        }
        if let Some(scope) = challenge
            .scope
            .clone()
            .or_else(|| Some(format!("repository:{}:pull", reference.repository_path())))
        {
            request = request.query(&[("scope", &scope)]);
        }
        request = self.apply_registry_auth(reference, request);
        let response = request.send().with_context(|| {
            format!(
                "Failed to request OCI registry token for feature {}",
                reference.original
            )
        })?;
        let token: BearerTokenResponse = response
            .error_for_status()
            .with_context(|| {
                format!(
                    "OCI registry token service returned an error for feature {}",
                    reference.original
                )
            })?
            .json()
            .with_context(|| {
                format!(
                    "Failed to parse OCI registry token response for feature {}",
                    reference.original
                )
            })?;

        token.token.or(token.access_token).ok_or_else(|| {
            anyhow!(
                "OCI registry token response did not include a token for feature {}",
                reference.original
            )
        })
    }

    fn apply_registry_auth(
        &self,
        reference: &OciFeatureRef,
        request: RequestBuilder,
    ) -> RequestBuilder {
        match self.auth.get(&reference.registry) {
            Some(RegistryAuth::Basic { username, password }) => {
                request.basic_auth(username, Some(password))
            }
            Some(RegistryAuth::Bearer(token)) => self.apply_bearer_auth(reference, request, token),
            None => request,
        }
    }

    fn apply_bearer_auth(
        &self,
        _reference: &OciFeatureRef,
        request: RequestBuilder,
        token: &str,
    ) -> RequestBuilder {
        request.header(AUTHORIZATION, format!("Bearer {token}"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RegistryAuth {
    Basic { username: String, password: String },
    Bearer(String),
}

#[derive(Debug, Clone)]
pub(crate) struct DockerConfigAuth;

impl DockerConfigAuth {
    pub(crate) fn from_config_file(path: &Path, registry: &str) -> Result<Option<RegistryAuth>> {
        DockerConfigAuthStore::from_config_file(path).map(|store| store.get(registry).cloned())
    }
}

#[derive(Debug, Clone, Default)]
struct DockerConfigAuthStore {
    entries: BTreeMap<String, RegistryAuth>,
}

impl DockerConfigAuthStore {
    fn from_default_config() -> Result<Self> {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return Ok(Self::default());
        };
        let path = home.join(".docker").join("config.json");
        if !path.exists() {
            return Ok(Self::default());
        }

        Self::from_config_file(&path)
    }

    fn from_config_file(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read Docker config: {}", path.display()))?;
        let config: DockerConfigFile = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse Docker config: {}", path.display()))?;
        let mut entries = BTreeMap::new();
        for (registry, entry) in config.auths {
            if let Some(auth) = entry.to_registry_auth().with_context(|| {
                format!(
                    "Failed to parse Docker registry auth for {registry} in {}",
                    path.display()
                )
            })? {
                entries.insert(normalize_registry_auth_key(&registry), auth);
            }
        }

        Ok(Self { entries })
    }

    fn get(&self, registry: &str) -> Option<&RegistryAuth> {
        self.entries
            .get(&normalize_registry_auth_key(registry))
            .or_else(|| self.entries.get(registry))
    }
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

pub(crate) fn pull_oci_feature_with_client(
    reference: &OciFeatureRef,
    cache_root: &Path,
    extract_root: &Path,
    registry: &dyn OciRegistryClient,
) -> Result<OciFeatureArtifact> {
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
        if archive_path.exists() {
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
    if archive_path.exists() {
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
    write_cache_archive(&archive_path, &blob)?;

    extract_cached_feature(
        reference,
        &manifest.digest,
        archive_path,
        extract_root,
        false,
    )
}

pub(crate) fn extract_feature_archive(archive_path: &Path, destination: &Path) -> Result<()> {
    if destination.exists() {
        fs::remove_dir_all(destination).with_context(|| {
            format!(
                "Failed to remove existing feature extraction directory: {}",
                destination.display()
            )
        })?;
    }
    fs::create_dir_all(destination).with_context(|| {
        format!(
            "Failed to create feature extraction directory: {}",
            destination.display()
        )
    })?;

    let mut file = fs::File::open(archive_path)
        .with_context(|| format!("Failed to open feature archive: {}", archive_path.display()))?;
    let mut magic = [0; 2];
    let read = file
        .read(&mut magic)
        .with_context(|| format!("Failed to read feature archive: {}", archive_path.display()))?;
    file.seek(SeekFrom::Start(0)).with_context(|| {
        format!(
            "Failed to rewind feature archive: {}",
            archive_path.display()
        )
    })?;

    if read == magic.len() && magic == [0x1f, 0x8b] {
        extract_tar_archive(archive_path, destination, GzDecoder::new(file))
    } else {
        extract_tar_archive(archive_path, destination, file)
    }
}

fn extract_tar_archive<R: Read>(archive_path: &Path, destination: &Path, reader: R) -> Result<()> {
    let mut archive = Archive::new(reader);

    for entry in archive.entries().with_context(|| {
        format!(
            "Failed to read feature archive entries: {}",
            archive_path.display()
        )
    })? {
        let mut entry = entry.with_context(|| {
            format!(
                "Failed to read feature archive entry: {}",
                archive_path.display()
            )
        })?;
        let path = entry.path().with_context(|| {
            format!(
                "Failed to read feature archive entry path: {}",
                archive_path.display()
            )
        })?;
        let path = path.into_owned();
        validate_archive_entry_path(&path)?;
        validate_archive_entry_type(entry.header().entry_type(), &path)?;
        entry.unpack_in(destination).with_context(|| {
            format!(
                "Failed to extract feature archive entry {} from {}",
                path.display(),
                archive_path.display()
            )
        })?;
    }

    Ok(())
}

pub(crate) fn read_feature_metadata(path: &Path) -> Result<FeatureMetadata> {
    read_feature_metadata_document(path).map(|document| document.metadata)
}

pub(crate) fn read_feature_metadata_document(path: &Path) -> Result<FeatureMetadataDocument> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read Feature metadata: {}", path.display()))?;
    let raw: JsonValue = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse Feature metadata: {}", path.display()))?;
    let metadata = serde_json::from_value(raw.clone())
        .with_context(|| format!("Failed to parse Feature metadata: {}", path.display()))?;
    let layer = parse_metadata_layer(raw.clone())
        .and_then(|metadata| metadata.to_config_layer_without_forward_ports())
        .with_context(|| {
            format!(
                "Failed to convert Feature metadata to devcontainer metadata layer: {}",
                path.display()
            )
        })?;

    Ok(FeatureMetadataDocument { metadata, layer })
}

pub(crate) fn prepare_feature_install_plan(
    features: &[ResolvedFeature],
    devcontainer_file: &Path,
    workspace_root: &Path,
    cache_root: &Path,
    override_feature_install_order: &[String],
    update_features: bool,
) -> Result<Option<PreparedFeatureInstallPlan>> {
    if features.is_empty() {
        return Ok(None);
    }

    let devcontainer_dir = devcontainer_file.parent().with_context(|| {
        format!(
            "Failed to resolve devcontainer directory for {}",
            devcontainer_file.display()
        )
    })?;
    let feature_cache_root = cache_root.join("features").join("archives");
    let extract_root = cache_root.join("features").join("extracted");
    let lock_path = workspace_root.join(".decune").join("features.lock.toml");
    let lock = read_feature_lock_file(&lock_path)?;
    let mut resolver = FeatureResolver {
        devcontainer_dir,
        lock: &lock,
        update_features,
        feature_cache_root,
        extract_root,
        sources: BTreeMap::new(),
    };

    let inputs = features
        .iter()
        .map(|feature| resolver.resolve_input(feature.clone()))
        .collect::<Result<Vec<_>>>()?;
    let entries =
        resolve_feature_install_order(inputs, override_feature_install_order, |request| {
            let feature = ResolvedFeature {
                id: dependency_feature_ref(request.dependency),
                canonical_id: request.canonical_id.clone(),
                options: request.options.clone(),
            };
            resolver.resolve_input(feature)
        })?;

    let mut prepared_entries = Vec::new();
    let mut metadata_layers = Vec::new();
    let mut lock_entries = Vec::new();
    for entry in entries {
        let source = resolver
            .sources
            .get(&entry.feature.canonical_id)
            .ok_or_else(|| {
                anyhow!(
                    "Feature source was not prepared for {}",
                    entry.feature.canonical_id
                )
            })?;
        prepared_entries.push(PreparedFeatureInstallEntry {
            feature: entry.feature,
            source_dir: source.source_dir.clone(),
            option_env: entry.option_env,
        });
        metadata_layers.push(source.layer.clone());
        if let Some(lock_entry) = &source.lock_entry {
            lock_entries.push(lock_entry.clone());
        }
    }

    lock_entries.sort_by(|left, right| left.feature_id.cmp(&right.feature_id));
    lock_entries.dedup_by(|left, right| left.feature_id == right.feature_id);
    let mut lock_file_entries = resolver
        .sources
        .values()
        .filter_map(|source| source.lock_file_entry.clone())
        .collect::<Vec<_>>();
    lock_file_entries.sort_by(|left, right| left.id.cmp(&right.id));
    lock_file_entries.dedup_by(|left, right| left.id == right.id);
    if !lock_file_entries.is_empty() {
        write_feature_lock_file(
            &lock_path,
            &FeatureLockFile {
                version: FEATURE_LOCK_VERSION,
                features: lock_file_entries,
            },
        )?;
    }
    Ok(Some(PreparedFeatureInstallPlan {
        entries: prepared_entries,
        metadata_layers,
        lock_entries,
    }))
}

pub(crate) fn resolve_feature_install_order<F>(
    inputs: Vec<FeatureInstallInput>,
    override_feature_install_order: &[String],
    mut resolve_dependency: F,
) -> Result<Vec<FeatureInstallPlanEntry>>
where
    F: FnMut(&FeatureDependencyRequest<'_>) -> Result<FeatureInstallInput>,
{
    let mut nodes = BTreeMap::new();
    for input in inputs {
        let key = input.feature.canonical_id.clone();
        if nodes.insert(key.clone(), input).is_some() {
            bail!("Duplicate Feature in install worklist: {key}");
        }
    }

    let mut scan_queue = nodes.keys().cloned().collect::<VecDeque<_>>();
    while let Some(canonical_id) = scan_queue.pop_front() {
        let input = nodes
            .get(&canonical_id)
            .expect("queued Feature must exist in install graph");
        let depends_on = input
            .metadata
            .depends_on
            .iter()
            .map(|(dependency, options)| (dependency.clone(), options.clone()))
            .collect::<Vec<_>>();

        for (dependency, options) in depends_on {
            let dependency_id = canonical_feature_dependency_id(&dependency);
            if nodes.contains_key(&dependency_id) {
                continue;
            }

            let options = feature_dependency_options(&canonical_id, &dependency, &options)?;
            let request = FeatureDependencyRequest {
                parent_canonical_id: &canonical_id,
                dependency: &dependency,
                canonical_id: dependency_id.clone(),
                options,
            };
            let mut dependency_input = resolve_dependency(&request).with_context(|| {
                format!(
                    "Failed to resolve Feature dependency {} for Feature {}",
                    request.dependency, request.parent_canonical_id
                )
            })?;
            if dependency_input.feature.canonical_id != dependency_id {
                bail!(
                    "Feature dependency resolver returned {} for {}, expected {}",
                    dependency_input.feature.canonical_id,
                    request.dependency,
                    dependency_id
                );
            }
            dependency_input.feature.options = request.options.clone();
            if nodes
                .insert(dependency_id.clone(), dependency_input)
                .is_some()
            {
                bail!("Duplicate Feature in install worklist: {dependency_id}");
            }
            scan_queue.push_back(dependency_id);
        }
    }

    let mut dependencies = BTreeMap::new();
    for (canonical_id, input) in &nodes {
        let mut required = BTreeSet::new();
        for dependency in input.metadata.depends_on.keys() {
            let dependency_id = canonical_feature_dependency_id(dependency);
            required.insert(dependency_id);
        }
        for dependency in &input.metadata.installs_after {
            let dependency_id = canonical_feature_dependency_id(dependency);
            if nodes.contains_key(&dependency_id) {
                required.insert(dependency_id);
            }
        }
        dependencies.insert(canonical_id.clone(), required);
    }

    let priorities = override_feature_install_priorities(override_feature_install_order);
    let mut worklist = nodes.keys().cloned().collect::<BTreeSet<_>>();
    let mut installed = BTreeSet::new();
    let mut ordered = Vec::new();

    while !worklist.is_empty() {
        let ready = worklist
            .iter()
            .filter(|canonical_id| {
                dependencies
                    .get(*canonical_id)
                    .is_none_or(|required| required.is_subset(&installed))
            })
            .cloned()
            .collect::<Vec<_>>();
        if ready.is_empty() {
            let blocked = worklist.into_iter().collect::<Vec<_>>().join(", ");
            bail!("Feature install order contains a dependency cycle involving: {blocked}");
        }

        let max_priority = ready
            .iter()
            .map(|canonical_id| feature_round_priority(canonical_id, &priorities))
            .max()
            .unwrap_or_default();
        let mut round = ready
            .into_iter()
            .filter(|canonical_id| {
                feature_round_priority(canonical_id, &priorities) == max_priority
            })
            .collect::<Vec<_>>();
        round.sort_by(|left, right| stable_feature_order(nodes.get(left), nodes.get(right)));

        for canonical_id in round {
            worklist.remove(&canonical_id);
            installed.insert(canonical_id.clone());
            let input = nodes
                .get(&canonical_id)
                .expect("ready Feature must exist in install graph");
            let option_env = feature_option_env(&input.feature, &input.metadata)?;
            ordered.push(FeatureInstallPlanEntry {
                feature: input.feature.clone(),
                metadata: input.metadata.clone(),
                option_env,
            });
        }
    }

    Ok(ordered)
}

pub(crate) fn feature_option_env(
    feature: &crate::config::resolved::ResolvedFeature,
    metadata: &FeatureMetadata,
) -> Result<BTreeMap<String, String>> {
    let mut env = BTreeMap::new();

    for (option, schema) in &metadata.options {
        if option == "enabled" {
            continue;
        }
        validate_feature_option_schema(&feature.id, option, schema)?;
        if feature.options.contains_key(option) {
            continue;
        }
        if let Some(default) = &schema.default {
            let value = feature_option_json_value(&feature.id, option, default, schema)?;
            env.insert(feature_option_env_name(option), value);
        }
    }

    for (option, value) in &feature.options {
        if option == "enabled" {
            continue;
        }
        let schema = metadata.options.get(option);
        if let Some(schema) = schema {
            validate_feature_option_schema(&feature.id, option, schema)?;
        }
        let value = feature_option_toml_value(&feature.id, option, value, schema)?;
        env.insert(feature_option_env_name(option), value);
    }

    Ok(env)
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
    let digest = digest
        .map(|digest| {
            validate_oci_digest(digest)
                .map(|()| digest.to_owned())
                .map_err(|error| invalid_feature_ref(value, &format!("invalid digest: {error}")))
        })
        .transpose()?;

    Ok(OciFeatureRef {
        original: value.to_owned(),
        registry: registry.to_owned(),
        repository: repository.to_owned(),
        feature_id: feature_id.to_owned(),
        tag: tag.map(str::to_owned),
        digest,
        canonical_id,
    })
}

impl OciFeatureRef {
    fn repository_path(&self) -> String {
        format!("{}/{}", self.repository, self.feature_id)
    }
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

struct FeatureResolver<'a> {
    devcontainer_dir: &'a Path,
    lock: &'a FeatureLockFile,
    update_features: bool,
    feature_cache_root: PathBuf,
    extract_root: PathBuf,
    sources: BTreeMap<String, FeatureSource>,
}

impl FeatureResolver<'_> {
    fn resolve_input(&mut self, feature: ResolvedFeature) -> Result<FeatureInstallInput> {
        if let Some(source) = self.sources.get(&feature.canonical_id) {
            return Ok(FeatureInstallInput {
                feature,
                metadata: source.metadata.clone(),
            });
        }

        let reference = parse_feature_ref_from_devcontainer_dir(&feature.id, self.devcontainer_dir)
            .with_context(|| format!("Failed to parse Feature ref: {}", feature.id))?;
        let source = self.resolve_source(&reference)?;
        let metadata = source.metadata.clone();
        self.sources.insert(feature.canonical_id.clone(), source);

        Ok(FeatureInstallInput { feature, metadata })
    }

    fn resolve_source(&self, reference: &FeatureRef) -> Result<FeatureSource> {
        match reference {
            FeatureRef::Local(local) => self.resolve_local_source(local),
            FeatureRef::Oci(oci) => self.resolve_oci_source(oci),
        }
    }

    fn resolve_local_source(&self, reference: &LocalFeatureRef) -> Result<FeatureSource> {
        let source_dir = reference.path.canonicalize().with_context(|| {
            format!(
                "Failed to resolve local Feature directory: {}",
                reference.path.display()
            )
        })?;
        ensure_feature_files(&source_dir)?;
        let document =
            read_feature_metadata_document(&source_dir.join("devcontainer-feature.json"))?;
        let digest = local_feature_content_digest(&source_dir)?;

        Ok(FeatureSource {
            source_dir,
            metadata: document.metadata,
            layer: document.layer,
            lock_entry: Some(FeatureLockHashEntry {
                feature_id: reference.canonical_id.clone(),
                digest,
            }),
            lock_file_entry: None,
        })
    }

    fn resolve_oci_source(&self, reference: &OciFeatureRef) -> Result<FeatureSource> {
        let locked = resolve_locked_feature_ref(
            &FeatureRef::Oci(reference.clone()),
            self.lock,
            self.update_features,
        );
        let locked_reference = parse_feature_ref(&locked).with_context(|| {
            format!(
                "Failed to parse locked Feature ref for {}",
                reference.original
            )
        })?;
        let FeatureRef::Oci(locked_reference) = locked_reference else {
            bail!(
                "OCI Feature resolved to a non-OCI reference: {}",
                reference.original
            );
        };
        let artifact = pull_oci_feature_with_client(
            &locked_reference,
            &self.feature_cache_root,
            &self.extract_root,
            &HttpOciRegistryClient::from_docker_config()?,
        )?;
        let document = read_feature_metadata_document(&artifact.metadata_path)?;

        Ok(FeatureSource {
            source_dir: artifact.extracted_dir,
            metadata: document.metadata,
            layer: document.layer,
            lock_entry: Some(FeatureLockHashEntry {
                feature_id: reference.canonical_id.clone(),
                digest: artifact.digest.clone(),
            }),
            lock_file_entry: Some(FeatureLockEntry {
                id: reference.canonical_id.clone(),
                reference: reference.original.clone(),
                digest: artifact.digest,
            }),
        })
    }
}

fn ensure_feature_files(source_dir: &Path) -> Result<()> {
    for name in ["install.sh", "devcontainer-feature.json"] {
        let path = source_dir.join(name);
        if !path.is_file() {
            bail!(
                "Feature directory must contain {name}: {}",
                source_dir.display()
            );
        }
    }

    Ok(())
}

fn local_feature_content_digest(source_dir: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    hash_local_feature_directory(source_dir, source_dir, &mut hasher)?;
    let digest = hasher.finalize();

    Ok(format!("sha256:{}", hex_lower(&digest)))
}

fn hash_local_feature_directory(root: &Path, directory: &Path, hasher: &mut Sha256) -> Result<()> {
    let mut entries = fs::read_dir(directory)
        .with_context(|| {
            format!(
                "Failed to read local Feature directory: {}",
                directory.display()
            )
        })?
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| {
            format!(
                "Failed to enumerate local Feature directory: {}",
                directory.display()
            )
        })?;
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        let path = entry.path();
        let relative_path = path.strip_prefix(root).with_context(|| {
            format!(
                "Failed to relativize local Feature path: {}",
                path.display()
            )
        })?;
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("Failed to inspect local Feature path: {}", path.display()))?;
        hash_local_feature_entry_header(
            hasher,
            relative_path,
            local_feature_entry_kind(&metadata),
            metadata.permissions().mode(),
        );

        if metadata.is_dir() {
            hash_local_feature_directory(root, &path, hasher)?;
        } else if metadata.file_type().is_symlink() {
            let target = fs::read_link(&path).with_context(|| {
                format!("Failed to read local Feature symlink: {}", path.display())
            })?;
            hasher.update(target.as_os_str().as_encoded_bytes());
            hasher.update([0]);
        } else if metadata.is_file() {
            let contents = fs::read(&path).with_context(|| {
                format!("Failed to read local Feature file: {}", path.display())
            })?;
            hasher.update(contents.len().to_be_bytes());
            hasher.update(contents);
        }
    }

    Ok(())
}

fn hash_local_feature_entry_header(
    hasher: &mut Sha256,
    relative_path: &Path,
    kind: &'static [u8],
    mode: u32,
) {
    hasher.update(kind);
    hasher.update([0]);
    hasher.update(relative_path.as_os_str().as_encoded_bytes());
    hasher.update([0]);
    hasher.update((mode & 0o7777).to_be_bytes());
}

fn local_feature_entry_kind(metadata: &fs::Metadata) -> &'static [u8] {
    if metadata.is_dir() {
        b"dir"
    } else if metadata.file_type().is_symlink() {
        b"symlink"
    } else if metadata.is_file() {
        b"file"
    } else {
        b"other"
    }
}

fn dependency_feature_ref(dependency: &str) -> String {
    if dependency.starts_with("./") || parse_feature_ref(dependency).is_ok() {
        dependency.to_owned()
    } else {
        format!("{dependency}:latest")
    }
}

fn feature_cache_archive_path(cache_root: &Path, digest: &str) -> PathBuf {
    cache_root.join(format!("{}.tgz", cache_safe_digest(digest)))
}

fn select_feature_archive_layer(manifest: &OciManifestResponse) -> Option<&OciLayerDescriptor> {
    manifest
        .layers
        .iter()
        .find(|layer| layer.media_type.contains("tar"))
        .or_else(|| manifest.layers.first())
}

fn write_cache_archive(path: &Path, content: &[u8]) -> Result<()> {
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
    })
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

fn find_required_feature_file(root: &Path, name: &str) -> Result<PathBuf> {
    find_file_by_name(root, name)?
        .ok_or_else(|| anyhow!("Feature archive is missing required file: {name}"))
}

fn find_file_by_name(root: &Path, name: &str) -> Result<Option<PathBuf>> {
    for entry in fs::read_dir(root)
        .with_context(|| format!("Failed to read directory: {}", root.display()))?
    {
        let entry =
            entry.with_context(|| format!("Failed to read directory entry: {}", root.display()))?;
        let path = entry.path();
        if path.file_name().and_then(|value| value.to_str()) == Some(name) {
            return Ok(Some(path));
        }
        if entry
            .file_type()
            .with_context(|| format!("Failed to read file type: {}", path.display()))?
            .is_dir()
            && let Some(path) = find_file_by_name(&path, name)?
        {
            return Ok(Some(path));
        }
    }

    Ok(None)
}

fn validate_archive_entry_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        bail!("Unsafe feature archive path: empty path");
    }
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("Unsafe feature archive path: {}", path.display());
            }
        }
    }

    Ok(())
}

fn validate_archive_entry_type(entry_type: EntryType, path: &Path) -> Result<()> {
    if entry_type.is_file() || entry_type.is_dir() {
        return Ok(());
    }

    bail!(
        "Unsupported feature archive entry type for {}",
        path.display()
    )
}

fn canonical_feature_dependency_id(value: &str) -> String {
    crate::config::layer::canonical_feature_id(value)
}

fn feature_dependency_options(
    parent_canonical_id: &str,
    dependency: &str,
    value: &serde_json::Value,
) -> Result<BTreeMap<String, toml::Value>> {
    match value {
        serde_json::Value::Object(values) => values
            .iter()
            .map(|(option, value)| {
                Ok((
                    option.clone(),
                    feature_dependency_option_value(
                        parent_canonical_id,
                        dependency,
                        option,
                        value,
                    )?,
                ))
            })
            .collect(),
        serde_json::Value::String(version) => Ok(BTreeMap::from([(
            "version".to_owned(),
            toml::Value::String(version.clone()),
        )])),
        _ => {
            bail!(
                "Unsupported Feature dependsOn value for {parent_canonical_id} dependency {dependency}"
            )
        }
    }
}

fn feature_dependency_option_value(
    parent_canonical_id: &str,
    dependency: &str,
    option: &str,
    value: &serde_json::Value,
) -> Result<toml::Value> {
    match value {
        serde_json::Value::String(value) => Ok(toml::Value::String(value.clone())),
        serde_json::Value::Bool(value) => Ok(toml::Value::Boolean(*value)),
        _ => {
            bail!(
                "Unsupported Feature dependsOn option value for {parent_canonical_id} dependency {dependency}.{option}"
            )
        }
    }
}

fn override_feature_install_priorities(override_order: &[String]) -> BTreeMap<String, usize> {
    let count = override_order.len();
    override_order
        .iter()
        .enumerate()
        .map(|(index, feature)| (canonical_feature_dependency_id(feature), count - index))
        .collect()
}

fn feature_round_priority(canonical_id: &str, priorities: &BTreeMap<String, usize>) -> usize {
    priorities.get(canonical_id).copied().unwrap_or_default()
}

fn stable_feature_order(
    left: Option<&FeatureInstallInput>,
    right: Option<&FeatureInstallInput>,
) -> std::cmp::Ordering {
    let Some(left) = left else {
        return std::cmp::Ordering::Greater;
    };
    let Some(right) = right else {
        return std::cmp::Ordering::Less;
    };

    left.feature
        .canonical_id
        .cmp(&right.feature.canonical_id)
        .then_with(|| right.feature.options.len().cmp(&left.feature.options.len()))
        .then_with(|| {
            feature_options_sort_key(&left.feature.options)
                .cmp(&feature_options_sort_key(&right.feature.options))
        })
        .then_with(|| left.feature.id.cmp(&right.feature.id))
}

fn feature_options_sort_key(options: &BTreeMap<String, toml::Value>) -> Vec<(String, String)> {
    options
        .iter()
        .filter(|(key, _)| key.as_str() != "enabled")
        .map(|(key, value)| (key.clone(), feature_option_sort_value(value)))
        .collect()
}

fn feature_option_sort_value(value: &toml::Value) -> String {
    match value {
        toml::Value::String(value) => value.clone(),
        toml::Value::Boolean(value) => value.to_string(),
        toml::Value::Integer(value) => value.to_string(),
        toml::Value::Float(value) => value.to_string(),
        toml::Value::Datetime(value) => value.to_string(),
        toml::Value::Array(values) => values
            .iter()
            .map(feature_option_sort_value)
            .collect::<Vec<_>>()
            .join(","),
        toml::Value::Table(values) => values
            .iter()
            .map(|(key, value)| format!("{key}={}", feature_option_sort_value(value)))
            .collect::<Vec<_>>()
            .join(","),
    }
}

fn validate_feature_option_schema(
    feature_id: &str,
    option: &str,
    schema: &FeatureOptionSchema,
) -> Result<()> {
    match schema.option_type.as_deref() {
        Some("string" | "boolean") | None => Ok(()),
        Some(option_type) => {
            bail!("Unsupported Feature option type for {feature_id}.{option}: {option_type}")
        }
    }
}

fn feature_option_toml_value(
    feature_id: &str,
    option: &str,
    value: &toml::Value,
    schema: Option<&FeatureOptionSchema>,
) -> Result<String> {
    let resolved = match value {
        toml::Value::String(value) => {
            if matches!(
                schema.and_then(|schema| schema.option_type.as_deref()),
                Some("boolean")
            ) {
                bail!("Feature option {feature_id}.{option} must be a boolean");
            }
            value.clone()
        }
        toml::Value::Boolean(value) => {
            if matches!(
                schema.and_then(|schema| schema.option_type.as_deref()),
                Some("string")
            ) {
                bail!("Feature option {feature_id}.{option} must be a string");
            }
            value.to_string()
        }
        _ => bail!("Unsupported Feature option value for {feature_id}.{option}"),
    };

    validate_feature_option_enum(feature_id, option, &resolved, schema)?;
    Ok(resolved)
}

fn feature_option_json_value(
    feature_id: &str,
    option: &str,
    value: &serde_json::Value,
    schema: &FeatureOptionSchema,
) -> Result<String> {
    let resolved = match value {
        serde_json::Value::String(value) => {
            if matches!(schema.option_type.as_deref(), Some("boolean")) {
                bail!("Feature option default {feature_id}.{option} must be a boolean");
            }
            value.clone()
        }
        serde_json::Value::Bool(value) => {
            if matches!(schema.option_type.as_deref(), Some("string")) {
                bail!("Feature option default {feature_id}.{option} must be a string");
            }
            value.to_string()
        }
        _ => bail!("Unsupported Feature option default for {feature_id}.{option}"),
    };

    validate_feature_option_enum(feature_id, option, &resolved, Some(schema))?;
    Ok(resolved)
}

fn validate_feature_option_enum(
    feature_id: &str,
    option: &str,
    value: &str,
    schema: Option<&FeatureOptionSchema>,
) -> Result<()> {
    if let Some(schema) = schema
        && !schema.enum_values.is_empty()
        && !schema.enum_values.iter().any(|allowed| allowed == value)
    {
        bail!("Feature option {feature_id}.{option} must be one of the declared enum values");
    }

    Ok(())
}

fn feature_option_env_name(option: &str) -> String {
    let sanitized = option
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                ch.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized
        .chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_alphabetic())
    {
        sanitized
    } else {
        format!("_{sanitized}")
    }
}

fn registry_url(reference: &OciFeatureRef, suffix: &str) -> String {
    format!(
        "https://{}/v2/{}/{}",
        reference.registry,
        reference.repository_path(),
        suffix
    )
}

fn registry_accept_header() -> HeaderValue {
    HeaderValue::from_static(
        "application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json, application/octet-stream, */*",
    )
}

fn parse_registry_manifest_response_body(
    reference: &str,
    requested_digest: Option<&str>,
    header_digest: Option<&str>,
    body: &[u8],
) -> Result<OciManifestResponse> {
    let requested_digest = requested_digest
        .map(|digest| {
            validate_oci_digest(digest)
                .map(|()| digest)
                .with_context(|| format!("Invalid requested OCI manifest digest: {digest}"))
        })
        .transpose()?;
    let header_digest = header_digest
        .map(|digest| {
            validate_oci_digest(digest)
                .map(|()| digest)
                .with_context(|| format!("Invalid Docker-Content-Digest header: {digest}"))
        })
        .transpose()?;
    let actual_digest = format!("sha256:{}", hex_lower(&Sha256::digest(body)));

    if let Some(header_digest) = header_digest
        && header_digest != actual_digest
    {
        bail!(
            "OCI manifest digest mismatch for {reference}: Docker-Content-Digest is {header_digest}, body is {actual_digest}"
        );
    }
    if let Some(requested_digest) = requested_digest
        && requested_digest != actual_digest
    {
        bail!(
            "OCI manifest digest mismatch for {reference}: expected {requested_digest}, got {actual_digest}"
        );
    }

    let digest = header_digest
        .or(requested_digest)
        .unwrap_or(&actual_digest)
        .to_owned();
    let manifest: RegistryManifest = serde_json::from_slice(body).with_context(|| {
        format!("Failed to parse OCI manifest for feature {reference} ({digest})")
    })?;
    let layers = manifest
        .layers
        .into_iter()
        .map(|layer| {
            validate_oci_digest(&layer.digest).with_context(|| {
                format!(
                    "Invalid OCI manifest layer digest for feature {reference}: {}",
                    layer.digest
                )
            })?;
            Ok(OciLayerDescriptor {
                digest: layer.digest,
                media_type: layer.media_type,
                size: layer.size.unwrap_or_default(),
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(OciManifestResponse { digest, layers })
}

fn validate_oci_digest(digest: &str) -> Result<()> {
    let Some(expected) = digest.strip_prefix("sha256:") else {
        bail!("Unsupported OCI digest algorithm: {digest}");
    };
    if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("Invalid OCI sha256 digest: {digest}");
    }
    if !expected
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        bail!("OCI sha256 digest must use lowercase hex: {digest}");
    }

    Ok(())
}

fn normalize_registry_auth_key(registry: &str) -> String {
    registry
        .strip_prefix("https://")
        .or_else(|| registry.strip_prefix("http://"))
        .unwrap_or(registry)
        .trim_end_matches('/')
        .to_owned()
}

fn bearer_challenge(headers: &HeaderMap) -> Option<BearerChallenge> {
    let value = headers.get(WWW_AUTHENTICATE)?.to_str().ok()?;
    parse_bearer_challenge(value)
}

fn parse_bearer_challenge(value: &str) -> Option<BearerChallenge> {
    let parameters = value.strip_prefix("Bearer ")?;
    let mut challenge = BearerChallenge::default();
    for entry in split_auth_parameters(parameters) {
        let Some((key, value)) = entry.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"').to_owned();
        match key.trim() {
            "realm" => challenge.realm = value,
            "service" => challenge.service = Some(value),
            "scope" => challenge.scope = Some(value),
            _ => {}
        }
    }
    if challenge.realm.is_empty() {
        None
    } else {
        Some(challenge)
    }
}

fn split_auth_parameters(value: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut in_quotes = false;
    let mut start = 0;
    for (index, byte) in value.bytes().enumerate() {
        match byte {
            b'"' => in_quotes = !in_quotes,
            b',' if !in_quotes => {
                parts.push(value[start..index].trim());
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(value[start..].trim());
    parts
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

fn hex_lower(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
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

#[derive(Debug, Deserialize)]
struct RegistryManifest {
    #[serde(default)]
    layers: Vec<RegistryDescriptor>,
}

#[derive(Debug, Deserialize)]
struct RegistryDescriptor {
    #[serde(rename = "mediaType", default)]
    media_type: String,
    digest: String,
    size: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct DockerConfigFile {
    #[serde(default)]
    auths: BTreeMap<String, DockerAuthEntry>,
}

#[derive(Debug, Deserialize)]
struct DockerAuthEntry {
    auth: Option<String>,
    username: Option<String>,
    password: Option<String>,
    identitytoken: Option<String>,
}

impl DockerAuthEntry {
    fn to_registry_auth(&self) -> Result<Option<RegistryAuth>> {
        if let Some(token) = &self.identitytoken {
            return Ok(Some(RegistryAuth::Bearer(token.clone())));
        }
        if let Some(auth) = &self.auth {
            let decoded = BASE64
                .decode(auth)
                .with_context(|| "Docker registry auth is not valid base64")?;
            let decoded = String::from_utf8(decoded)
                .with_context(|| "Docker registry auth is not valid UTF-8")?;
            let (username, password) = decoded
                .split_once(':')
                .ok_or_else(|| anyhow!("Docker registry auth must be username:password"))?;
            return Ok(Some(RegistryAuth::Basic {
                username: username.to_owned(),
                password: password.to_owned(),
            }));
        }
        if let (Some(username), Some(password)) = (&self.username, &self.password) {
            return Ok(Some(RegistryAuth::Basic {
                username: username.clone(),
                password: password.clone(),
            }));
        }

        Ok(None)
    }
}

#[derive(Debug, Default)]
struct BearerChallenge {
    realm: String,
    service: Option<String>,
    scope: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BearerTokenResponse {
    token: Option<String>,
    access_token: Option<String>,
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, collections::BTreeMap, fs, io::Write, path::Path};

    use flate2::{Compression, write::GzEncoder};
    use tar::{Builder, Header};

    use super::*;

    const MANIFEST_DIGEST_HEX: &str =
        "1111111111111111111111111111111111111111111111111111111111111111";

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
        let digest = sha256_digest(MANIFEST_DIGEST_HEX);
        let reference = parse_feature_ref(&format!(
            "localhost:5000/devcontainers/features/tool@{digest}"
        ))
        .unwrap();

        assert_eq!(
            reference,
            FeatureRef::Oci(OciFeatureRef {
                original: format!("localhost:5000/devcontainers/features/tool@{digest}"),
                registry: "localhost:5000".to_owned(),
                repository: "devcontainers/features".to_owned(),
                feature_id: "tool".to_owned(),
                tag: None,
                digest: Some(digest),
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
    fn oci_feature_ref_rejects_invalid_digest_values() {
        for reference in [
            "ghcr.io/example/features/tool@../../x",
            "ghcr.io/example/features/tool@sha256:abcd",
            "ghcr.io/example/features/tool@sha512:1111111111111111111111111111111111111111111111111111111111111111",
        ] {
            let error = parse_feature_ref(reference).unwrap_err();

            assert!(error.to_string().contains(reference), "{error:#}");
            assert!(error.to_string().contains("digest"), "{error:#}");
        }
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
    fn local_feature_content_change_changes_prepared_lock_entry_digest() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_root = temp.path().join("workspace");
        let devcontainer_dir = workspace_root.join(".devcontainer");
        let feature_dir = devcontainer_dir.join("features/local");
        let cache_root = temp.path().join("cache");
        fs::create_dir_all(&feature_dir).unwrap();
        fs::write(devcontainer_dir.join("devcontainer.json"), "{}").unwrap();
        fs::write(
            feature_dir.join("devcontainer-feature.json"),
            r#"{"id":"local"}"#,
        )
        .unwrap();
        fs::write(
            feature_dir.join("install.sh"),
            "#!/bin/sh\ncat helper.txt\n",
        )
        .unwrap();
        fs::write(feature_dir.join("helper.txt"), "first\n").unwrap();
        let features = vec![ResolvedFeature {
            id: "./features/local".to_owned(),
            canonical_id: "local:features/local".to_owned(),
            options: BTreeMap::new(),
        }];

        let first = prepare_feature_install_plan(
            &features,
            &devcontainer_dir.join("devcontainer.json"),
            &workspace_root,
            &cache_root,
            &[],
            false,
        )
        .unwrap()
        .unwrap();
        fs::write(feature_dir.join("helper.txt"), "second\n").unwrap();
        let second = prepare_feature_install_plan(
            &features,
            &devcontainer_dir.join("devcontainer.json"),
            &workspace_root,
            &cache_root,
            &[],
            false,
        )
        .unwrap()
        .unwrap();

        assert_eq!(first.lock_entries.len(), 1);
        assert_eq!(second.lock_entries.len(), 1);
        assert_eq!(first.lock_entries[0].feature_id, "local:features/local");
        assert_eq!(second.lock_entries[0].feature_id, "local:features/local");
        assert_ne!(first.lock_entries[0].digest, second.lock_entries[0].digest);
    }

    #[test]
    fn local_feature_depends_on_is_resolved_relative_to_devcontainer_dir() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_root = temp.path().join("workspace");
        let devcontainer_dir = workspace_root.join(".devcontainer");
        let tool_dir = devcontainer_dir.join("features/tool");
        let base_dir = devcontainer_dir.join("features/base");
        let cache_root = temp.path().join("cache");
        fs::create_dir_all(&tool_dir).unwrap();
        fs::create_dir_all(&base_dir).unwrap();
        fs::write(devcontainer_dir.join("devcontainer.json"), "{}").unwrap();
        fs::write(
            tool_dir.join("devcontainer-feature.json"),
            r#"{
                "id": "tool",
                "dependsOn": {
                    "./features/base": {
                        "version": "1.2"
                    }
                }
            }"#,
        )
        .unwrap();
        fs::write(tool_dir.join("install.sh"), "#!/bin/sh\n").unwrap();
        fs::write(
            base_dir.join("devcontainer-feature.json"),
            r#"{"id":"base"}"#,
        )
        .unwrap();
        fs::write(base_dir.join("install.sh"), "#!/bin/sh\n").unwrap();
        let features = vec![ResolvedFeature {
            id: "./features/tool".to_owned(),
            canonical_id: "./features/tool".to_owned(),
            options: BTreeMap::new(),
        }];

        let plan = prepare_feature_install_plan(
            &features,
            &devcontainer_dir.join("devcontainer.json"),
            &workspace_root,
            &cache_root,
            &[],
            false,
        )
        .unwrap()
        .unwrap();

        assert_eq!(
            plan.entries
                .iter()
                .map(|entry| entry.feature.canonical_id.as_str())
                .collect::<Vec<_>>(),
            vec!["./features/base", "./features/tool"]
        );
        assert_eq!(
            plan.entries[0].option_env.get("VERSION"),
            Some(&"1.2".to_owned())
        );
        assert!(base_dir.exists());
        assert!(!devcontainer_dir.join("features/base:latest").exists());
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

    #[test]
    fn feature_archive_rejects_path_traversal_entries() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("feature.tgz");
        write_malicious_feature_archive(&archive, "../escape", b"owned");

        let error = extract_feature_archive(&archive, &temp.path().join("out")).unwrap_err();

        assert!(error.to_string().contains("Unsafe feature archive path"));
        assert!(!temp.path().join("escape").exists());
    }

    #[test]
    fn digest_cache_hit_skips_registry_access() {
        let temp = tempfile::tempdir().unwrap();
        let cache_root = temp.path().join("cache");
        fs::create_dir_all(&cache_root).unwrap();
        let digest = sha256_digest(MANIFEST_DIGEST_HEX);
        let archive = feature_cache_archive_path(&cache_root, &digest);
        write_feature_archive(
            &archive,
            &[
                ("install.sh", b"#!/bin/sh\n".as_slice()),
                ("devcontainer-feature.json", br#"{"id":"tool"}"#.as_slice()),
            ],
        );
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
    fn feature_cache_archive_path_uses_safe_digest_filename() {
        let cache_root = Path::new("/tmp/cache");
        let digest = sha256_digest(MANIFEST_DIGEST_HEX);

        assert_eq!(
            feature_cache_archive_path(cache_root, &digest),
            cache_root.join(format!("sha256_{MANIFEST_DIGEST_HEX}.tgz"))
        );
    }

    #[test]
    fn feature_install_order_honors_depends_on_and_installs_after() {
        let plan = resolve_feature_install_order(
            vec![
                feature_install_input(
                    "ghcr.io/example/features/tool:1",
                    FeatureMetadata {
                        depends_on: BTreeMap::from([(
                            "ghcr.io/example/features/base:1".to_owned(),
                            serde_json::json!({}),
                        )]),
                        ..FeatureMetadata::default()
                    },
                ),
                feature_install_input(
                    "ghcr.io/example/features/lint:1",
                    FeatureMetadata {
                        installs_after: vec![
                            "ghcr.io/example/features/tool".to_owned(),
                            "ghcr.io/example/features/missing".to_owned(),
                        ],
                        ..FeatureMetadata::default()
                    },
                ),
                feature_install_input(
                    "ghcr.io/example/features/base:1",
                    FeatureMetadata::default(),
                ),
            ],
            &[],
            missing_feature_dependency,
        )
        .unwrap();

        assert_eq!(
            plan.iter()
                .map(|entry| entry.feature.canonical_id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "ghcr.io/example/features/base",
                "ghcr.io/example/features/tool",
                "ghcr.io/example/features/lint",
            ]
        );
    }

    #[test]
    fn feature_install_order_resolves_missing_depends_on_recursively() {
        let plan = resolve_feature_install_order(
            vec![feature_install_input(
                "ghcr.io/example/features/tool:1",
                FeatureMetadata {
                    depends_on: BTreeMap::from([(
                        "ghcr.io/example/features/base:1".to_owned(),
                        serde_json::json!({
                            "version": "1.2"
                        }),
                    )]),
                    ..FeatureMetadata::default()
                },
            )],
            &[],
            |request| match request.canonical_id.as_str() {
                "ghcr.io/example/features/base" => Ok(feature_install_input(
                    &request.dependency,
                    FeatureMetadata {
                        depends_on: BTreeMap::from([(
                            "ghcr.io/example/features/common:1".to_owned(),
                            serde_json::json!("3"),
                        )]),
                        ..FeatureMetadata::default()
                    },
                )),
                "ghcr.io/example/features/common" => Ok(feature_install_input(
                    &request.dependency,
                    FeatureMetadata::default(),
                )),
                _ => bail!("unexpected dependency {}", request.dependency),
            },
        )
        .unwrap();

        assert_eq!(
            plan.iter()
                .map(|entry| entry.feature.canonical_id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "ghcr.io/example/features/common",
                "ghcr.io/example/features/base",
                "ghcr.io/example/features/tool",
            ]
        );
        assert_eq!(
            plan[1].feature.options.get("version"),
            Some(&toml::Value::String("1.2".to_owned()))
        );
        assert_eq!(
            plan[0].feature.options.get("version"),
            Some(&toml::Value::String("3".to_owned()))
        );
    }

    #[test]
    fn override_feature_install_order_prioritizes_ready_features() {
        let plan = resolve_feature_install_order(
            vec![
                feature_install_input(
                    "ghcr.io/example/features/alpha:1",
                    FeatureMetadata::default(),
                ),
                feature_install_input(
                    "ghcr.io/example/features/beta:1",
                    FeatureMetadata::default(),
                ),
                feature_install_input(
                    "ghcr.io/example/features/gamma:1",
                    FeatureMetadata::default(),
                ),
            ],
            &["ghcr.io/example/features/gamma".to_owned()],
            missing_feature_dependency,
        )
        .unwrap();

        assert_eq!(
            plan.iter()
                .map(|entry| entry.feature.canonical_id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "ghcr.io/example/features/gamma",
                "ghcr.io/example/features/alpha",
                "ghcr.io/example/features/beta",
            ]
        );
    }

    #[test]
    fn feature_install_order_cycle_error_includes_feature_names() {
        let error = resolve_feature_install_order(
            vec![
                feature_install_input(
                    "ghcr.io/example/features/alpha:1",
                    FeatureMetadata {
                        depends_on: BTreeMap::from([(
                            "ghcr.io/example/features/beta:1".to_owned(),
                            serde_json::json!({}),
                        )]),
                        ..FeatureMetadata::default()
                    },
                ),
                feature_install_input(
                    "ghcr.io/example/features/beta:1",
                    FeatureMetadata {
                        installs_after: vec!["ghcr.io/example/features/alpha".to_owned()],
                        ..FeatureMetadata::default()
                    },
                ),
            ],
            &[],
            missing_feature_dependency,
        )
        .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("alpha"), "{error:#}");
        assert!(message.contains("beta"), "{error:#}");
    }

    #[test]
    fn feature_option_env_uses_defaults_and_skips_reserved_enabled() {
        let feature = feature_install_input(
            "ghcr.io/example/features/tool:1",
            FeatureMetadata {
                options: BTreeMap::from([
                    (
                        "version".to_owned(),
                        FeatureOptionSchema {
                            option_type: Some("string".to_owned()),
                            default: Some(serde_json::json!("latest")),
                            enum_values: vec!["latest".to_owned(), "1.2".to_owned()],
                        },
                    ),
                    (
                        "installTools".to_owned(),
                        FeatureOptionSchema {
                            option_type: Some("boolean".to_owned()),
                            default: Some(serde_json::json!(true)),
                            enum_values: Vec::new(),
                        },
                    ),
                ]),
                ..FeatureMetadata::default()
            },
        )
        .with_options([
            ("version", toml::Value::String("1.2".to_owned())),
            ("installTools", toml::Value::Boolean(false)),
            ("enabled", toml::Value::Boolean(true)),
        ]);

        let env = feature_option_env(&feature.feature, &feature.metadata).unwrap();

        assert_eq!(env.get("VERSION").map(String::as_str), Some("1.2"));
        assert_eq!(env.get("INSTALLTOOLS").map(String::as_str), Some("false"));
        assert!(!env.contains_key("ENABLED"));
    }

    #[test]
    fn feature_option_env_skips_reserved_enabled_metadata_default() {
        let feature = feature_install_input(
            "ghcr.io/example/features/tool:1",
            FeatureMetadata {
                options: BTreeMap::from([
                    (
                        "enabled".to_owned(),
                        FeatureOptionSchema {
                            option_type: Some("boolean".to_owned()),
                            default: Some(serde_json::json!(true)),
                            enum_values: Vec::new(),
                        },
                    ),
                    (
                        "version".to_owned(),
                        FeatureOptionSchema {
                            option_type: Some("string".to_owned()),
                            default: Some(serde_json::json!("latest")),
                            enum_values: Vec::new(),
                        },
                    ),
                ]),
                ..FeatureMetadata::default()
            },
        );

        let env = feature_option_env(&feature.feature, &feature.metadata).unwrap();

        assert_eq!(env.get("VERSION").map(String::as_str), Some("latest"));
        assert!(!env.contains_key("ENABLED"));
    }

    #[test]
    fn feature_option_env_rejects_unsupported_schema_type() {
        let feature = feature_install_input(
            "ghcr.io/example/features/tool:1",
            FeatureMetadata {
                options: BTreeMap::from([(
                    "items".to_owned(),
                    FeatureOptionSchema {
                        option_type: Some("array".to_owned()),
                        default: None,
                        enum_values: Vec::new(),
                    },
                )]),
                ..FeatureMetadata::default()
            },
        );

        let error = feature_option_env(&feature.feature, &feature.metadata).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Unsupported Feature option type")
        );
        assert!(error.to_string().contains("items"));
    }

    #[test]
    fn manifest_response_rejects_body_digest_mismatch_for_digest_reference() {
        let requested_digest = sha256_digest(MANIFEST_DIGEST_HEX);
        let body = br#"{"layers":[]}"#;

        let error = parse_registry_manifest_response_body(
            "ghcr.io/example/features/tool",
            Some(&requested_digest),
            Some(&requested_digest),
            body,
        )
        .unwrap_err();

        assert!(error.to_string().contains("digest mismatch"), "{error:#}");
        assert!(error.to_string().contains(&requested_digest), "{error:#}");
    }

    #[test]
    fn manifest_response_rejects_header_digest_mismatch() {
        let body = br#"{"layers":[]}"#;
        let actual_digest = sha256_digest(&hex_lower(&Sha256::digest(body)));
        let header_digest = sha256_digest(MANIFEST_DIGEST_HEX);

        let error = parse_registry_manifest_response_body(
            "ghcr.io/example/features/tool:1",
            None,
            Some(&header_digest),
            body,
        )
        .unwrap_err();

        assert!(error.to_string().contains("digest mismatch"), "{error:#}");
        assert!(error.to_string().contains(&actual_digest), "{error:#}");
    }

    #[test]
    fn manifest_response_uses_body_digest_when_header_is_missing_for_tag_reference() {
        let body = br#"{"layers":[{"mediaType":"application/vnd.devcontainers.layer.v1+tar","digest":"sha256:1111111111111111111111111111111111111111111111111111111111111111","size":12}]}"#;
        let actual_digest = sha256_digest(&hex_lower(&Sha256::digest(body)));

        let manifest = parse_registry_manifest_response_body(
            "ghcr.io/example/features/tool:1",
            None,
            None,
            body,
        )
        .unwrap();

        assert_eq!(manifest.digest, actual_digest);
        assert_eq!(manifest.layers.len(), 1);
    }

    #[test]
    fn registry_pull_caches_and_extracts_feature_archive() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("source.tgz");
        write_feature_archive(
            &archive,
            &[
                ("install.sh", b"#!/bin/sh\n".as_slice()),
                ("devcontainer-feature.json", br#"{"id":"tool"}"#.as_slice()),
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
                    digest: layer_digest.clone(),
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
    fn docker_config_auth_decodes_registry_credentials() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config.json");
        fs::write(
            &config,
            r#"{"auths":{"ghcr.io":{"auth":"dXNlcjp0b2tlbg=="}}}"#,
        )
        .unwrap();

        let auth = DockerConfigAuth::from_config_file(&config, "ghcr.io").unwrap();

        assert_eq!(
            auth,
            Some(RegistryAuth::Basic {
                username: "user".to_owned(),
                password: "token".to_owned(),
            })
        );
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

    fn write_malicious_feature_archive(path: &Path, entry_path: &str, content: &[u8]) {
        let file = fs::File::create(path).unwrap();
        let mut encoder = GzEncoder::new(file, Compression::default());
        let mut header = [0u8; 512];
        header[..entry_path.len()].copy_from_slice(entry_path.as_bytes());
        write_octal(&mut header[100..108], 0o755);
        write_octal(&mut header[108..116], 0);
        write_octal(&mut header[116..124], 0);
        write_octal(&mut header[124..136], content.len() as u64);
        write_octal(&mut header[136..148], 0);
        for byte in &mut header[148..156] {
            *byte = b' ';
        }
        header[156] = b'0';
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        let checksum = header.iter().map(|byte| u32::from(*byte)).sum::<u32>() as u64;
        write_checksum(&mut header[148..156], checksum);

        encoder.write_all(&header).unwrap();
        encoder.write_all(content).unwrap();
        let padding = (512 - (content.len() % 512)) % 512;
        encoder.write_all(&vec![0; padding]).unwrap();
        encoder.write_all(&[0; 1024]).unwrap();
        let mut file = encoder.finish().unwrap();
        file.flush().unwrap();
    }

    fn write_octal(field: &mut [u8], value: u64) {
        let value = format!("{value:0width$o}\0", width = field.len() - 1);
        field.copy_from_slice(value.as_bytes());
    }

    fn write_checksum(field: &mut [u8], value: u64) {
        let value = format!("{value:06o}\0 ",);
        field.copy_from_slice(value.as_bytes());
    }

    fn feature_install_input(id: &str, metadata: FeatureMetadata) -> FeatureInstallInput {
        let reference = parse_feature_ref(id).unwrap();
        FeatureInstallInput {
            feature: crate::config::resolved::ResolvedFeature {
                id: id.to_owned(),
                canonical_id: reference.canonical_id().to_owned(),
                options: BTreeMap::new(),
            },
            metadata,
        }
    }

    fn missing_feature_dependency(
        request: &FeatureDependencyRequest<'_>,
    ) -> Result<FeatureInstallInput> {
        bail!(
            "unexpected missing Feature dependency {} for {}",
            request.dependency,
            request.parent_canonical_id
        )
    }

    trait FeatureInstallInputTestExt {
        fn with_options<const N: usize>(
            self,
            options: [(&'static str, toml::Value); N],
        ) -> FeatureInstallInput;
    }

    impl FeatureInstallInputTestExt for FeatureInstallInput {
        fn with_options<const N: usize>(
            mut self,
            options: [(&'static str, toml::Value); N],
        ) -> FeatureInstallInput {
            self.feature.options = options
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value))
                .collect();
            self
        }
    }
}
