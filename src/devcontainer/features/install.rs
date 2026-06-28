use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};

use crate::config::{FeatureLockHashEntry, layer::ConfigLayer, resolved::ResolvedFeature};

use super::local::{
    ensure_feature_files, local_feature_content_digest, validate_local_feature_directory_name,
};
use super::options::feature_options_sort_key;
use super::reference::{split_digest, split_tag};
use super::{
    FEATURE_LOCK_VERSION, FeatureLockEntry, FeatureLockFile, FeatureMetadata, FeatureRef,
    HttpOciRegistryClient, LocalFeatureRef, OciFeatureRef, feature_option_env, parse_feature_ref,
    parse_feature_ref_from_devcontainer_dir, pull_oci_feature_with_client, read_feature_lock_file,
    read_feature_metadata_document, remove_feature_lock_file, resolve_locked_feature_ref,
    write_feature_lock_file,
};

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
    for entry in entries {
        let source = resolver.sources.get(&entry.source_key).ok_or_else(|| {
            anyhow!(
                "Feature source was not prepared for {}",
                entry.feature.canonical_id
            )
        })?;
        prepared_entries.push(PreparedFeatureInstallEntry {
            feature: entry.feature,
            source_dir: source.source_dir.clone(),
            option_env: entry.option_env,
            container_env: source.container_env.clone(),
        });
        metadata_layers.push(feature_runtime_metadata_layer(&source.layer));
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
    let mut graph = FeatureInstallGraph::new(inputs);
    graph.resolve_dependency_edges(&mut resolve_dependency)?;
    let dependencies = graph.dependency_requirements()?;
    let priorities =
        override_feature_install_priorities(&graph.nodes, override_feature_install_order)?;
    order_feature_install_plan(&graph.nodes, &dependencies, &priorities)
}

struct FeatureInstallGraph {
    nodes: BTreeMap<String, FeatureInstallInput>,
    dependency_edges: BTreeMap<String, BTreeSet<String>>,
}

struct FeatureDependencyScan {
    parent_instance_key: String,
    parent_canonical_id: String,
    parent_reference: FeatureRef,
    dependencies: Vec<PendingFeatureDependency>,
}

struct PendingFeatureDependency {
    dependency: String,
    options: serde_json::Value,
}

impl FeatureInstallGraph {
    fn new(inputs: Vec<FeatureInstallInput>) -> Self {
        let mut nodes = BTreeMap::new();
        for input in inputs {
            nodes.entry(input.instance_key.clone()).or_insert(input);
        }
        Self {
            nodes,
            dependency_edges: BTreeMap::new(),
        }
    }

    fn resolve_dependency_edges<F>(&mut self, resolve_dependency: &mut F) -> Result<()>
    where
        F: FnMut(&FeatureDependencyRequest<'_>) -> Result<FeatureInstallInput>,
    {
        let mut scan_queue = self.nodes.keys().cloned().collect::<VecDeque<_>>();
        while let Some(instance_key) = scan_queue.pop_front() {
            let mut scan = self.dependency_scan(&instance_key)?;
            let dependencies = std::mem::take(&mut scan.dependencies);
            for dependency in dependencies {
                self.resolve_feature_dependency(
                    &scan,
                    &dependency,
                    &mut scan_queue,
                    resolve_dependency,
                )?;
            }
        }
        Ok(())
    }

    fn dependency_scan(&self, instance_key: &str) -> Result<FeatureDependencyScan> {
        let Some(input) = self.nodes.get(instance_key) else {
            bail!("Feature install graph is missing queued Feature: {instance_key}");
        };
        let dependencies = input
            .metadata
            .depends_on
            .iter()
            .map(|(dependency, options)| PendingFeatureDependency {
                dependency: dependency.clone(),
                options: options.clone(),
            })
            .collect();
        Ok(FeatureDependencyScan {
            parent_instance_key: instance_key.to_owned(),
            parent_canonical_id: input.feature.canonical_id.clone(),
            parent_reference: input.reference.clone(),
            dependencies,
        })
    }

    fn resolve_feature_dependency<F>(
        &mut self,
        scan: &FeatureDependencyScan,
        dependency: &PendingFeatureDependency,
        scan_queue: &mut VecDeque<String>,
        resolve_dependency: &mut F,
    ) -> Result<()>
    where
        F: FnMut(&FeatureDependencyRequest<'_>) -> Result<FeatureInstallInput>,
    {
        let dependency_target =
            dependency_feature_target(&scan.parent_reference, &dependency.dependency)?;
        let options = feature_dependency_options(
            &scan.parent_canonical_id,
            &dependency.dependency,
            &dependency.options,
        )?;
        if let Some(existing_instance_key) = find_existing_feature_instance(
            &self.nodes,
            &dependency_target.canonical_id,
            &dependency_target.reference,
            &options,
        ) {
            self.add_dependency_edge(&scan.parent_instance_key, existing_instance_key);
            return Ok(());
        }

        let request = FeatureDependencyRequest {
            parent_canonical_id: &scan.parent_canonical_id,
            dependency: &dependency.dependency,
            reference: dependency_target.reference,
            canonical_id: dependency_target.canonical_id,
            options,
        };
        let dependency_input = resolved_dependency_input(resolve_dependency, &request)?;
        let dependency_instance_key = dependency_input.instance_key.clone();
        self.add_dependency_edge(&scan.parent_instance_key, dependency_instance_key.clone());
        if self
            .nodes
            .insert(dependency_instance_key.clone(), dependency_input)
            .is_none()
        {
            scan_queue.push_back(dependency_instance_key);
        }
        Ok(())
    }

    fn add_dependency_edge(&mut self, parent_instance_key: &str, dependency_instance_key: String) {
        self.dependency_edges
            .entry(parent_instance_key.to_owned())
            .or_default()
            .insert(dependency_instance_key);
    }

    fn dependency_requirements(&self) -> Result<BTreeMap<String, BTreeSet<String>>> {
        let mut dependencies = BTreeMap::new();
        for (instance_key, input) in &self.nodes {
            let mut required = self
                .dependency_edges
                .get(instance_key)
                .cloned()
                .unwrap_or_default();
            add_soft_order_dependencies(&self.nodes, input, &mut required)?;
            dependencies.insert(instance_key.clone(), required);
        }
        Ok(dependencies)
    }
}

fn resolved_dependency_input<F>(
    resolve_dependency: &mut F,
    request: &FeatureDependencyRequest<'_>,
) -> Result<FeatureInstallInput>
where
    F: FnMut(&FeatureDependencyRequest<'_>) -> Result<FeatureInstallInput>,
{
    let mut dependency_input = resolve_dependency(request).with_context(|| {
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
        dependency_input.instance_key = feature_install_input_instance_key(&dependency_input);
    }
    Ok(dependency_input)
}

fn add_soft_order_dependencies(
    nodes: &BTreeMap<String, FeatureInstallInput>,
    input: &FeatureInstallInput,
    required: &mut BTreeSet<String>,
) -> Result<()> {
    for dependency in &input.metadata.installs_after {
        let dependency_target =
            soft_order_feature_target(&input.reference, dependency, "installsAfter")?;
        for (candidate_key, candidate) in nodes {
            if feature_matches_order_target(candidate, &dependency_target.canonical_id)? {
                required.insert(candidate_key.clone());
            }
        }
    }
    Ok(())
}

fn order_feature_install_plan(
    nodes: &BTreeMap<String, FeatureInstallInput>,
    dependencies: &BTreeMap<String, BTreeSet<String>>,
    priorities: &BTreeMap<String, usize>,
) -> Result<Vec<FeatureInstallPlanEntry>> {
    let mut worklist = nodes.keys().cloned().collect::<BTreeSet<_>>();
    let mut installed = BTreeSet::new();
    let mut ordered = Vec::new();
    while !worklist.is_empty() {
        let round =
            next_feature_install_round(nodes, dependencies, priorities, &worklist, &installed)?;
        for canonical_id in round {
            worklist.remove(&canonical_id);
            installed.insert(canonical_id.clone());
            let Some(input) = nodes.get(&canonical_id) else {
                bail!("Feature install graph is missing ready Feature: {canonical_id}");
            };
            ordered.push(feature_install_plan_entry(input)?);
        }
    }
    Ok(ordered)
}

fn next_feature_install_round(
    nodes: &BTreeMap<String, FeatureInstallInput>,
    dependencies: &BTreeMap<String, BTreeSet<String>>,
    priorities: &BTreeMap<String, usize>,
    worklist: &BTreeSet<String>,
    installed: &BTreeSet<String>,
) -> Result<Vec<String>> {
    let ready = ready_feature_instances(dependencies, worklist, installed);
    if ready.is_empty() {
        let blocked = worklist.iter().cloned().collect::<Vec<_>>().join(", ");
        bail!("Feature install order contains a dependency cycle involving: {blocked}");
    }
    let max_priority = max_feature_round_priority(nodes, priorities, &ready)?;
    let mut round = ready
        .into_iter()
        .filter(|instance_key| {
            nodes.get(instance_key).is_some_and(|input| {
                feature_round_priority(&input.instance_key, priorities) == max_priority
            })
        })
        .collect::<Vec<_>>();
    round.sort_by(|left, right| stable_feature_order(nodes.get(left), nodes.get(right)));
    Ok(round)
}

fn ready_feature_instances(
    dependencies: &BTreeMap<String, BTreeSet<String>>,
    worklist: &BTreeSet<String>,
    installed: &BTreeSet<String>,
) -> Vec<String> {
    worklist
        .iter()
        .filter(|canonical_id| {
            dependencies
                .get(*canonical_id)
                .is_none_or(|required| required.is_subset(installed))
        })
        .cloned()
        .collect()
}

fn max_feature_round_priority(
    nodes: &BTreeMap<String, FeatureInstallInput>,
    priorities: &BTreeMap<String, usize>,
    ready: &[String],
) -> Result<usize> {
    let mut max_priority = 0;
    for instance_key in ready {
        let Some(input) = nodes.get(instance_key) else {
            bail!("Feature install graph is missing ready Feature: {instance_key}");
        };
        max_priority = max_priority.max(feature_round_priority(&input.instance_key, priorities));
    }
    Ok(max_priority)
}

fn feature_install_plan_entry(input: &FeatureInstallInput) -> Result<FeatureInstallPlanEntry> {
    Ok(FeatureInstallPlanEntry {
        feature: input.feature.clone(),
        metadata: input.metadata.clone(),
        source_key: input.source_key.clone(),
        instance_key: input.instance_key.clone(),
        option_env: feature_option_env(&input.feature, &input.metadata)?,
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
            let instance_key = feature_instance_key(&feature, &reference, source, local_instance);
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
        let instance_key = feature_instance_key(&feature, &reference, &source, local_instance);
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
                "Failed to resolve local Feature directory for `{}`: {}",
                reference.original,
                reference.path.display()
            )
        })?;
        if !source_dir.starts_with(&devcontainer_dir) {
            bail!(
                "Invalid local Feature `{}`: canonical path must stay inside devcontainer directory {} but resolved to {}",
                reference.original,
                devcontainer_dir.display(),
                source_dir.display()
            );
        }
        ensure_feature_files(&source_dir, &reference.original)?;
        let document =
            read_feature_metadata_document(&source_dir.join("devcontainer-feature.json"))
                .with_context(|| {
                    format!("Failed to read local Feature `{}`", reference.original)
                })?;
        validate_local_feature_directory_name(
            &source_dir,
            &document.metadata.id,
            &reference.original,
        )?;
        let digest = local_feature_content_digest(&source_dir)?;
        let container_env = feature_layer_container_env(&document.layer);

        Ok(FeatureSource {
            source_dir,
            metadata: document.metadata,
            layer: document.layer,
            container_env,
            digest,
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

fn feature_runtime_metadata_layer(layer: &ConfigLayer) -> ConfigLayer {
    let mut layer = layer.clone();
    if let Some(devcontainer) = &mut layer.devcontainer {
        devcontainer.container_env.clear();
    }
    layer
}

fn feature_source_key(reference: &FeatureRef) -> String {
    match reference {
        FeatureRef::Oci(reference) => reference.normalized_reference(),
        FeatureRef::Local(reference) => reference.canonical_id.clone(),
    }
}

fn feature_instance_key(
    feature: &ResolvedFeature,
    reference: &FeatureRef,
    source: &FeatureSource,
    local_instance: Option<usize>,
) -> String {
    let options = feature_options_sort_key(&feature.options)
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("\x1f");
    local_instance.map_or_else(
        || {
            format!(
                "oci\x1e{}\x1e{}\x1e{options}",
                feature.canonical_id, source.digest
            )
        },
        |instance| {
            let canonical_id = match reference {
                FeatureRef::Local(reference) => &reference.canonical_id,
                FeatureRef::Oci(_) => &feature.canonical_id,
            };
            format!(
                "local\x1e{}\x1e{}\x1e{options}\x1e{instance}",
                canonical_id, source.digest
            )
        },
    )
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
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::Array(_) => {
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
        serde_json::Value::Null
        | serde_json::Value::Number(_)
        | serde_json::Value::Array(_)
        | serde_json::Value::Object(_) => {
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
            part.parse::<u64>().map_or_else(
                |_| FeatureTagPart::Text(part.to_ascii_lowercase()),
                FeatureTagPart::Number,
            )
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

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs, io::Write, os::unix::fs::PermissionsExt, path::Path};

    use flate2::{Compression, write::GzEncoder};
    use sha2::{Digest, Sha256};
    use tar::{Builder, Header};

    use super::*;
    use crate::devcontainer::features::cache::{
        FeatureCacheMetadata, feature_cache_archive_path, write_cache_archive,
    };
    use crate::hex::hex_lower;

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
        write_local_feature_install_script(&feature_dir);
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
            error.to_string().contains("local Feature directory name"),
            "{error:#}"
        );
        assert!(error.to_string().contains("tool"), "{error:#}");
        assert!(error.to_string().contains("different-tool"), "{error:#}");
    }

    #[test]
    fn local_feature_missing_install_script_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_root = temp.path().join("workspace");
        let devcontainer_dir = workspace_root.join(".devcontainer");
        let feature_dir = devcontainer_dir.join("features/tool");
        let cache_root = temp.path().join("cache");
        fs::create_dir_all(&feature_dir).unwrap();
        fs::write(devcontainer_dir.join("devcontainer.json"), "{}").unwrap();
        fs::write(
            feature_dir.join("devcontainer-feature.json"),
            r#"{"id":"tool","version":"1.0.0","name":"Tool"}"#,
        )
        .unwrap();
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

        assert!(error.to_string().contains("./features/tool"), "{error:#}");
        assert!(error.to_string().contains("install.sh"), "{error:#}");
    }

    #[test]
    fn local_feature_non_executable_install_script_is_accepted() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_root = temp.path().join("workspace");
        let devcontainer_dir = workspace_root.join(".devcontainer");
        let feature_dir = devcontainer_dir.join("features/tool");
        let cache_root = temp.path().join("cache");
        fs::create_dir_all(&feature_dir).unwrap();
        fs::write(devcontainer_dir.join("devcontainer.json"), "{}").unwrap();
        fs::write(
            feature_dir.join("devcontainer-feature.json"),
            r#"{"id":"tool","version":"1.0.0","name":"Tool"}"#,
        )
        .unwrap();
        fs::write(feature_dir.join("install.sh"), "#!/bin/sh\n").unwrap();
        let features = vec![ResolvedFeature {
            id: "./features/tool".to_owned(),
            canonical_id: "local:features/tool".to_owned(),
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

        assert_eq!(plan.entries.len(), 1);
        assert_eq!(
            plan.entries[0].source_dir,
            feature_dir.canonicalize().unwrap()
        );
    }

    #[test]
    fn local_feature_missing_metadata_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_root = temp.path().join("workspace");
        let devcontainer_dir = workspace_root.join(".devcontainer");
        let feature_dir = devcontainer_dir.join("features/tool");
        let cache_root = temp.path().join("cache");
        fs::create_dir_all(&feature_dir).unwrap();
        fs::write(devcontainer_dir.join("devcontainer.json"), "{}").unwrap();
        write_local_feature_install_script(&feature_dir);
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

        assert!(error.to_string().contains("./features/tool"), "{error:#}");
        assert!(
            error.to_string().contains("devcontainer-feature.json"),
            "{error:#}"
        );
    }

    #[test]
    fn local_feature_symlink_escape_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_root = temp.path().join("workspace");
        let devcontainer_dir = workspace_root.join(".devcontainer");
        let feature_parent = devcontainer_dir.join("features");
        let outside_dir = workspace_root.join("outside-tool");
        let cache_root = temp.path().join("cache");
        fs::create_dir_all(&feature_parent).unwrap();
        fs::create_dir_all(&outside_dir).unwrap();
        fs::write(devcontainer_dir.join("devcontainer.json"), "{}").unwrap();
        fs::write(
            outside_dir.join("devcontainer-feature.json"),
            r#"{"id":"tool","version":"1.0.0","name":"Tool"}"#,
        )
        .unwrap();
        write_local_feature_install_script(&outside_dir);
        std::os::unix::fs::symlink(&outside_dir, feature_parent.join("tool")).unwrap();
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

        assert!(error.to_string().contains("./features/tool"), "{error:#}");
        assert!(error.to_string().contains("outside"), "{error:#}");
    }

    #[test]
    fn valid_local_feature_validation_succeeds() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_root = temp.path().join("workspace");
        let devcontainer_dir = workspace_root.join(".devcontainer");
        let feature_dir = devcontainer_dir.join("features/tool");
        let cache_root = temp.path().join("cache");
        fs::create_dir_all(&feature_dir).unwrap();
        fs::write(devcontainer_dir.join("devcontainer.json"), "{}").unwrap();
        fs::write(
            feature_dir.join("devcontainer-feature.json"),
            r#"{"id":"tool","version":"1.0.0","name":"Tool"}"#,
        )
        .unwrap();
        write_local_feature_install_script(&feature_dir);
        let features = vec![ResolvedFeature {
            id: "./features/./tool".to_owned(),
            canonical_id: "./features/./tool".to_owned(),
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

        assert_eq!(plan.entries.len(), 1);
        assert_eq!(
            plan.entries[0].source_dir,
            feature_dir.canonicalize().unwrap()
        );
        assert_eq!(plan.lock_entries.len(), 1);
        assert!(
            plan.lock_entries[0]
                .feature_id
                .contains("local:features/tool")
        );
        assert!(
            !plan.lock_entries[0]
                .feature_id
                .contains("./features/./tool")
        );
    }

    #[test]
    fn feature_install_plan_keeps_container_env_for_build_but_not_runtime_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_root = temp.path().join("workspace");
        let devcontainer_dir = workspace_root.join(".devcontainer");
        let feature_dir = devcontainer_dir.join("features/tool");
        let cache_root = temp.path().join("cache");
        fs::create_dir_all(&feature_dir).unwrap();
        fs::write(devcontainer_dir.join("devcontainer.json"), "{}").unwrap();
        fs::write(
            feature_dir.join("devcontainer-feature.json"),
            r#"{
              "id": "tool",
              "version": "1.0.0",
              "name": "Tool",
              "containerEnv": {
                "PATH": "/opt/tool/bin:${PATH}",
                "FROM_FEATURE": "yes"
              },
              "postStartCommand": "test -n \"$FROM_FEATURE\""
            }"#,
        )
        .unwrap();
        write_local_feature_install_script(&feature_dir);
        let features = vec![ResolvedFeature {
            id: "./features/tool".to_owned(),
            canonical_id: "local:features/tool".to_owned(),
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
            plan.entries[0]
                .container_env
                .get("PATH")
                .map(String::as_str),
            Some("/opt/tool/bin:${PATH}")
        );
        let metadata = plan.metadata_layers[0]
            .devcontainer
            .as_ref()
            .expect("Feature metadata layer should contain devcontainer metadata");
        assert!(metadata.container_env.is_empty());
        assert!(metadata.lifecycle.is_some());
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
        mark_executable(&feature_dir.join("install.sh"));
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
        write_local_feature_install_script(&tool_dir);
        fs::write(
            base_dir.join("devcontainer-feature.json"),
            r#"{"id":"base","version":"1.0.0","name":"Base"}"#,
        )
        .unwrap();
        write_local_feature_install_script(&base_dir);
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
        write_local_feature_install_script(&feature_dir);
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

        let message = format!("{error:#}");
        assert!(message.contains("./../outside-feature"), "{error:#}");
        assert!(message.contains(".. traversal"), "{error:#}");
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
        write_local_feature_install_script(&local_feature_dir);
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
                    request.dependency,
                    FeatureMetadata {
                        depends_on: BTreeMap::from([(
                            "ghcr.io/example/features/common:1".to_owned(),
                            serde_json::json!("3"),
                        )]),
                        ..FeatureMetadata::default()
                    },
                )),
                "ghcr.io/example/features/common" => Ok(feature_install_input(
                    request.dependency,
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
    fn feature_install_order_allows_pinned_installs_after_when_dependency_is_missing() {
        let digest = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
        let plan = resolve_feature_install_order(
            vec![feature_install_input(
                "ghcr.io/example/features/tool:1",
                FeatureMetadata {
                    installs_after: vec![
                        "base:1".to_owned(),
                        format!("ghcr.io/example/features/common@{digest}"),
                    ],
                    ..FeatureMetadata::default()
                },
            )],
            &[],
            missing_feature_dependency,
        )
        .unwrap();

        assert_eq!(
            plan.iter()
                .map(|entry| entry.feature.id.as_str())
                .collect::<Vec<_>>(),
            vec!["ghcr.io/example/features/tool:1"]
        );
    }

    #[test]
    fn feature_install_order_matches_pinned_installs_after_by_canonical_id() {
        let digest = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
        let plan = resolve_feature_install_order(
            vec![
                feature_install_input(
                    "ghcr.io/example/features/tool:1",
                    FeatureMetadata::default(),
                ),
                feature_install_input(
                    &format!("ghcr.io/example/features/common@{digest}"),
                    FeatureMetadata::default(),
                ),
                feature_install_input(
                    "ghcr.io/example/features/lint:1",
                    FeatureMetadata {
                        installs_after: vec![
                            "tool:2".to_owned(),
                            format!("ghcr.io/example/features/common@{digest}"),
                        ],
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
                .map(|entry| entry.feature.canonical_id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "ghcr.io/example/features/common",
                "ghcr.io/example/features/tool",
                "ghcr.io/example/features/lint",
            ]
        );
    }

    #[test]
    fn feature_install_order_rejects_versioned_override_order_ids() {
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

    fn write_local_feature_install_script(feature_dir: &Path) {
        let script = feature_dir.join("install.sh");
        fs::write(&script, "#!/bin/sh\n").unwrap();
        mark_executable(&script);
    }

    fn mark_executable(path: &Path) {
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(permissions.mode() | 0o111);
        fs::set_permissions(path, permissions).unwrap();
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
