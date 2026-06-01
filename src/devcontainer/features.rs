// M10-T02 exposes OCI pull/cache primitives that M10-T04 wires into image build.
#![allow(dead_code)]

use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{ErrorKind, Read, Seek, SeekFrom},
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
use sha2::{Digest, Sha256};
use tar::{Archive, EntryType};

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
            .map(str::to_owned)
            .or_else(|| reference.digest.clone())
            .ok_or_else(|| {
                anyhow!(
                    "OCI manifest response for feature {} did not include Docker-Content-Digest",
                    reference.original
                )
            })?;
        let manifest: RegistryManifest = response.json().with_context(|| {
            format!(
                "Failed to parse OCI manifest for feature {} ({digest})",
                reference.original
            )
        })?;
        let layers = manifest
            .layers
            .into_iter()
            .map(|layer| OciLayerDescriptor {
                digest: layer.digest,
                media_type: layer.media_type,
                size: layer.size.unwrap_or_default(),
            })
            .collect();

        Ok(OciManifestResponse { digest, layers })
    }

    fn fetch_blob(&self, reference: &OciFeatureRef, digest: &str) -> Result<Vec<u8>> {
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

fn feature_cache_archive_path(cache_root: &Path, digest: &str) -> PathBuf {
    cache_root.join(format!("{digest}.tgz"))
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
    let Some(expected) = digest.strip_prefix("sha256:") else {
        bail!("Unsupported OCI digest algorithm: {digest}");
    };
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
    digest.replace([':', '/'], "_")
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
    use std::{cell::Cell, fs, io::Write, path::Path};

    use flate2::{Compression, write::GzEncoder};
    use tar::{Builder, Header};

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
        let archive = cache_root.join("sha256:manifest.tgz");
        write_feature_archive(
            &archive,
            &[
                ("install.sh", b"#!/bin/sh\n".as_slice()),
                ("devcontainer-feature.json", br#"{"id":"tool"}"#.as_slice()),
            ],
        );
        let reference =
            parse_oci_feature_ref("ghcr.io/example/features/tool@sha256:manifest").unwrap();
        let registry = PanicRegistryClient;

        let artifact = pull_oci_feature_with_client(
            &reference,
            &cache_root,
            &temp.path().join("extract"),
            &registry,
        )
        .unwrap();

        assert!(artifact.cache_hit);
        assert_eq!(artifact.digest, "sha256:manifest");
        assert!(artifact.install_script_path.exists());
        assert!(artifact.metadata_path.exists());
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
                digest: "sha256:manifest".to_owned(),
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
        assert_eq!(artifact.digest, "sha256:manifest");
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
}
