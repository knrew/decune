use std::collections::BTreeMap;

use crate::workspace::Workspace;

const MANAGED_LABEL: &str = "decune.managed";
const WORKSPACE_LABEL: &str = "decune.workspace";
const WORKSPACE_ID_LABEL: &str = "decune.workspace_id";
const CONFIG_HASH_LABEL: &str = "decune.config_hash";
const VERSION_LABEL: &str = "decune.version";
const DEVCONTAINER_LOCAL_FOLDER_LABEL: &str = "devcontainer.local_folder";
const DEVCONTAINER_CONFIG_FILE_LABEL: &str = "devcontainer.config_file";
const IMAGE_REPOSITORY_PREFIX: &str = "decune/";
const DOCKER_REPOSITORY_NAME_MAX: usize = 255;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DockerResources {
    pub(crate) container_name: String,
    pub(crate) image_tag: String,
    pub(crate) labels: BTreeMap<String, String>,
    pub(crate) config_hash: String,
}

impl DockerResources {
    pub(crate) fn from_workspace(
        workspace: &Workspace,
        config_hash: impl Into<String>,
        config_file: impl Into<String>,
    ) -> Self {
        let resource_basename = docker_name_segment(workspace.basename());
        let config_hash = config_hash.into();
        let workspace_root = workspace.root().display().to_string();
        let config_file = config_file.into();
        let image_repository = docker_image_repository(&resource_basename, workspace.id());
        let labels = labels([
            (MANAGED_LABEL, "true".to_owned()),
            (WORKSPACE_LABEL, workspace_root.clone()),
            (WORKSPACE_ID_LABEL, workspace.id().to_owned()),
            (CONFIG_HASH_LABEL, config_hash.clone()),
            (VERSION_LABEL, env!("CARGO_PKG_VERSION").to_owned()),
            (DEVCONTAINER_LOCAL_FOLDER_LABEL, workspace_root),
            (DEVCONTAINER_CONFIG_FILE_LABEL, config_file),
        ]);

        Self {
            container_name: format!("decune-{resource_basename}-{}", workspace.id()),
            image_tag: format!("{image_repository}:{config_hash}"),
            labels,
            config_hash,
        }
    }

    pub(crate) fn image_repository_for_workspace(workspace: &Workspace) -> String {
        let resource_basename = docker_name_segment(workspace.basename());

        docker_image_repository(&resource_basename, workspace.id())
    }
}

pub(crate) fn managed_workspace_label_filters(workspace_id: &str) -> BTreeMap<String, Vec<String>> {
    let mut filters = BTreeMap::new();
    filters.insert(
        "label".to_owned(),
        vec![
            format!("{MANAGED_LABEL}=true"),
            format!("{WORKSPACE_ID_LABEL}={workspace_id}"),
        ],
    );
    filters
}

#[allow(dead_code)]
pub(crate) fn is_managed_workspace_container(
    labels: &BTreeMap<String, String>,
    workspace_id: &str,
) -> bool {
    labels
        .get(MANAGED_LABEL)
        .is_some_and(|value| value == "true")
        && labels
            .get(WORKSPACE_ID_LABEL)
            .is_some_and(|value| value == workspace_id)
}

#[allow(dead_code)]
pub(crate) fn has_config_hash(labels: &BTreeMap<String, String>, config_hash: &str) -> bool {
    labels
        .get(CONFIG_HASH_LABEL)
        .is_some_and(|value| value == config_hash)
}

fn labels(entries: impl IntoIterator<Item = (&'static str, String)>) -> BTreeMap<String, String> {
    entries
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

fn docker_image_repository(resource_basename: &str, workspace_id: &str) -> String {
    let max_basename_len =
        DOCKER_REPOSITORY_NAME_MAX - IMAGE_REPOSITORY_PREFIX.len() - 1 - workspace_id.len();
    let basename = truncate_docker_name_segment(resource_basename, max_basename_len);

    format!("{IMAGE_REPOSITORY_PREFIX}{basename}-{workspace_id}")
}

fn truncate_docker_name_segment(value: &str, max_len: usize) -> String {
    let mut output = value.to_owned();
    output.truncate(max_len);

    while output.ends_with('-') {
        output.pop();
    }

    if output.is_empty() {
        "workspace".to_owned()
    } else {
        output
    }
}

fn docker_name_segment(value: &str) -> String {
    let mut output = String::new();
    let mut previous_was_separator = true;

    for character in value.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            output.push(character);
            previous_was_separator = false;
        } else if !previous_was_separator {
            output.push('-');
            previous_was_separator = true;
        }
    }

    while output.ends_with('-') {
        output.pop();
    }

    if output.is_empty() {
        "workspace".to_owned()
    } else {
        output
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use crate::workspace::Workspace;

    use super::{
        DockerResources, has_config_hash, is_managed_workspace_container,
        managed_workspace_label_filters,
    };

    static NEXT_FIXTURE_ID: AtomicUsize = AtomicUsize::new(0);

    fn fixture_root(name: &str) -> PathBuf {
        let fixture_id = NEXT_FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        let parent = std::env::temp_dir()
            .join("decune-docker-resource-tests")
            .join(std::process::id().to_string())
            .join(fixture_id.to_string());
        let root = parent.join(name);
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn resources_include_names_and_labels_from_workspace_and_config_hash() {
        let root = fixture_root("project");
        let workspace = Workspace::resolve(&root).unwrap();
        let config_file = root.join(".devcontainer/devcontainer.json");

        let resources = DockerResources::from_workspace(
            &workspace,
            "abc123def456",
            config_file.display().to_string(),
        );

        assert_eq!(
            resources.container_name,
            format!("decune-{}-{}", workspace.basename(), workspace.id())
        );
        assert_eq!(
            resources.image_tag,
            format!(
                "decune/{}-{}:abc123def456",
                workspace.basename(),
                workspace.id()
            )
        );
        assert_eq!(resources.config_hash, "abc123def456");
        assert_eq!(resources.labels["decune.managed"], "true");
        assert_eq!(
            resources.labels["decune.workspace"],
            workspace.root().display().to_string()
        );
        assert_eq!(resources.labels["decune.workspace_id"], workspace.id());
        assert_eq!(resources.labels["decune.config_hash"], "abc123def456");
        assert_eq!(
            resources.labels["decune.version"],
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(
            resources.labels["devcontainer.local_folder"],
            workspace.root().display().to_string()
        );
        assert_eq!(
            resources.labels["devcontainer.config_file"],
            config_file.display().to_string()
        );
    }

    #[test]
    fn resource_names_sanitize_workspace_basename_for_docker() {
        let root = fixture_root("Project Name!");
        let workspace = Workspace::resolve(&root).unwrap();

        let resources = DockerResources::from_workspace(
            &workspace,
            "abc123",
            "/workspace/.devcontainer/devcontainer.json",
        );

        assert_eq!(
            resources.container_name,
            format!("decune-project-name-{}", workspace.id())
        );
        assert_eq!(
            resources.image_tag,
            format!("decune/project-name-{}:abc123", workspace.id())
        );
    }

    #[test]
    fn container_name_keeps_long_sanitized_basename_without_docker_length_truncation() {
        let basename = "a".repeat(249);
        let root = fixture_root(&basename);
        let workspace = Workspace::resolve(&root).unwrap();

        let resources = DockerResources::from_workspace(
            &workspace,
            "abc123",
            "/workspace/.devcontainer/devcontainer.json",
        );

        assert_eq!(
            resources.container_name,
            format!("decune-{basename}-{}", workspace.id())
        );
    }

    #[test]
    fn fixture_roots_do_not_delete_sibling_workspaces() {
        let first = fixture_root("first");
        let second = fixture_root("second");

        assert!(first.is_dir());
        assert!(second.is_dir());
    }

    #[test]
    fn image_repository_is_truncated_to_docker_name_limit() {
        let root = fixture_root(&"a".repeat(249));
        let workspace = Workspace::resolve(&root).unwrap();

        let resources = DockerResources::from_workspace(
            &workspace,
            "abc123",
            "/workspace/.devcontainer/devcontainer.json",
        );
        let repository = resources.image_tag.split(':').next().unwrap();

        assert!(repository.len() <= 255, "{repository}");
        assert!(repository.starts_with("decune/"));
        assert!(repository.ends_with(workspace.id()));
    }

    #[test]
    fn managed_workspace_filters_only_use_decune_ownership_labels() {
        let filters = managed_workspace_label_filters("abc123def456");

        assert_eq!(
            filters.get("label"),
            Some(&vec![
                "decune.managed=true".to_owned(),
                "decune.workspace_id=abc123def456".to_owned(),
            ])
        );
    }

    #[test]
    fn managed_workspace_container_predicate_rejects_other_tools_and_workspaces() {
        let managed_labels = labels([
            ("decune.managed", "true"),
            ("decune.workspace_id", "abc123def456"),
            ("com.example.owner", "other"),
        ]);

        assert!(is_managed_workspace_container(
            &managed_labels,
            "abc123def456"
        ));

        let unmanaged = labels([("decune.workspace_id", "abc123def456")]);
        assert!(!is_managed_workspace_container(&unmanaged, "abc123def456"));

        let different_workspace = labels([
            ("decune.managed", "true"),
            ("decune.workspace_id", "different"),
        ]);
        assert!(!is_managed_workspace_container(
            &different_workspace,
            "abc123def456"
        ));
    }

    #[test]
    fn config_hash_predicate_is_separate_from_workspace_ownership() {
        let managed_labels = labels([
            ("decune.managed", "true"),
            ("decune.workspace_id", "abc123def456"),
            ("decune.config_hash", "config-a"),
        ]);

        assert!(is_managed_workspace_container(
            &managed_labels,
            "abc123def456"
        ));
        assert!(has_config_hash(&managed_labels, "config-a"));
        assert!(!has_config_hash(&managed_labels, "config-b"));
        assert!(!has_config_hash(
            &labels([("decune.managed", "true")]),
            "config-a"
        ));
    }

    fn labels(
        entries: impl IntoIterator<Item = (&'static str, &'static str)>,
    ) -> BTreeMap<String, String> {
        entries
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect()
    }
}
