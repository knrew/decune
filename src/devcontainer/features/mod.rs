#![allow(dead_code)]

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Child, Command as HostCommand, Stdio},
    thread,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use reqwest::{
    StatusCode,
    blocking::{Client as HttpClient, RequestBuilder, Response},
    header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, WWW_AUTHENTICATE},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::{FeatureLockHashEntry, layer::ConfigLayer, resolved::ResolvedFeature};

mod archive;
mod local;
mod lock;
mod metadata;
mod reference;

pub(crate) use archive::extract_feature_archive;
use archive::find_required_feature_file;
use local::{
    ensure_feature_files, local_feature_content_digest, validate_local_feature_directory_name,
};
pub(crate) use lock::{
    FEATURE_LOCK_VERSION, FeatureLockEntry, FeatureLockFile, read_feature_lock_file,
    remove_feature_lock_file, resolve_locked_feature_ref, write_feature_lock_file,
};
use metadata::validate_feature_option_schema;
pub(crate) use metadata::{FeatureMetadata, FeatureOptionSchema, read_feature_metadata_document};
#[allow(unused_imports)]
pub(crate) use metadata::{FeatureMetadataDocument, read_feature_metadata};
#[cfg(test)]
use reference::parse_oci_feature_ref;
pub(crate) use reference::{
    FeatureRef, LocalFeatureRef, OciFeatureRef, parse_feature_ref,
    parse_feature_ref_from_devcontainer_dir,
};
use reference::{split_digest, split_tag, validate_oci_digest};

const DEVCONTAINER_FEATURE_LAYER_MEDIA_TYPE: &str = "application/vnd.devcontainers.layer.v1+tar";
const DOCKER_HUB_CANONICAL_HOST: &str = "docker.io";
const DOCKER_HUB_REGISTRY_HOST: &str = "registry-1.docker.io";
const DOCKER_HUB_INDEX_HOST: &str = "index.docker.io";
const DOCKER_HUB_INDEX_AUTH_KEY: &str = "index.docker.io/v1";
const DOCKER_HUB_CREDENTIAL_HELPER_SERVER: &str = "https://index.docker.io/v1/";
const DOCKER_CREDENTIAL_HELPER_SPAWN_RETRIES: usize = 5;
const DOCKER_CREDENTIAL_HELPER_SPAWN_RETRY_DELAY: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FeatureCacheMetadata {
    manifest_digest: String,
    layer_digest: String,
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

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FeatureInstallInput {
    pub(crate) feature: crate::config::resolved::ResolvedFeature,
    pub(crate) reference: FeatureRef,
    pub(crate) metadata: FeatureMetadata,
    pub(crate) source_key: String,
    pub(crate) instance_key: String,
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
    pub(crate) container_env: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
struct FeatureSource {
    source_dir: PathBuf,
    metadata: FeatureMetadata,
    layer: ConfigLayer,
    container_env: BTreeMap<String, String>,
    digest: String,
    lock_file_entry: Option<FeatureLockEntry>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FeatureInstallPlanEntry {
    pub(crate) feature: crate::config::resolved::ResolvedFeature,
    pub(crate) metadata: FeatureMetadata,
    pub(crate) source_key: String,
    pub(crate) instance_key: String,
    pub(crate) option_env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FeatureDependencyRequest<'a> {
    pub(crate) parent_canonical_id: &'a str,
    pub(crate) dependency: &'a str,
    pub(crate) reference: String,
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
        let request = self.apply_registry_auth(reference, request)?;
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
        request = self.apply_registry_auth(reference, request)?;
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
    ) -> Result<RequestBuilder> {
        Ok(match self.auth.get(&reference.registry)? {
            Some(RegistryAuth::Basic { username, password }) => {
                request.basic_auth(username, Some(password))
            }
            Some(RegistryAuth::Bearer(token)) => self.apply_bearer_auth(reference, request, &token),
            None => request,
        })
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
        DockerConfigAuthStore::from_config_file(path)?.get(registry)
    }
}

#[derive(Debug, Clone, Default)]
struct DockerConfigAuthStore {
    entries: BTreeMap<String, RegistryAuth>,
    cred_helpers: BTreeMap<String, String>,
    creds_store: Option<String>,
    helper_paths: Vec<PathBuf>,
}

impl DockerConfigAuthStore {
    fn from_default_config() -> Result<Self> {
        let path = match env::var_os("DOCKER_CONFIG").map(PathBuf::from) {
            Some(config_dir) => config_dir.join("config.json"),
            None => {
                let Some(home) = env::var_os("HOME").map(PathBuf::from) else {
                    return Ok(Self::default());
                };
                home.join(".docker").join("config.json")
            }
        };
        if !path.exists() {
            return Ok(Self::default());
        }

        Self::from_config_file(&path)
    }

    fn from_config_file(path: &Path) -> Result<Self> {
        let helper_paths = env::var_os("PATH")
            .map(|paths| env::split_paths(&paths).collect::<Vec<_>>())
            .unwrap_or_default();
        Self::from_config_file_with_helper_paths(path, &helper_paths)
    }

    fn from_config_file_with_helper_paths(path: &Path, helper_paths: &[PathBuf]) -> Result<Self> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read Docker config: {}", path.display()))?;
        let config: DockerConfigFile = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse Docker config: {}", path.display()))?;
        let mut entries = BTreeMap::new();
        for (registry, entry) in &config.auths {
            if let Some(auth) = entry.to_registry_auth().with_context(|| {
                format!(
                    "Failed to parse Docker registry auth for {registry} in {}",
                    path.display()
                )
            })? {
                entries.insert(normalize_registry_auth_key(registry), auth);
            }
        }

        Ok(Self {
            entries,
            cred_helpers: config
                .cred_helpers
                .into_iter()
                .map(|(registry, helper)| (normalize_registry_auth_key(&registry), helper))
                .collect(),
            creds_store: config.creds_store,
            helper_paths: helper_paths.to_vec(),
        })
    }

    fn get(&self, registry: &str) -> Result<Option<RegistryAuth>> {
        let registry = normalize_registry_auth_key(registry);
        let lookup_keys = registry_auth_lookup_keys(&registry);
        let helper_server = registry_auth_helper_server(&registry);
        if let Some(helper) = self.credential_helper_for_registry(&lookup_keys) {
            return docker_config_helper_auth(helper, &helper_server, &self.helper_paths)
                .with_context(|| {
                    format!("Failed to read Docker registry credential helper auth for {registry}")
                });
        }
        if let Some(helper) = self.creds_store.as_deref() {
            return docker_config_helper_auth(helper, &helper_server, &self.helper_paths)
                .with_context(|| {
                    format!("Failed to read Docker registry credential helper auth for {registry}")
                });
        }

        for key in lookup_keys {
            if let Some(auth) = self.entries.get(&key) {
                return Ok(Some(auth.clone()));
            }
        }

        Ok(None)
    }

    fn credential_helper_for_registry<'a>(&'a self, lookup_keys: &[String]) -> Option<&'a str> {
        lookup_keys
            .iter()
            .find_map(|registry| self.cred_helpers.get(registry).map(String::as_str))
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

pub(crate) fn prepare_feature_install_plan(
    features: &[ResolvedFeature],
    devcontainer_file: &Path,
    workspace_root: &Path,
    feature_archive_cache_root: &Path,
    extract_root: &Path,
    override_feature_install_order: &[String],
    update_features: bool,
) -> Result<Option<PreparedFeatureInstallPlan>> {
    let lock_path = workspace_root.join(".decune").join("features.lock.toml");
    if features.is_empty() {
        remove_feature_lock_file(&lock_path)?;
        return Ok(None);
    }

    let devcontainer_dir = devcontainer_file.parent().with_context(|| {
        format!(
            "Failed to resolve devcontainer directory for {}",
            devcontainer_file.display()
        )
    })?;
    let lock = read_feature_lock_file(&lock_path)?;
    let mut resolver = FeatureResolver {
        devcontainer_dir,
        lock: &lock,
        update_features,
        feature_archive_cache_root: feature_archive_cache_root.to_path_buf(),
        extract_root: extract_root.to_path_buf(),
        sources: BTreeMap::new(),
        next_local_instance: 0,
    };

    let inputs = features
        .iter()
        .map(|feature| resolver.resolve_input(feature.clone()))
        .collect::<Result<Vec<_>>>()?;
    let entries =
        resolve_feature_install_order(inputs, override_feature_install_order, |request| {
            let feature = ResolvedFeature {
                id: request.reference.clone(),
                canonical_id: request.canonical_id.clone(),
                options: request.options.clone(),
            };
            resolver.resolve_input(feature)
        })?;

    let mut prepared_entries = Vec::new();
    let mut metadata_layers = Vec::new();
    let mut lock_entries = Vec::new();
    let mut cumulative_container_env = BTreeMap::new();
    for entry in entries {
        let source = resolver.sources.get(&entry.source_key).ok_or_else(|| {
            anyhow!(
                "Feature source was not prepared for {}",
                entry.feature.canonical_id
            )
        })?;
        cumulative_container_env.extend(source.container_env.clone());
        prepared_entries.push(PreparedFeatureInstallEntry {
            feature: entry.feature,
            source_dir: source.source_dir.clone(),
            option_env: entry.option_env,
            container_env: cumulative_container_env.clone(),
        });
        metadata_layers.push(source.layer.clone());
        lock_entries.push(FeatureLockHashEntry {
            feature_id: entry.instance_key,
            digest: source.digest.clone(),
        });
    }

    lock_entries.sort_by(|left, right| left.feature_id.cmp(&right.feature_id));
    lock_entries
        .dedup_by(|left, right| left.feature_id == right.feature_id && left.digest == right.digest);
    let mut lock_file_entries = resolver
        .sources
        .values()
        .filter_map(|source| source.lock_file_entry.clone())
        .collect::<Vec<_>>();
    lock_file_entries.sort_by(|left, right| {
        left.id
            .cmp(&right.id)
            .then_with(|| left.reference.cmp(&right.reference))
            .then_with(|| left.digest.cmp(&right.digest))
    });
    lock_file_entries.dedup_by(|left, right| {
        left.id == right.id && left.reference == right.reference && left.digest == right.digest
    });
    if lock_file_entries.is_empty() {
        remove_feature_lock_file(&lock_path)?;
    } else {
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
        nodes.entry(input.instance_key.clone()).or_insert(input);
    }

    let mut scan_queue = nodes.keys().cloned().collect::<VecDeque<_>>();
    let mut dependency_edges = BTreeMap::<String, BTreeSet<String>>::new();
    while let Some(canonical_id) = scan_queue.pop_front() {
        let input = nodes
            .get(&canonical_id)
            .expect("queued Feature must exist in install graph");
        let parent_canonical_id = input.feature.canonical_id.clone();
        let parent_reference = input.reference.clone();
        let depends_on = input
            .metadata
            .depends_on
            .iter()
            .map(|(dependency, options)| (dependency.clone(), options.clone()))
            .collect::<Vec<_>>();

        for (dependency, options) in depends_on {
            let dependency_target = dependency_feature_target(&parent_reference, &dependency)?;
            let options = feature_dependency_options(&parent_canonical_id, &dependency, &options)?;
            if let Some(existing_instance_key) = find_existing_feature_instance(
                &nodes,
                &dependency_target.canonical_id,
                &dependency_target.reference,
                &options,
            ) {
                dependency_edges
                    .entry(canonical_id.clone())
                    .or_default()
                    .insert(existing_instance_key);
                continue;
            }
            let request = FeatureDependencyRequest {
                parent_canonical_id: &parent_canonical_id,
                dependency: &dependency,
                reference: dependency_target.reference,
                canonical_id: dependency_target.canonical_id.clone(),
                options,
            };
            let mut dependency_input = resolve_dependency(&request).with_context(|| {
                format!(
                    "Failed to resolve Feature dependency {} for Feature {}",
                    request.dependency, request.parent_canonical_id
                )
            })?;
            if dependency_input.feature.canonical_id != request.canonical_id {
                bail!(
                    "Feature dependency resolver returned {} for {}, expected {}",
                    dependency_input.feature.canonical_id,
                    request.dependency,
                    request.canonical_id
                );
            }
            if dependency_input.feature.options != request.options {
                dependency_input.feature.options = request.options.clone();
                dependency_input.instance_key =
                    feature_install_input_instance_key(&dependency_input);
            }
            let dependency_instance_key = dependency_input.instance_key.clone();
            dependency_edges
                .entry(canonical_id.clone())
                .or_default()
                .insert(dependency_instance_key.clone());
            if nodes
                .insert(dependency_instance_key.clone(), dependency_input)
                .is_none()
            {
                scan_queue.push_back(dependency_instance_key);
            }
        }
    }

    let mut dependencies = BTreeMap::new();
    for (instance_key, input) in &nodes {
        let mut required = dependency_edges.remove(instance_key).unwrap_or_default();
        for dependency in &input.metadata.installs_after {
            let dependency_target =
                soft_order_feature_target(&input.reference, dependency, "installsAfter")?;
            for (candidate_key, candidate) in &nodes {
                if feature_matches_order_target(candidate, &dependency_target.canonical_id)? {
                    required.insert(candidate_key.clone());
                }
            }
        }
        dependencies.insert(instance_key.clone(), required);
    }

    let priorities = override_feature_install_priorities(&nodes, override_feature_install_order)?;
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
            .map(|instance_key| {
                let canonical_id = &nodes
                    .get(instance_key)
                    .expect("ready Feature must exist in install graph")
                    .instance_key;
                feature_round_priority(canonical_id, &priorities)
            })
            .max()
            .unwrap_or_default();
        let mut round = ready
            .into_iter()
            .filter(|instance_key| {
                let canonical_id = &nodes
                    .get(instance_key)
                    .expect("ready Feature must exist in install graph")
                    .instance_key;
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
                source_key: input.source_key.clone(),
                instance_key: input.instance_key.clone(),
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
    let mut env_sources = BTreeMap::new();

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
            insert_feature_option_env(&mut env, &mut env_sources, &feature.id, option, value)?;
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
        insert_feature_option_env(&mut env, &mut env_sources, &feature.id, option, value)?;
    }

    Ok(env)
}

fn insert_feature_option_env(
    env: &mut BTreeMap<String, String>,
    env_sources: &mut BTreeMap<String, String>,
    feature_id: &str,
    option: &str,
    value: String,
) -> Result<()> {
    let key = feature_option_env_name(option);
    if let Some(existing_option) = env_sources.get(&key) {
        bail!(
            "Feature option environment variable collision for {feature_id}: options `{existing_option}` and `{option}` both map to {key}"
        );
    }
    env_sources.insert(key.clone(), option.to_owned());
    env.insert(key, value);
    Ok(())
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
    feature_archive_cache_root: PathBuf,
    extract_root: PathBuf,
    sources: BTreeMap<String, FeatureSource>,
    next_local_instance: usize,
}

impl FeatureResolver<'_> {
    fn resolve_input(&mut self, feature: ResolvedFeature) -> Result<FeatureInstallInput> {
        let reference = parse_feature_ref_from_devcontainer_dir(&feature.id, self.devcontainer_dir)
            .with_context(|| format!("Failed to parse Feature ref: {}", feature.id))?;
        let source_key = feature_source_key(&reference);
        let local_instance = matches!(reference, FeatureRef::Local(_)).then(|| {
            let instance = self.next_local_instance;
            self.next_local_instance += 1;
            instance
        });

        if let Some(source) = self.sources.get(&source_key) {
            let instance_key = feature_instance_key(&feature, source, local_instance);
            return Ok(FeatureInstallInput {
                feature,
                reference,
                metadata: source.metadata.clone(),
                source_key,
                instance_key,
            });
        }

        let source = self.resolve_source(&reference)?;
        let metadata = source.metadata.clone();
        let instance_key = feature_instance_key(&feature, &source, local_instance);
        self.sources.insert(source_key.clone(), source);

        Ok(FeatureInstallInput {
            feature,
            reference,
            metadata,
            source_key,
            instance_key,
        })
    }

    fn resolve_source(&self, reference: &FeatureRef) -> Result<FeatureSource> {
        match reference {
            FeatureRef::Local(local) => self.resolve_local_source(local),
            FeatureRef::Oci(oci) => self.resolve_oci_source(oci),
        }
    }

    fn resolve_local_source(&self, reference: &LocalFeatureRef) -> Result<FeatureSource> {
        let devcontainer_dir = self.devcontainer_dir.canonicalize().with_context(|| {
            format!(
                "Failed to resolve devcontainer directory: {}",
                self.devcontainer_dir.display()
            )
        })?;
        let source_dir = reference.path.canonicalize().with_context(|| {
            format!(
                "Failed to resolve local Feature directory: {}",
                reference.path.display()
            )
        })?;
        if !source_dir.starts_with(&devcontainer_dir) {
            bail!(
                "Local Feature path must stay inside devcontainer directory {}: {}",
                devcontainer_dir.display(),
                source_dir.display()
            );
        }
        ensure_feature_files(&source_dir)?;
        let document =
            read_feature_metadata_document(&source_dir.join("devcontainer-feature.json"))?;
        validate_local_feature_directory_name(&source_dir, &document.metadata.id)?;
        let digest = local_feature_content_digest(&source_dir)?;
        let container_env = feature_layer_container_env(&document.layer);

        Ok(FeatureSource {
            source_dir,
            metadata: document.metadata,
            layer: document.layer,
            container_env,
            digest: digest.clone(),
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
            &self.feature_archive_cache_root,
            &self.extract_root,
            &HttpOciRegistryClient::from_docker_config()?,
        )?;
        let document = read_feature_metadata_document(&artifact.metadata_path)?;
        let container_env = feature_layer_container_env(&document.layer);

        Ok(FeatureSource {
            source_dir: artifact.extracted_dir,
            metadata: document.metadata,
            layer: document.layer,
            container_env,
            digest: artifact.digest.clone(),
            lock_file_entry: Some(FeatureLockEntry {
                id: reference.canonical_id.clone(),
                reference: reference.normalized_reference(),
                digest: artifact.digest,
            }),
        })
    }
}

fn feature_layer_container_env(layer: &ConfigLayer) -> BTreeMap<String, String> {
    layer
        .devcontainer
        .as_ref()
        .map(|devcontainer| devcontainer.container_env.clone())
        .unwrap_or_default()
}

fn feature_source_key(reference: &FeatureRef) -> String {
    match reference {
        FeatureRef::Oci(reference) => reference.normalized_reference(),
        FeatureRef::Local(reference) => reference.canonical_id.clone(),
    }
}

fn feature_instance_key(
    feature: &ResolvedFeature,
    source: &FeatureSource,
    local_instance: Option<usize>,
) -> String {
    let options = feature_options_sort_key(&feature.options)
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("\x1f");
    match local_instance {
        Some(instance) => format!(
            "local\x1e{}\x1e{}\x1e{options}\x1e{instance}",
            feature.canonical_id, source.digest
        ),
        None => format!(
            "oci\x1e{}\x1e{}\x1e{options}",
            feature.canonical_id, source.digest
        ),
    }
}

fn feature_install_input_instance_key(input: &FeatureInstallInput) -> String {
    let options = feature_options_sort_key(&input.feature.options)
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("\x1f");
    format!(
        "test\x1e{}\x1e{}\x1e{options}",
        input.feature.canonical_id, input.feature.id
    )
}

struct FeatureDependencyTarget {
    reference: String,
    canonical_id: String,
}

fn dependency_feature_target(
    parent: &FeatureRef,
    dependency: &str,
) -> Result<FeatureDependencyTarget> {
    if dependency.starts_with("./") {
        return Ok(FeatureDependencyTarget {
            reference: dependency.to_owned(),
            canonical_id: canonical_feature_dependency_id(dependency),
        });
    }

    if let Ok(reference) = parse_feature_ref(dependency) {
        return Ok(FeatureDependencyTarget {
            reference: dependency.to_owned(),
            canonical_id: reference.canonical_id().to_owned(),
        });
    }

    let FeatureRef::Oci(parent) = parent else {
        bail!(
            "Local Feature dependency {dependency} must use a relative ./ path or full OCI Feature ref"
        );
    };
    let reference = format!("{}/{dependency}", parent.canonical_repository());
    let parsed = parse_feature_ref(&reference)
        .with_context(|| format!("Failed to resolve Feature dependency ref: {dependency}"))?;

    Ok(FeatureDependencyTarget {
        reference,
        canonical_id: parsed.canonical_id().to_owned(),
    })
}

fn soft_order_feature_target(
    parent: &FeatureRef,
    dependency: &str,
    property: &str,
) -> Result<FeatureDependencyTarget> {
    ensure_feature_order_identifier_is_unpinned(dependency, property)?;
    dependency_feature_target(parent, dependency)
        .with_context(|| format!("Failed to resolve Feature {property} ref: {dependency}"))
}

fn ensure_feature_order_identifier_is_unpinned(value: &str, property: &str) -> Result<()> {
    let (without_digest, digest) = split_digest(value)?;
    if digest.is_some() {
        bail!("Feature {property} value `{value}` must not include a digest");
    }
    let (_, tag) = split_tag(without_digest);
    if tag.is_some() {
        bail!("Feature {property} value `{value}` must not include a version tag");
    }

    Ok(())
}

fn feature_order_value_is_unqualified(value: &str) -> bool {
    !value.starts_with("./") && parse_feature_ref(value).is_err()
}

fn feature_matches_order_value(
    input: &FeatureInstallInput,
    value: &str,
    property: &str,
) -> Result<bool> {
    ensure_feature_order_identifier_is_unpinned(value, property)?;
    if feature_order_value_is_unqualified(value) && matches!(input.reference, FeatureRef::Local(_))
    {
        return Ok(false);
    }
    let target = dependency_feature_target(&input.reference, value)
        .with_context(|| format!("Failed to resolve Feature {property} ref: {value}"))?;

    feature_matches_order_target(input, &target.canonical_id)
}

fn feature_matches_order_target(input: &FeatureInstallInput, canonical_id: &str) -> Result<bool> {
    if input.feature.canonical_id == canonical_id {
        return Ok(true);
    }

    for legacy_id in &input.metadata.legacy_ids {
        ensure_feature_order_identifier_is_unpinned(legacy_id, "legacyIds")?;
        if feature_order_value_is_unqualified(legacy_id)
            && matches!(input.reference, FeatureRef::Local(_))
        {
            continue;
        }
        let target = dependency_feature_target(&input.reference, legacy_id).with_context(|| {
            format!(
                "Failed to resolve Feature legacyIds value for {}: {}",
                input.feature.canonical_id, legacy_id
            )
        })?;
        if target.canonical_id == canonical_id {
            return Ok(true);
        }
    }

    Ok(false)
}

fn feature_cache_archive_path(cache_root: &Path, digest: &str) -> PathBuf {
    cache_root.join(format!("{}.tgz", cache_safe_digest(digest)))
}

fn feature_cache_metadata_path(archive_path: &Path) -> PathBuf {
    archive_path.with_extension("tgz.toml")
}

fn select_feature_archive_layer(manifest: &OciManifestResponse) -> Option<&OciLayerDescriptor> {
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

fn write_cache_archive(path: &Path, content: &[u8], metadata: &FeatureCacheMetadata) -> Result<()> {
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

fn find_existing_feature_instance(
    nodes: &BTreeMap<String, FeatureInstallInput>,
    dependency_id: &str,
    dependency_ref: &str,
    options: &BTreeMap<String, toml::Value>,
) -> Option<String> {
    nodes
        .iter()
        .find(|(_, input)| {
            input.feature.canonical_id == dependency_id
                && feature_reference_matches(input, dependency_ref)
                && feature_options_sort_key(&input.feature.options)
                    == feature_options_sort_key(options)
        })
        .map(|(instance_key, _)| instance_key.clone())
}

fn feature_reference_matches(input: &FeatureInstallInput, dependency_ref: &str) -> bool {
    if input.feature.id == dependency_ref {
        return true;
    }

    let Ok(FeatureRef::Oci(dependency)) = parse_feature_ref(dependency_ref) else {
        return false;
    };
    let FeatureRef::Oci(existing) = &input.reference else {
        return false;
    };

    existing.canonical_id == dependency.canonical_id
        && existing.tag == dependency.tag
        && existing.digest == dependency.digest
}

fn override_feature_install_priorities(
    nodes: &BTreeMap<String, FeatureInstallInput>,
    override_order: &[String],
) -> Result<BTreeMap<String, usize>> {
    let count = override_order.len();
    let mut priorities = BTreeMap::new();
    for (index, feature) in override_order.iter().enumerate() {
        ensure_feature_order_identifier_is_unpinned(feature, "overrideFeatureInstallOrder")?;
        for (instance_key, input) in nodes {
            if feature_matches_order_value(input, feature, "overrideFeatureInstallOrder")? {
                priorities.insert(instance_key.clone(), count - index);
            }
        }
    }

    Ok(priorities)
}

fn feature_round_priority(instance_key: &str, priorities: &BTreeMap<String, usize>) -> usize {
    priorities.get(instance_key).copied().unwrap_or_default()
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

    feature_resource_sort_name(left)
        .cmp(&feature_resource_sort_name(right))
        .then_with(|| feature_tag_sort_key(left).cmp(&feature_tag_sort_key(right)))
        .then_with(|| right.feature.options.len().cmp(&left.feature.options.len()))
        .then_with(|| {
            feature_options_sort_key(&left.feature.options)
                .cmp(&feature_options_sort_key(&right.feature.options))
        })
        .then_with(|| feature_canonical_sort_name(left).cmp(&feature_canonical_sort_name(right)))
        .then_with(|| left.feature.id.cmp(&right.feature.id))
}

fn feature_resource_sort_name(input: &FeatureInstallInput) -> String {
    match &input.reference {
        FeatureRef::Oci(reference) => reference.canonical_id.clone(),
        FeatureRef::Local(_) => input.feature.canonical_id.clone(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum FeatureTagSortKey {
    Tagged(Vec<FeatureTagPart>),
    Latest,
    Digest,
    Local,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum FeatureTagPart {
    Number(u64),
    Text(String),
}

fn feature_tag_sort_key(input: &FeatureInstallInput) -> FeatureTagSortKey {
    match &input.reference {
        FeatureRef::Local(_) => FeatureTagSortKey::Local,
        FeatureRef::Oci(reference) if reference.digest.is_some() => FeatureTagSortKey::Digest,
        FeatureRef::Oci(reference) if reference.tag.as_deref() == Some("latest") => {
            FeatureTagSortKey::Latest
        }
        FeatureRef::Oci(reference) => FeatureTagSortKey::Tagged(feature_tag_parts(
            reference.tag.as_deref().unwrap_or_default(),
        )),
    }
}

fn feature_tag_parts(tag: &str) -> Vec<FeatureTagPart> {
    tag.split(['.', '-', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            part.parse::<u64>()
                .map(FeatureTagPart::Number)
                .unwrap_or_else(|_| FeatureTagPart::Text(part.to_ascii_lowercase()))
        })
        .collect()
}

fn feature_canonical_sort_name(input: &FeatureInstallInput) -> String {
    match &input.reference {
        FeatureRef::Oci(reference) => reference
            .digest
            .clone()
            .unwrap_or_else(|| input.instance_key.clone()),
        FeatureRef::Local(_) => input.instance_key.clone(),
    }
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
    let mut sanitized = option
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();

    let prefix_len = sanitized
        .char_indices()
        .find(|(_, ch)| !ch.is_ascii_digit() && *ch != '_')
        .map(|(index, _)| index)
        .unwrap_or(sanitized.len());
    if prefix_len > 0 {
        sanitized.replace_range(..prefix_len, "_");
    } else if sanitized.is_empty() {
        sanitized.push('_');
    }

    sanitized
        .chars()
        .map(|ch| ch.to_ascii_uppercase())
        .collect()
}

fn registry_url(reference: &OciFeatureRef, suffix: &str) -> String {
    format!(
        "https://{}/v2/{}/{}",
        registry_endpoint_host(&reference.registry),
        reference.repository_path(),
        suffix
    )
}

fn registry_endpoint_host(registry: &str) -> &str {
    if matches!(
        registry,
        DOCKER_HUB_CANONICAL_HOST | DOCKER_HUB_INDEX_HOST | DOCKER_HUB_REGISTRY_HOST
    ) {
        DOCKER_HUB_REGISTRY_HOST
    } else {
        registry
    }
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

fn normalize_registry_auth_key(registry: &str) -> String {
    registry
        .strip_prefix("https://")
        .or_else(|| registry.strip_prefix("http://"))
        .unwrap_or(registry)
        .trim_end_matches('/')
        .to_owned()
}

fn registry_auth_lookup_keys(registry: &str) -> Vec<String> {
    if !is_docker_hub_auth_key(registry) {
        return vec![registry.to_owned()];
    }

    let mut keys = vec![registry.to_owned()];
    for key in [
        DOCKER_HUB_INDEX_AUTH_KEY,
        DOCKER_HUB_CANONICAL_HOST,
        DOCKER_HUB_REGISTRY_HOST,
        DOCKER_HUB_INDEX_HOST,
    ] {
        if !keys.iter().any(|existing| existing.as_str() == key) {
            keys.push(key.to_owned());
        }
    }

    keys
}

fn registry_auth_helper_server(registry: &str) -> String {
    if is_docker_hub_auth_key(registry) {
        DOCKER_HUB_CREDENTIAL_HELPER_SERVER.to_owned()
    } else {
        registry.to_owned()
    }
}

fn is_docker_hub_auth_key(registry: &str) -> bool {
    matches!(
        registry,
        DOCKER_HUB_CANONICAL_HOST
            | DOCKER_HUB_REGISTRY_HOST
            | DOCKER_HUB_INDEX_HOST
            | DOCKER_HUB_INDEX_AUTH_KEY
    )
}

fn bearer_challenge(headers: &HeaderMap) -> Option<BearerChallenge> {
    let value = headers.get(WWW_AUTHENTICATE)?.to_str().ok()?;
    parse_bearer_challenge(value)
}

fn parse_bearer_challenge(value: &str) -> Option<BearerChallenge> {
    let value = value.trim_start();
    let scheme_end = value.find(char::is_whitespace)?;
    let scheme = &value[..scheme_end];
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let parameters = value[scheme_end..].trim_start();
    let mut challenge = BearerChallenge::default();
    for entry in split_auth_parameters(parameters) {
        let Some((key, value)) = entry.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"').to_owned();
        let key = key.trim();
        if key.eq_ignore_ascii_case("realm") {
            challenge.realm = value;
        } else if key.eq_ignore_ascii_case("service") {
            challenge.service = Some(value);
        } else if key.eq_ignore_ascii_case("scope") {
            challenge.scope = Some(value);
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
    #[serde(default, rename = "credHelpers")]
    cred_helpers: BTreeMap<String, String>,
    #[serde(default, rename = "credsStore")]
    creds_store: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DockerAuthEntry {
    auth: Option<String>,
    username: Option<String>,
    password: Option<String>,
    identitytoken: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DockerCredentialHelperGetResponse {
    username: Option<String>,
    secret: Option<String>,
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

fn docker_config_helper_auth(
    helper: &str,
    helper_server: &str,
    helper_paths: &[PathBuf],
) -> Result<Option<RegistryAuth>> {
    let Some(binary) = docker_credential_helper_binary(helper, helper_paths) else {
        bail!("Docker credential helper was not found: docker-credential-{helper}");
    };
    let Some(output) = run_docker_credential_helper_get(&binary, helper_server)? else {
        return Ok(None);
    };
    let Some(secret) = output.secret else {
        return Ok(None);
    };
    let username = output.username.unwrap_or_default();
    if username == "<token>" {
        Ok(Some(RegistryAuth::Bearer(secret)))
    } else {
        Ok(Some(RegistryAuth::Basic {
            username,
            password: secret,
        }))
    }
}

fn docker_credential_helper_binary(helper: &str, helper_paths: &[PathBuf]) -> Option<PathBuf> {
    let binary = format!("docker-credential-{helper}");
    helper_paths
        .iter()
        .map(|path| path.join(&binary))
        .find(|path| path.is_file())
}

fn run_docker_credential_helper_get(
    binary: &Path,
    registry: &str,
) -> Result<Option<DockerCredentialHelperGetResponse>> {
    let mut child = spawn_docker_credential_helper_get(binary)?;
    child
        .stdin
        .as_mut()
        .context("Docker credential helper stdin was not available")?
        .write_all(registry.as_bytes())
        .with_context(|| {
            format!(
                "Failed to write registry to Docker credential helper: {}",
                binary.display()
            )
        })?;
    let output = child.wait_with_output().with_context(|| {
        format!(
            "Failed to wait for Docker credential helper: {}",
            binary.display()
        )
    })?;
    if !output.status.success() {
        let message = docker_credential_helper_error_message(&output);
        if docker_credential_helper_credentials_not_found(&message) {
            return Ok(None);
        }
        bail!(
            "Docker credential helper failed for {registry}: {}",
            message
        );
    }

    serde_json::from_slice(&output.stdout)
        .map(Some)
        .with_context(|| {
            format!(
                "Failed to parse Docker credential helper output: {}",
                binary.display()
            )
        })
}

fn spawn_docker_credential_helper_get(binary: &Path) -> Result<Child> {
    let mut last_error = None;
    for attempt in 0..=DOCKER_CREDENTIAL_HELPER_SPAWN_RETRIES {
        match docker_credential_helper_get_command(binary).spawn() {
            Ok(child) => return Ok(child),
            Err(error)
                if is_text_file_busy(&error)
                    && attempt < DOCKER_CREDENTIAL_HELPER_SPAWN_RETRIES =>
            {
                last_error = Some(error);
                thread::sleep(DOCKER_CREDENTIAL_HELPER_SPAWN_RETRY_DELAY);
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "Failed to spawn Docker credential helper: {}",
                        binary.display()
                    )
                });
            }
        }
    }

    Err(last_error
        .unwrap_or_else(|| io::Error::other("Docker credential helper spawn retry exhausted")))
    .with_context(|| {
        format!(
            "Failed to spawn Docker credential helper: {}",
            binary.display()
        )
    })
}

fn docker_credential_helper_get_command(binary: &Path) -> HostCommand {
    let mut command = HostCommand::new(binary);
    command
        .arg("get")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn is_text_file_busy(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::ETXTBSY)
}

fn docker_credential_helper_error_message(output: &std::process::Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    [stdout.trim(), stderr.trim()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(": ")
}

fn docker_credential_helper_credentials_not_found(message: &str) -> bool {
    message
        .to_ascii_lowercase()
        .contains("credentials not found")
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
    use std::{
        cell::Cell, collections::BTreeMap, fs, io::Write, os::unix::fs::PermissionsExt, path::Path,
    };

    use flate2::{Compression, write::GzEncoder};
    use tar::{Builder, Header};

    use super::*;

    const MANIFEST_DIGEST_HEX: &str =
        "1111111111111111111111111111111111111111111111111111111111111111";

    fn write_credential_helper(path: &Path, contents: &str) {
        let staged = path.with_extension(format!("tmp-{}", std::process::id()));
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&staged)
            .unwrap();
        file.write_all(contents.as_bytes()).unwrap();
        file.flush().unwrap();
        file.sync_all().unwrap();
        drop(file);
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o755)).unwrap();
        fs::rename(&staged, path).unwrap();
    }

    #[test]
    fn local_feature_metadata_id_must_match_directory_name() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_root = temp.path().join("workspace");
        let devcontainer_dir = workspace_root.join(".devcontainer");
        let feature_dir = devcontainer_dir.join("features/tool");
        let cache_root = temp.path().join("cache");
        fs::create_dir_all(&feature_dir).unwrap();
        fs::write(devcontainer_dir.join("devcontainer.json"), "{}").unwrap();
        fs::write(
            feature_dir.join("devcontainer-feature.json"),
            r#"{"id":"different-tool","version":"1.0.0","name":"Different Tool"}"#,
        )
        .unwrap();
        fs::write(feature_dir.join("install.sh"), "#!/bin/sh\n").unwrap();
        let features = vec![ResolvedFeature {
            id: "./features/tool".to_owned(),
            canonical_id: "local:features/tool".to_owned(),
            options: BTreeMap::new(),
        }];

        let error = prepare_feature_install_plan(
            &features,
            &devcontainer_dir.join("devcontainer.json"),
            &workspace_root,
            &cache_root,
            &cache_root.join("extracted"),
            &[],
            false,
        )
        .unwrap_err();

        assert!(
            error.to_string().contains("Local Feature directory name"),
            "{error:#}"
        );
        assert!(error.to_string().contains("tool"), "{error:#}");
        assert!(error.to_string().contains("different-tool"), "{error:#}");
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
            r#"{"id":"local","version":"1.0.0","name":"Local"}"#,
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
            &cache_root.join("extracted"),
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
            &cache_root.join("extracted"),
            &[],
            false,
        )
        .unwrap()
        .unwrap();

        assert_eq!(first.lock_entries.len(), 1);
        assert_eq!(second.lock_entries.len(), 1);
        assert!(
            first.lock_entries[0]
                .feature_id
                .contains("local:features/local")
        );
        assert!(
            second.lock_entries[0]
                .feature_id
                .contains("local:features/local")
        );
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
                "version": "1.0.0",
                "name": "Tool",
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
            r#"{"id":"base","version":"1.0.0","name":"Base"}"#,
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
            &cache_root.join("extracted"),
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
    fn local_feature_path_must_stay_inside_devcontainer_dir() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_root = temp.path().join("workspace");
        let devcontainer_dir = workspace_root.join(".devcontainer");
        let feature_dir = workspace_root.join("outside-feature");
        let cache_root = temp.path().join("cache");
        fs::create_dir_all(&devcontainer_dir).unwrap();
        fs::create_dir_all(&feature_dir).unwrap();
        fs::write(devcontainer_dir.join("devcontainer.json"), "{}").unwrap();
        fs::write(
            feature_dir.join("devcontainer-feature.json"),
            r#"{"id":"outside-feature","version":"1.0.0","name":"Outside Feature"}"#,
        )
        .unwrap();
        fs::write(feature_dir.join("install.sh"), "#!/bin/sh\n").unwrap();
        let features = vec![ResolvedFeature {
            id: "./../outside-feature".to_owned(),
            canonical_id: "local:../outside-feature".to_owned(),
            options: BTreeMap::new(),
        }];

        let error = prepare_feature_install_plan(
            &features,
            &devcontainer_dir.join("devcontainer.json"),
            &workspace_root,
            &cache_root,
            &cache_root.join("extracted"),
            &[],
            false,
        )
        .unwrap_err();

        assert!(error.to_string().contains(".devcontainer"), "{error:#}");
    }

    #[test]
    fn prepared_plan_removes_stale_lock_file_when_feature_graph_is_empty() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_root = temp.path().join("workspace");
        let devcontainer_dir = workspace_root.join(".devcontainer");
        let lock_path = workspace_root.join(".decune/features.lock.toml");
        fs::create_dir_all(&devcontainer_dir).unwrap();
        fs::write(devcontainer_dir.join("devcontainer.json"), "{}").unwrap();
        write_feature_lock_file(
            &lock_path,
            &FeatureLockFile {
                version: FEATURE_LOCK_VERSION,
                features: vec![FeatureLockEntry {
                    id: "ghcr.io/example/features/tool".to_owned(),
                    reference: "ghcr.io/example/features/tool:1".to_owned(),
                    digest: "sha256:locked".to_owned(),
                }],
            },
        )
        .unwrap();

        let plan = prepare_feature_install_plan(
            &[],
            &devcontainer_dir.join("devcontainer.json"),
            &workspace_root,
            &temp.path().join("cache"),
            &temp.path().join("extract"),
            &[],
            false,
        )
        .unwrap();

        assert!(plan.is_none());
        assert!(!lock_path.exists());
    }

    #[test]
    fn prepared_plan_removes_stale_lock_file_when_only_local_features_remain() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_root = temp.path().join("workspace");
        let devcontainer_dir = workspace_root.join(".devcontainer");
        let local_feature_dir = devcontainer_dir.join("tool");
        let lock_path = workspace_root.join(".decune/features.lock.toml");
        fs::create_dir_all(&local_feature_dir).unwrap();
        fs::write(devcontainer_dir.join("devcontainer.json"), "{}").unwrap();
        fs::write(local_feature_dir.join("install.sh"), "#!/bin/sh\n").unwrap();
        fs::write(
            local_feature_dir.join("devcontainer-feature.json"),
            r#"{"id":"tool","version":"1.0.0","name":"Tool"}"#,
        )
        .unwrap();
        write_feature_lock_file(
            &lock_path,
            &FeatureLockFile {
                version: FEATURE_LOCK_VERSION,
                features: vec![FeatureLockEntry {
                    id: "ghcr.io/example/features/tool".to_owned(),
                    reference: "ghcr.io/example/features/tool:1".to_owned(),
                    digest: "sha256:locked".to_owned(),
                }],
            },
        )
        .unwrap();

        let plan = prepare_feature_install_plan(
            &[ResolvedFeature {
                id: "./tool".to_owned(),
                canonical_id: "local:tool".to_owned(),
                options: BTreeMap::new(),
            }],
            &devcontainer_dir.join("devcontainer.json"),
            &workspace_root,
            &temp.path().join("cache"),
            &temp.path().join("extract"),
            &[],
            false,
        )
        .unwrap()
        .unwrap();

        assert_eq!(plan.entries.len(), 1);
        assert!(!lock_path.exists());
    }

    #[test]
    fn prepared_plan_lock_file_preserves_same_feature_id_with_distinct_digests() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_root = temp.path().join("workspace");
        let devcontainer_dir = workspace_root.join(".devcontainer");
        let cache_root = temp.path().join("cache");
        let extract_root = temp.path().join("extract");
        fs::create_dir_all(&devcontainer_dir).unwrap();
        fs::create_dir_all(&cache_root).unwrap();
        fs::write(devcontainer_dir.join("devcontainer.json"), "{}").unwrap();
        let first_digest =
            "sha256:1111111111111111111111111111111111111111111111111111111111111111";
        let second_digest =
            "sha256:2222222222222222222222222222222222222222222222222222222222222222";
        write_cached_feature_archive_for_manifest(&cache_root, first_digest, "tool-one");
        write_cached_feature_archive_for_manifest(&cache_root, second_digest, "tool-two");

        let plan = prepare_feature_install_plan(
            &[
                ResolvedFeature {
                    id: format!("ghcr.io/example/features/tool@{first_digest}"),
                    canonical_id: "ghcr.io/example/features/tool".to_owned(),
                    options: BTreeMap::new(),
                },
                ResolvedFeature {
                    id: format!("ghcr.io/example/features/tool@{second_digest}"),
                    canonical_id: "ghcr.io/example/features/tool".to_owned(),
                    options: BTreeMap::new(),
                },
            ],
            &devcontainer_dir.join("devcontainer.json"),
            &workspace_root,
            &cache_root,
            &extract_root,
            &[],
            false,
        )
        .unwrap()
        .unwrap();
        let lock =
            read_feature_lock_file(&workspace_root.join(".decune/features.lock.toml")).unwrap();

        assert_eq!(plan.entries.len(), 2);
        assert_eq!(
            lock.features
                .iter()
                .map(|entry| entry.digest.as_str())
                .collect::<Vec<_>>(),
            vec![first_digest, second_digest]
        );
    }

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
    fn feature_install_order_resolves_oci_sibling_short_depends_on() {
        let plan = resolve_feature_install_order(
            vec![feature_install_input(
                "ghcr.io/example/features/tool:1",
                FeatureMetadata {
                    depends_on: BTreeMap::from([(
                        "base:1".to_owned(),
                        serde_json::json!({
                            "version": "1.2"
                        }),
                    )]),
                    ..FeatureMetadata::default()
                },
            )],
            &[],
            |request| {
                assert_eq!(request.dependency, "base:1");
                assert_eq!(request.canonical_id, "ghcr.io/example/features/base");
                Ok(feature_install_input(
                    &request.reference,
                    FeatureMetadata::default(),
                ))
            },
        )
        .unwrap();

        assert_eq!(
            plan.iter()
                .map(|entry| entry.feature.id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "ghcr.io/example/features/base:1",
                "ghcr.io/example/features/tool:1",
            ]
        );
        assert_eq!(
            plan[0].feature.options.get("version"),
            Some(&toml::Value::String("1.2".to_owned()))
        );
    }

    #[test]
    fn feature_install_order_resolves_oci_sibling_short_installs_after() {
        let plan = resolve_feature_install_order(
            vec![
                feature_install_input(
                    "ghcr.io/example/features/tool:1",
                    FeatureMetadata::default(),
                ),
                feature_install_input(
                    "ghcr.io/example/features/lint:1",
                    FeatureMetadata {
                        installs_after: vec!["tool".to_owned()],
                        ..FeatureMetadata::default()
                    },
                ),
            ],
            &[],
            missing_feature_dependency,
        )
        .unwrap();

        assert_eq!(
            plan.iter()
                .map(|entry| entry.feature.id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "ghcr.io/example/features/tool:1",
                "ghcr.io/example/features/lint:1",
            ]
        );
    }

    #[test]
    fn feature_install_order_matches_legacy_ids_for_installs_after() {
        let plan = resolve_feature_install_order(
            vec![
                feature_install_input(
                    "ghcr.io/example/features/new-tool:1",
                    FeatureMetadata {
                        legacy_ids: vec!["old-tool".to_owned()],
                        ..FeatureMetadata::default()
                    },
                ),
                feature_install_input(
                    "ghcr.io/example/features/lint:1",
                    FeatureMetadata {
                        installs_after: vec!["old-tool".to_owned()],
                        ..FeatureMetadata::default()
                    },
                ),
            ],
            &[],
            missing_feature_dependency,
        )
        .unwrap();

        assert_eq!(
            plan.iter()
                .map(|entry| entry.feature.id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "ghcr.io/example/features/new-tool:1",
                "ghcr.io/example/features/lint:1",
            ]
        );
    }

    #[test]
    fn feature_install_order_matches_mixed_case_feature_ids() {
        let plan = resolve_feature_install_order(
            vec![
                feature_install_input(
                    "GHCR.IO/Example/Features/Base:1",
                    FeatureMetadata::default(),
                ),
                feature_install_input(
                    "ghcr.io/example/features/tool:1",
                    FeatureMetadata {
                        installs_after: vec!["BASE".to_owned()],
                        ..FeatureMetadata::default()
                    },
                ),
                feature_install_input(
                    "ghcr.io/example/features/lint:1",
                    FeatureMetadata {
                        installs_after: vec!["GhCr.Io/Example/Features/Tool".to_owned()],
                        ..FeatureMetadata::default()
                    },
                ),
            ],
            &["LINT".to_owned()],
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
    fn feature_install_order_reuses_latest_dependency_instance_with_implicit_or_explicit_tag() {
        let plan = resolve_feature_install_order(
            vec![
                feature_install_input(
                    "ghcr.io/example/features/base:latest",
                    FeatureMetadata::default(),
                ),
                feature_install_input(
                    "ghcr.io/example/features/tool:1",
                    FeatureMetadata {
                        depends_on: BTreeMap::from([("base".to_owned(), serde_json::json!({}))]),
                        ..FeatureMetadata::default()
                    },
                ),
            ],
            &[],
            missing_feature_dependency,
        )
        .unwrap();

        assert_eq!(
            plan.iter()
                .map(|entry| entry.feature.id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "ghcr.io/example/features/base:latest",
                "ghcr.io/example/features/tool:1",
            ]
        );

        let plan = resolve_feature_install_order(
            vec![
                feature_install_input("ghcr.io/example/features/base", FeatureMetadata::default()),
                feature_install_input(
                    "ghcr.io/example/features/tool:1",
                    FeatureMetadata {
                        depends_on: BTreeMap::from([(
                            "base:latest".to_owned(),
                            serde_json::json!({}),
                        )]),
                        ..FeatureMetadata::default()
                    },
                ),
            ],
            &[],
            missing_feature_dependency,
        )
        .unwrap();

        assert_eq!(
            plan.iter()
                .map(|entry| entry.feature.id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "ghcr.io/example/features/base",
                "ghcr.io/example/features/tool:1",
            ]
        );
    }

    #[test]
    fn feature_install_order_treats_same_dependency_with_different_tags_as_distinct_instances() {
        let mut resolved_dependency = false;
        let plan = resolve_feature_install_order(
            vec![
                feature_install_input(
                    "ghcr.io/example/features/base:1",
                    FeatureMetadata::default(),
                ),
                feature_install_input(
                    "ghcr.io/example/features/tool:1",
                    FeatureMetadata {
                        depends_on: BTreeMap::from([(
                            "ghcr.io/example/features/base:2".to_owned(),
                            serde_json::json!({}),
                        )]),
                        ..FeatureMetadata::default()
                    },
                ),
            ],
            &[],
            |request| {
                resolved_dependency = true;
                assert_eq!(request.reference, "ghcr.io/example/features/base:2");
                Ok(feature_install_input(
                    &request.reference,
                    FeatureMetadata::default(),
                ))
            },
        )
        .unwrap();

        assert!(resolved_dependency);
        assert_eq!(
            plan.iter()
                .map(|entry| entry.feature.id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "ghcr.io/example/features/base:1",
                "ghcr.io/example/features/base:2",
                "ghcr.io/example/features/tool:1",
            ]
        );
    }

    #[test]
    fn feature_install_order_treats_same_dependency_with_different_digests_as_distinct_instances() {
        let first_digest =
            "sha256:1111111111111111111111111111111111111111111111111111111111111111";
        let second_digest =
            "sha256:2222222222222222222222222222222222222222222222222222222222222222";
        let mut resolved_dependency = false;
        let plan = resolve_feature_install_order(
            vec![
                feature_install_input(
                    &format!("ghcr.io/example/features/base@{first_digest}"),
                    FeatureMetadata::default(),
                ),
                feature_install_input(
                    "ghcr.io/example/features/tool:1",
                    FeatureMetadata {
                        depends_on: BTreeMap::from([(
                            format!("ghcr.io/example/features/base@{second_digest}"),
                            serde_json::json!({}),
                        )]),
                        ..FeatureMetadata::default()
                    },
                ),
            ],
            &[],
            |request| {
                resolved_dependency = true;
                assert_eq!(
                    request.reference,
                    format!("ghcr.io/example/features/base@{second_digest}")
                );
                Ok(feature_install_input(
                    &request.reference,
                    FeatureMetadata::default(),
                ))
            },
        )
        .unwrap();

        assert!(resolved_dependency);
        assert_eq!(
            plan.iter()
                .map(|entry| entry.feature.id.clone())
                .collect::<Vec<_>>(),
            vec![
                format!("ghcr.io/example/features/base@{first_digest}"),
                format!("ghcr.io/example/features/base@{second_digest}"),
                "ghcr.io/example/features/tool:1".to_owned(),
            ]
        );
    }

    #[test]
    fn feature_install_order_treats_same_dependency_with_different_options_as_distinct_instances() {
        let plan = resolve_feature_install_order(
            vec![
                feature_install_input(
                    "ghcr.io/example/features/base:1",
                    FeatureMetadata::default(),
                )
                .with_options([("version", toml::Value::String("top-level".to_owned()))]),
                feature_install_input(
                    "ghcr.io/example/features/tool:1",
                    FeatureMetadata {
                        depends_on: BTreeMap::from([(
                            "ghcr.io/example/features/base:1".to_owned(),
                            serde_json::json!({
                                "version": "dependency"
                            }),
                        )]),
                        ..FeatureMetadata::default()
                    },
                ),
            ],
            &[],
            |request| {
                Ok(feature_install_input(
                    request.dependency,
                    FeatureMetadata::default(),
                ))
            },
        )
        .unwrap();

        let base_versions = plan
            .iter()
            .filter(|entry| entry.feature.canonical_id == "ghcr.io/example/features/base")
            .map(|entry| {
                entry
                    .feature
                    .options
                    .get("version")
                    .and_then(toml::Value::as_str)
                    .unwrap_or("<missing>")
                    .to_owned()
            })
            .collect::<Vec<_>>();

        assert_eq!(base_versions, vec!["dependency", "top-level"]);
        assert_eq!(
            plan.iter()
                .map(|entry| entry.feature.canonical_id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "ghcr.io/example/features/base",
                "ghcr.io/example/features/base",
                "ghcr.io/example/features/tool",
            ]
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
    fn override_feature_install_order_resolves_oci_sibling_short_ids() {
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
            &["gamma".to_owned()],
            missing_feature_dependency,
        )
        .unwrap();

        assert_eq!(
            plan.iter()
                .map(|entry| entry.feature.id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "ghcr.io/example/features/gamma:1",
                "ghcr.io/example/features/alpha:1",
                "ghcr.io/example/features/beta:1",
            ]
        );
    }

    #[test]
    fn feature_install_order_rejects_versioned_soft_order_ids() {
        let installs_after_error = resolve_feature_install_order(
            vec![
                feature_install_input(
                    "ghcr.io/example/features/base:1",
                    FeatureMetadata::default(),
                ),
                feature_install_input(
                    "ghcr.io/example/features/tool:1",
                    FeatureMetadata {
                        installs_after: vec!["base:1".to_owned()],
                        ..FeatureMetadata::default()
                    },
                ),
            ],
            &[],
            missing_feature_dependency,
        )
        .unwrap_err();
        assert!(
            installs_after_error.to_string().contains("installsAfter"),
            "{installs_after_error:#}"
        );

        let override_error = resolve_feature_install_order(
            vec![feature_install_input(
                "ghcr.io/example/features/tool:1",
                FeatureMetadata::default(),
            )],
            &["tool:1".to_owned()],
            missing_feature_dependency,
        )
        .unwrap_err();
        assert!(
            override_error
                .to_string()
                .contains("overrideFeatureInstallOrder"),
            "{override_error:#}"
        );
    }

    #[test]
    fn feature_install_order_stable_sort_orders_numeric_tags_before_latest() {
        let digest = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
        let plan = resolve_feature_install_order(
            vec![
                feature_install_input(
                    "ghcr.io/example/features/tool:latest",
                    FeatureMetadata::default(),
                ),
                feature_install_input(
                    "ghcr.io/example/features/tool:10",
                    FeatureMetadata::default(),
                ),
                feature_install_input(
                    "ghcr.io/example/features/tool:2",
                    FeatureMetadata::default(),
                ),
                feature_install_input(
                    &format!("ghcr.io/example/features/tool@{digest}"),
                    FeatureMetadata::default(),
                ),
            ],
            &[],
            missing_feature_dependency,
        )
        .unwrap();

        assert_eq!(
            plan.iter()
                .map(|entry| entry.feature.id.clone())
                .collect::<Vec<_>>(),
            vec![
                "ghcr.io/example/features/tool:2".to_owned(),
                "ghcr.io/example/features/tool:10".to_owned(),
                "ghcr.io/example/features/tool:latest".to_owned(),
                format!("ghcr.io/example/features/tool@{digest}"),
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
                            ..FeatureOptionSchema::default()
                        },
                    ),
                    (
                        "installTools".to_owned(),
                        FeatureOptionSchema {
                            option_type: Some("boolean".to_owned()),
                            default: Some(serde_json::json!(true)),
                            enum_values: Vec::new(),
                            ..FeatureOptionSchema::default()
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
                            ..FeatureOptionSchema::default()
                        },
                    ),
                    (
                        "version".to_owned(),
                        FeatureOptionSchema {
                            option_type: Some("string".to_owned()),
                            default: Some(serde_json::json!("latest")),
                            enum_values: Vec::new(),
                            ..FeatureOptionSchema::default()
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
    fn feature_option_env_uses_feature_spec_name_conversion() {
        assert_eq!(feature_option_env_name("version"), "VERSION");
        assert_eq!(feature_option_env_name("install-zsh"), "INSTALL_ZSH");
        assert_eq!(feature_option_env_name("node.version"), "NODE_VERSION");
        assert_eq!(feature_option_env_name("1password"), "_PASSWORD");

        let feature = feature_install_input(
            "ghcr.io/example/features/tool:1",
            FeatureMetadata {
                options: BTreeMap::from([
                    (
                        "1password".to_owned(),
                        FeatureOptionSchema {
                            option_type: Some("string".to_owned()),
                            default: Some(serde_json::json!("secret")),
                            enum_values: Vec::new(),
                            ..FeatureOptionSchema::default()
                        },
                    ),
                    (
                        "_debug".to_owned(),
                        FeatureOptionSchema {
                            option_type: Some("boolean".to_owned()),
                            default: Some(serde_json::json!(true)),
                            enum_values: Vec::new(),
                            ..FeatureOptionSchema::default()
                        },
                    ),
                    (
                        "foo-bar".to_owned(),
                        FeatureOptionSchema {
                            option_type: Some("string".to_owned()),
                            default: Some(serde_json::json!("dash")),
                            enum_values: Vec::new(),
                            ..FeatureOptionSchema::default()
                        },
                    ),
                    (
                        "has space".to_owned(),
                        FeatureOptionSchema {
                            option_type: Some("string".to_owned()),
                            default: Some(serde_json::json!("space")),
                            enum_values: Vec::new(),
                            ..FeatureOptionSchema::default()
                        },
                    ),
                ]),
                ..FeatureMetadata::default()
            },
        );

        let env = feature_option_env(&feature.feature, &feature.metadata).unwrap();

        assert_eq!(env.get("_PASSWORD").map(String::as_str), Some("secret"));
        assert_eq!(env.get("_DEBUG").map(String::as_str), Some("true"));
        assert_eq!(env.get("FOO_BAR").map(String::as_str), Some("dash"));
        assert_eq!(env.get("HAS_SPACE").map(String::as_str), Some("space"));
    }

    #[test]
    fn feature_option_env_rejects_converted_env_key_collision() {
        let feature = feature_install_input(
            "ghcr.io/example/features/tool:1",
            FeatureMetadata {
                options: BTreeMap::from([
                    (
                        "foo-bar".to_owned(),
                        FeatureOptionSchema {
                            option_type: Some("string".to_owned()),
                            default: Some(serde_json::json!("dash")),
                            enum_values: Vec::new(),
                            ..FeatureOptionSchema::default()
                        },
                    ),
                    (
                        "foo_bar".to_owned(),
                        FeatureOptionSchema {
                            option_type: Some("string".to_owned()),
                            default: Some(serde_json::json!("underscore")),
                            enum_values: Vec::new(),
                            ..FeatureOptionSchema::default()
                        },
                    ),
                ]),
                ..FeatureMetadata::default()
            },
        );

        let error = feature_option_env(&feature.feature, &feature.metadata).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Feature option environment variable collision"),
            "{error:#}"
        );
        assert!(error.to_string().contains("FOO_BAR"), "{error:#}");
        assert!(error.to_string().contains("foo-bar"), "{error:#}");
        assert!(error.to_string().contains("foo_bar"), "{error:#}");
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
                        ..FeatureOptionSchema::default()
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
    fn feature_option_env_rejects_string_schema_with_enum_and_proposals() {
        let feature = feature_install_input(
            "ghcr.io/example/features/tool:1",
            FeatureMetadata {
                options: BTreeMap::from([(
                    "version".to_owned(),
                    FeatureOptionSchema {
                        option_type: Some("string".to_owned()),
                        default: Some(serde_json::json!("latest")),
                        enum_values: vec!["latest".to_owned()],
                        proposals: vec!["preview".to_owned()],
                        ..FeatureOptionSchema::default()
                    },
                )]),
                ..FeatureMetadata::default()
            },
        );

        let error = feature_option_env(&feature.feature, &feature.metadata).unwrap_err();

        assert!(
            error.to_string().contains(
                "Feature option ghcr.io/example/features/tool:1.version must not declare both enum and proposals"
            ),
            "{error:#}"
        );
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
    fn docker_hub_registry_url_uses_registry_endpoint() {
        let reference = parse_oci_feature_ref("docker.io/example/features/tool:1").unwrap();

        assert_eq!(
            registry_url(&reference, "manifests/1"),
            "https://registry-1.docker.io/v2/example/features/tool/manifests/1"
        );
    }

    #[test]
    fn bracketed_ipv6_registry_url_preserves_host_port() {
        let reference =
            parse_oci_feature_ref("[2001:db8::1]:5000/example/features/tool:1").unwrap();

        assert_eq!(
            registry_url(&reference, "manifests/1"),
            "https://[2001:db8::1]:5000/v2/example/features/tool/manifests/1"
        );
    }

    #[test]
    fn bearer_challenge_parses_case_insensitive_scheme_and_parameters() {
        let challenge = parse_bearer_challenge(
            r#"bearer REALM="https://auth.example/token", SERVICE="registry.example", SCOPE="repository:example/features/tool:pull""#,
        )
        .unwrap();

        assert_eq!(challenge.realm, "https://auth.example/token");
        assert_eq!(challenge.service.as_deref(), Some("registry.example"));
        assert_eq!(
            challenge.scope.as_deref(),
            Some("repository:example/features/tool:pull")
        );
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
    fn docker_config_auth_matches_docker_hub_index_auth_for_aliases() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config.json");
        fs::write(
            &config,
            r#"{"auths":{"https://index.docker.io/v1/":{"auth":"aHViOnRva2Vu"}}}"#,
        )
        .unwrap();
        let store = DockerConfigAuthStore::from_config_file(&config).unwrap();

        for registry in ["docker.io", "registry-1.docker.io", "index.docker.io"] {
            assert_eq!(
                store.get(registry).unwrap(),
                Some(RegistryAuth::Basic {
                    username: "hub".to_owned(),
                    password: "token".to_owned(),
                }),
                "{registry}"
            );
        }
    }

    #[test]
    fn docker_config_auth_uses_registry_credential_helper_before_inline_auth() {
        let temp = tempfile::tempdir().unwrap();
        let helper_dir = temp.path().join("bin");
        fs::create_dir_all(&helper_dir).unwrap();
        let helper = helper_dir.join("docker-credential-fake");
        write_credential_helper(
            &helper,
            r#"#!/bin/sh
set -eu
test "$1" = get
server="$(cat)"
test "$server" = ghcr.io
printf '{"Username":"helper-user","Secret":"helper-token"}'
"#,
        );
        let config = temp.path().join("config.json");
        fs::write(
            &config,
            r#"{
                "credHelpers": {
                    "ghcr.io": "fake"
                },
                "auths": {
                    "ghcr.io": {
                        "auth": "aW5saW5lOnRva2Vu"
                    }
                }
            }"#,
        )
        .unwrap();

        let store =
            DockerConfigAuthStore::from_config_file_with_helper_paths(&config, &[helper_dir])
                .unwrap();

        assert_eq!(
            store.get("ghcr.io").unwrap(),
            Some(RegistryAuth::Basic {
                username: "helper-user".to_owned(),
                password: "helper-token".to_owned(),
            })
        );
    }

    #[test]
    fn docker_config_auth_uses_docker_hub_index_address_for_default_credential_store() {
        let temp = tempfile::tempdir().unwrap();
        let helper_dir = temp.path().join("bin");
        fs::create_dir_all(&helper_dir).unwrap();
        let helper = helper_dir.join("docker-credential-store");
        write_credential_helper(
            &helper,
            r#"#!/bin/sh
set -eu
test "$1" = get
server="$(cat)"
test "$server" = https://index.docker.io/v1/
printf '{"Username":"hub-user","Secret":"hub-token"}'
"#,
        );
        let config = temp.path().join("config.json");
        fs::write(
            &config,
            r#"{
                "credsStore": "store"
            }"#,
        )
        .unwrap();

        let store =
            DockerConfigAuthStore::from_config_file_with_helper_paths(&config, &[helper_dir])
                .unwrap();

        assert_eq!(
            store.get("docker.io").unwrap(),
            Some(RegistryAuth::Basic {
                username: "hub-user".to_owned(),
                password: "hub-token".to_owned(),
            })
        );
    }

    #[test]
    fn docker_config_auth_uses_trailing_slash_docker_hub_helper_server() {
        let temp = tempfile::tempdir().unwrap();
        let helper_dir = temp.path().join("bin");
        fs::create_dir_all(&helper_dir).unwrap();
        let helper = helper_dir.join("docker-credential-hub");
        write_credential_helper(
            &helper,
            r#"#!/bin/sh
set -eu
test "$1" = get
server="$(cat)"
test "$server" = https://index.docker.io/v1/
printf '{"Username":"hub-user","Secret":"hub-token"}'
"#,
        );
        let config = temp.path().join("config.json");
        fs::write(
            &config,
            r#"{
                "credHelpers": {
                    "docker.io": "hub"
                }
            }"#,
        )
        .unwrap();

        let store =
            DockerConfigAuthStore::from_config_file_with_helper_paths(&config, &[helper_dir])
                .unwrap();

        assert_eq!(
            store.get("docker.io").unwrap(),
            Some(RegistryAuth::Basic {
                username: "hub-user".to_owned(),
                password: "hub-token".to_owned(),
            })
        );
    }

    #[test]
    fn docker_config_auth_does_not_fallback_to_inline_auth_when_helper_has_no_credentials() {
        let temp = tempfile::tempdir().unwrap();
        let helper_dir = temp.path().join("bin");
        fs::create_dir_all(&helper_dir).unwrap();
        let helper = helper_dir.join("docker-credential-fake");
        write_credential_helper(
            &helper,
            r#"#!/bin/sh
set -eu
test "$1" = get
server="$(cat)"
test "$server" = ghcr.io
printf 'credentials not found in native keychain'
exit 1
"#,
        );
        let config = temp.path().join("config.json");
        fs::write(
            &config,
            r#"{
                "credHelpers": {
                    "ghcr.io": "fake"
                },
                "auths": {
                    "ghcr.io": {
                        "auth": "aW5saW5lOnRva2Vu"
                    }
                }
            }"#,
        )
        .unwrap();

        let store =
            DockerConfigAuthStore::from_config_file_with_helper_paths(&config, &[helper_dir])
                .unwrap();

        assert_eq!(store.get("ghcr.io").unwrap(), None);
    }

    #[test]
    fn docker_config_auth_uses_registry_helper_instead_of_default_store() {
        let temp = tempfile::tempdir().unwrap();
        let helper_dir = temp.path().join("bin");
        fs::create_dir_all(&helper_dir).unwrap();
        let helper = helper_dir.join("docker-credential-fake");
        write_credential_helper(
            &helper,
            r#"#!/bin/sh
set -eu
test "$1" = get
server="$(cat)"
test "$server" = ghcr.io
printf 'credentials not found in native keychain'
exit 1
"#,
        );
        let store_helper = helper_dir.join("docker-credential-store");
        write_credential_helper(
            &store_helper,
            r#"#!/bin/sh
set -eu
test "$1" = get
server="$(cat)"
test "$server" = ghcr.io
printf '{"Username":"store-user","Secret":"store-token"}'
"#,
        );
        let config = temp.path().join("config.json");
        fs::write(
            &config,
            r#"{
                "credHelpers": {
                    "ghcr.io": "fake"
                },
                "credsStore": "store"
            }"#,
        )
        .unwrap();

        let store =
            DockerConfigAuthStore::from_config_file_with_helper_paths(&config, &[helper_dir])
                .unwrap();

        assert_eq!(store.get("ghcr.io").unwrap(), None);
    }

    #[test]
    fn docker_config_auth_does_not_fallback_to_inline_auth_when_default_store_has_no_credentials() {
        let temp = tempfile::tempdir().unwrap();
        let helper_dir = temp.path().join("bin");
        fs::create_dir_all(&helper_dir).unwrap();
        let helper = helper_dir.join("docker-credential-fake");
        write_credential_helper(
            &helper,
            r#"#!/bin/sh
set -eu
test "$1" = get
server="$(cat)"
test "$server" = ghcr.io
printf 'credentials not found in native keychain'
exit 1
"#,
        );
        let config = temp.path().join("config.json");
        fs::write(
            &config,
            r#"{
                "credsStore": "fake",
                "auths": {
                    "ghcr.io": {
                        "auth": "aW5saW5lOnRva2Vu"
                    }
                }
            }"#,
        )
        .unwrap();

        let store =
            DockerConfigAuthStore::from_config_file_with_helper_paths(&config, &[helper_dir])
                .unwrap();

        assert_eq!(store.get("ghcr.io").unwrap(), None);
    }

    #[test]
    fn docker_config_auth_errors_when_configured_registry_helper_is_missing() {
        let temp = tempfile::tempdir().unwrap();
        let helper_dir = temp.path().join("bin");
        fs::create_dir_all(&helper_dir).unwrap();
        let config = temp.path().join("config.json");
        fs::write(
            &config,
            r#"{
                "credHelpers": {
                    "ghcr.io": "missing"
                },
                "auths": {
                    "ghcr.io": {
                        "auth": "aW5saW5lOnRva2Vu"
                    }
                }
            }"#,
        )
        .unwrap();

        let store =
            DockerConfigAuthStore::from_config_file_with_helper_paths(&config, &[helper_dir])
                .unwrap();
        let error = store.get("ghcr.io").unwrap_err();
        let message = format!("{error:#}");

        assert!(
            message.contains("Docker credential helper was not found"),
            "{error:#}"
        );
    }

    #[test]
    fn docker_config_auth_treats_missing_helper_credentials_as_no_auth() {
        let temp = tempfile::tempdir().unwrap();
        let helper_dir = temp.path().join("bin");
        fs::create_dir_all(&helper_dir).unwrap();
        let helper = helper_dir.join("docker-credential-fake");
        write_credential_helper(
            &helper,
            r#"#!/bin/sh
set -eu
test "$1" = get
server="$(cat)"
test "$server" = ghcr.io
printf 'credentials not found in native keychain'
exit 1
"#,
        );
        let config = temp.path().join("config.json");
        fs::write(
            &config,
            r#"{
                "credsStore": "fake"
            }"#,
        )
        .unwrap();

        let store =
            DockerConfigAuthStore::from_config_file_with_helper_paths(&config, &[helper_dir])
                .unwrap();

        assert_eq!(store.get("ghcr.io").unwrap(), None);
    }

    #[test]
    fn docker_config_auth_uses_default_credential_store_when_registry_helper_is_absent() {
        let temp = tempfile::tempdir().unwrap();
        let helper_dir = temp.path().join("bin");
        fs::create_dir_all(&helper_dir).unwrap();
        let helper = helper_dir.join("docker-credential-store");
        write_credential_helper(
            &helper,
            r#"#!/bin/sh
set -eu
test "$1" = get
server="$(cat)"
test "$server" = ghcr.io
printf '{"Username":"store-user","Secret":"store-token"}'
"#,
        );
        let config = temp.path().join("config.json");
        fs::write(
            &config,
            r#"{
                "credsStore": "store"
            }"#,
        )
        .unwrap();

        let store =
            DockerConfigAuthStore::from_config_file_with_helper_paths(&config, &[helper_dir])
                .unwrap();

        assert_eq!(
            store.get("ghcr.io").unwrap(),
            Some(RegistryAuth::Basic {
                username: "store-user".to_owned(),
                password: "store-token".to_owned(),
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

    fn write_cached_feature_archive_for_manifest(
        cache_root: &Path,
        manifest_digest: &str,
        id: &str,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("feature.tgz");
        let metadata = format!(r#"{{"id":"{id}","version":"1.0.0","name":"{id}"}}"#);
        write_feature_archive(
            &archive,
            &[
                ("install.sh", b"#!/bin/sh\n".as_slice()),
                ("devcontainer-feature.json", metadata.as_bytes()),
            ],
        );
        let blob = fs::read(&archive).unwrap();
        let layer_digest = format!("sha256:{}", hex_lower(&Sha256::digest(&blob)));
        write_cache_archive(
            &feature_cache_archive_path(cache_root, manifest_digest),
            &blob,
            &FeatureCacheMetadata {
                manifest_digest: manifest_digest.to_owned(),
                layer_digest,
            },
        )
        .unwrap();
    }

    fn sha256_digest(hex: &str) -> String {
        format!("sha256:{hex}")
    }

    fn feature_install_input(id: &str, metadata: FeatureMetadata) -> FeatureInstallInput {
        let reference = parse_feature_ref(id).unwrap();
        let canonical_id = reference.canonical_id().to_owned();
        FeatureInstallInput {
            feature: crate::config::resolved::ResolvedFeature {
                id: id.to_owned(),
                canonical_id: canonical_id.clone(),
                options: BTreeMap::new(),
            },
            reference,
            metadata,
            source_key: id.to_owned(),
            instance_key: format!("test\x1e{canonical_id}\x1e{id}"),
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
            self.instance_key = feature_install_input_instance_key(&self);
            self
        }
    }
}
