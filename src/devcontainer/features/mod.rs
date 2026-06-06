#![allow(dead_code)]

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};

use crate::config::{FeatureLockHashEntry, layer::ConfigLayer, resolved::ResolvedFeature};

mod archive;
mod auth;
mod cache;
mod local;
mod lock;
mod metadata;
mod reference;
mod registry;

#[allow(unused_imports)]
pub(crate) use archive::extract_feature_archive;
#[allow(unused_imports)]
pub(crate) use auth::{DockerConfigAuth, RegistryAuth};
#[cfg(test)]
use cache::{FeatureCacheMetadata, feature_cache_archive_path, write_cache_archive};
#[allow(unused_imports)]
pub(crate) use cache::{OciFeatureArtifact, pull_oci_feature_with_client};
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
pub(crate) use reference::{
    FeatureRef, LocalFeatureRef, OciFeatureRef, parse_feature_ref,
    parse_feature_ref_from_devcontainer_dir,
};
use reference::{split_digest, split_tag};
#[allow(unused_imports)]
pub(crate) use registry::{
    HttpOciRegistryClient, OciLayerDescriptor, OciManifestResponse, OciRegistryClient,
};
#[cfg(test)]
use sha2::{Digest, Sha256};

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

fn hex_lower(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs, io::Write, path::Path};

    use flate2::{Compression, write::GzEncoder};
    use tar::{Builder, Header};

    use super::*;

    const MANIFEST_DIGEST_HEX: &str =
        "1111111111111111111111111111111111111111111111111111111111111111";

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
