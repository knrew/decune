use std::collections::BTreeMap;

use crate::workspace::{Workspace, is_valid_workspace_id};

const MANAGED_LABEL: &str = "decune.managed";
const WORKSPACE_LABEL: &str = "decune.workspace";
const WORKSPACE_ID_LABEL: &str = "decune.workspace_id";
const CONFIG_HASH_LABEL: &str = "decune.config_hash";
const VERSION_LABEL: &str = "decune.version";
const DEVCONTAINER_LOCAL_FOLDER_LABEL: &str = "devcontainer.local_folder";
const DEVCONTAINER_CONFIG_FILE_LABEL: &str = "devcontainer.config_file";
pub(crate) const COMPOSE_PROJECT_LABEL: &str = "com.docker.compose.project";
pub(crate) const COMPOSE_SERVICE_LABEL: &str = "com.docker.compose.service";
const IMAGE_REPOSITORY_PREFIX: &str = "decune/";
const DOCKER_REPOSITORY_NAME_MAX: usize = 255;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DockerResources {
    pub(crate) container_name: String,
    pub(crate) image_tag: String,
    pub(crate) workspace_volume_name: String,
    pub(crate) labels: BTreeMap<String, String>,
    pub(crate) config_hash: String,
}

impl DockerResources {
    pub(crate) fn from_workspace(
        workspace: &Workspace,
        config_hash: impl Into<String>,
        config_file: impl Into<String>,
    ) -> Self {
        let safe_workspace_slug = workspace.safe_slug();
        let config_hash = config_hash.into();
        let workspace_root = workspace.root().display().to_string();
        let config_file = config_file.into();
        let image_repository = docker_image_repository(safe_workspace_slug, workspace.id());
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
            container_name: format!("decune-{safe_workspace_slug}-{}", workspace.id()),
            image_tag: format!("{image_repository}:{config_hash}"),
            workspace_volume_name: workspace_volume_name(safe_workspace_slug, workspace.id()),
            labels,
            config_hash,
        }
    }

    pub(crate) fn image_repository_for_workspace(workspace: &Workspace) -> String {
        docker_image_repository(workspace.safe_slug(), workspace.id())
    }

    pub(crate) fn image_repository_for_slug_and_id(
        safe_workspace_slug: &str,
        workspace_id: &str,
    ) -> String {
        docker_image_repository(safe_workspace_slug, workspace_id)
    }
}

#[cfg(test)]
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

pub(crate) fn managed_workspace_id_from_labels(
    labels: &BTreeMap<String, String>,
) -> Option<String> {
    let managed = labels.get(MANAGED_LABEL)?;
    if managed != "true" {
        return None;
    }
    labels
        .get(WORKSPACE_ID_LABEL)
        .filter(|workspace_id| is_valid_workspace_id(workspace_id))
        .cloned()
}

pub(crate) fn managed_workspace_id_from_container(
    container: &crate::docker::container::ContainerInspect,
) -> Option<(String, &BTreeMap<String, String>)> {
    let labels = container.config.as_ref()?.labels.as_ref()?;
    let workspace_id = managed_workspace_id_from_labels(labels)?;
    Some((workspace_id, labels))
}

pub(crate) fn workspace_path_from_labels(labels: &BTreeMap<String, String>) -> Option<String> {
    labels
        .get(WORKSPACE_LABEL)
        .or_else(|| labels.get(DEVCONTAINER_LOCAL_FOLDER_LABEL))
        .filter(|workspace_path| !workspace_path.trim().is_empty())
        .cloned()
}

pub(crate) fn config_hash_from_labels(labels: &BTreeMap<String, String>) -> Option<String> {
    labels
        .get(CONFIG_HASH_LABEL)
        .filter(|config_hash| !config_hash.trim().is_empty())
        .cloned()
}

pub(crate) fn compose_project_name_from_labels(
    labels: &BTreeMap<String, String>,
) -> Option<String> {
    labels
        .get(COMPOSE_PROJECT_LABEL)
        .and_then(|project_name| non_empty_trimmed(project_name))
        .map(str::to_owned)
}

pub(crate) fn non_empty_trimmed(value: &str) -> Option<&str> {
    let value = value.trim();
    if value.is_empty() { None } else { Some(value) }
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

fn workspace_volume_name(safe_workspace_slug: &str, workspace_id: &str) -> String {
    format!("decune-{safe_workspace_slug}-{workspace_id}-workspace")
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

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use crate::workspace::Workspace;

    use super::{DockerResources, managed_workspace_label_filters};

    static NEXT_FIXTURE_ID: AtomicUsize = AtomicUsize::new(0);

    fn fixture_root(name: &str) -> PathBuf {
        let fixture_id = NEXT_FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        let parent = std::env::temp_dir()
            .join("decune-docker-resource-tests")
            .join(std::process::id().to_string())
            .join(fixture_id.to_string());
        let root = parent.join(name);
        _ = fs::remove_dir_all(&root);
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

        assert_eq!(workspace.safe_slug(), "project");
        assert_eq!(
            resources.container_name,
            format!("decune-project-{}", workspace.id())
        );
        assert_eq!(
            resources.image_tag,
            format!("decune/project-{}:abc123def456", workspace.id())
        );
        assert_eq!(
            resources.workspace_volume_name,
            format!("decune-project-{}-workspace", workspace.id())
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
    fn resource_names_do_not_keep_invalid_image_repository_separator_runs() {
        let root = fixture_root("APP__Name...v2");
        let workspace = Workspace::resolve(&root).unwrap();

        let resources = DockerResources::from_workspace(
            &workspace,
            "abc123",
            "/workspace/.devcontainer/devcontainer.json",
        );

        assert_eq!(workspace.safe_slug(), "app-name-v2");
        assert_eq!(
            resources.container_name,
            format!("decune-app-name-v2-{}", workspace.id())
        );
        assert_eq!(
            resources.image_tag,
            format!("decune/app-name-v2-{}:abc123", workspace.id())
        );
    }

    #[test]
    fn resource_names_truncate_safe_workspace_slug_to_48_chars() {
        let basename = "a".repeat(249);
        let root = fixture_root(&basename);
        let workspace = Workspace::resolve(&root).unwrap();
        let safe_slug = "a".repeat(48);

        let resources = DockerResources::from_workspace(
            &workspace,
            "abc123",
            "/workspace/.devcontainer/devcontainer.json",
        );

        assert_eq!(
            resources.container_name,
            format!("decune-{safe_slug}-{}", workspace.id())
        );
        assert_eq!(
            resources.image_tag,
            format!("decune/{safe_slug}-{}:abc123", workspace.id())
        );
    }

    #[test]
    fn resource_names_keep_workspace_id_for_distinct_workspaces_with_same_slug() {
        let first_root = fixture_root("Project Name");
        let second_root = fixture_root("Project Name");
        let first = Workspace::resolve(&first_root).unwrap();
        let second = Workspace::resolve(&second_root).unwrap();

        let first_resources = DockerResources::from_workspace(
            &first,
            "abc123",
            "/workspace/.devcontainer/devcontainer.json",
        );
        let second_resources = DockerResources::from_workspace(
            &second,
            "abc123",
            "/workspace/.devcontainer/devcontainer.json",
        );

        assert_eq!(first.safe_slug(), "project-name");
        assert_eq!(second.safe_slug(), "project-name");
        assert_ne!(first.id(), second.id());
        assert_ne!(
            first_resources.container_name,
            second_resources.container_name
        );
        assert_ne!(first_resources.image_tag, second_resources.image_tag);
        assert_ne!(
            first_resources.workspace_volume_name,
            second_resources.workspace_volume_name
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
}
