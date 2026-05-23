#![allow(dead_code)]

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};

use crate::{
    docker::{
        client::DockerClient,
        exec::{ExecCommandSpec, exec_capture_output},
    },
    ui,
};

const ROOT_USER: &str = "root";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteUserSource {
    Explicit,
    ImageMetadata,
    ImageConfig,
    RootFallback,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteUserSelection {
    pub(crate) user: String,
    pub(crate) source: RemoteUserSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RemoteUserSelectionInput<'a> {
    pub(crate) explicit_remote_user: Option<&'a str>,
    pub(crate) image_metadata_remote_user: Option<&'a str>,
    pub(crate) image_config_user: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteUserResolveInput<'a> {
    pub(crate) image: &'a str,
    pub(crate) explicit_remote_user: Option<&'a str>,
    pub(crate) image_metadata_remote_user: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedRemoteUser {
    pub(crate) user: String,
    pub(crate) home: String,
    pub(crate) source: RemoteUserSource,
    pub(crate) fallback_from: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContainerUserRecord {
    pub(crate) name: String,
    pub(crate) home: String,
}

pub(crate) async fn resolve_remote_user(
    client: &DockerClient,
    container: &str,
    input: RemoteUserResolveInput<'_>,
) -> Result<ResolvedRemoteUser> {
    let selection = match select_configured_remote_user(RemoteUserSelectionInput {
        explicit_remote_user: input.explicit_remote_user,
        image_metadata_remote_user: input.image_metadata_remote_user,
        image_config_user: None,
    }) {
        Some(selection) => selection,
        None => {
            let image_config_user = image_config_user(client, input.image).await?;
            select_remote_user(RemoteUserSelectionInput {
                explicit_remote_user: None,
                image_metadata_remote_user: None,
                image_config_user: image_config_user.as_deref(),
            })
        }
    };

    resolve_selected_remote_user(client, container, selection).await
}

pub(crate) async fn remote_user_home(
    client: &DockerClient,
    container: &str,
    user: &str,
) -> Result<String> {
    let record = lookup_container_user(client, container, user)
        .await?
        .with_context(|| format!("Remote user does not exist in container {container}: {user}"))?;

    Ok(record.home)
}

pub(crate) fn select_remote_user(input: RemoteUserSelectionInput<'_>) -> RemoteUserSelection {
    if let Some(selection) = select_configured_remote_user(input) {
        return selection;
    }

    if let Some(user) = normalize_image_config_user(input.image_config_user) {
        return RemoteUserSelection {
            user,
            source: RemoteUserSource::ImageConfig,
        };
    }

    RemoteUserSelection {
        user: ROOT_USER.to_owned(),
        source: RemoteUserSource::RootFallback,
    }
}

fn select_configured_remote_user(
    input: RemoteUserSelectionInput<'_>,
) -> Option<RemoteUserSelection> {
    if let Some(user) = normalize_user(input.explicit_remote_user) {
        return Some(RemoteUserSelection {
            user,
            source: RemoteUserSource::Explicit,
        });
    }

    if let Some(user) = normalize_user(input.image_metadata_remote_user) {
        return Some(RemoteUserSelection {
            user,
            source: RemoteUserSource::ImageMetadata,
        });
    }

    None
}

async fn resolve_selected_remote_user(
    client: &DockerClient,
    container: &str,
    selection: RemoteUserSelection,
) -> Result<ResolvedRemoteUser> {
    if let Some(record) = lookup_container_user(client, container, &selection.user).await? {
        return Ok(ResolvedRemoteUser {
            user: selection.user,
            home: record.home,
            source: selection.source,
            fallback_from: None,
        });
    }

    if selection.user != ROOT_USER {
        ui::warn(&format!(
            "Remote user does not exist in container {container}: {}. Falling back to root.",
            selection.user
        ));
    }

    let root = lookup_container_user(client, container, ROOT_USER)
        .await?
        .with_context(|| {
            format!("Fallback remote user does not exist in container: {container}")
        })?;

    Ok(ResolvedRemoteUser {
        user: ROOT_USER.to_owned(),
        home: root.home,
        source: RemoteUserSource::RootFallback,
        fallback_from: Some(selection.user),
    })
}

async fn image_config_user(client: &DockerClient, image: &str) -> Result<Option<String>> {
    let inspect = client
        .raw()
        .inspect_image(image)
        .await
        .with_context(|| format!("Failed to inspect Docker image for remote user: {image}"))?;

    Ok(inspect
        .config
        .and_then(|config| normalize_user(config.user.as_deref())))
}

async fn lookup_container_user(
    client: &DockerClient,
    container: &str,
    user: &str,
) -> Result<Option<ContainerUserRecord>> {
    let output = exec_capture_output(
        client,
        container,
        &ExecCommandSpec {
            command: vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                "while IFS=: read -r name passwd uid gid gecos home shell; do if [ \"$name\" = \"$DECUNE_REMOTE_USER\" ] || [ \"$uid\" = \"$DECUNE_REMOTE_USER\" ]; then printf '%s:%s:%s:%s:%s:%s:%s\\n' \"$name\" \"$passwd\" \"$uid\" \"$gid\" \"$gecos\" \"$home\" \"$shell\"; exit 0; fi; done </etc/passwd; exit 1".to_owned(),
            ],
            user: None,
            working_dir: None,
            env: BTreeMap::from([("DECUNE_REMOTE_USER".to_owned(), user.to_owned())]),
            tty: false,
        },
    )
    .await
    .with_context(|| format!("Failed to query user in container {container}: {user}"))?;

    if output.exit_code != 0 {
        return Ok(None);
    }

    let stdout = String::from_utf8(output.stdout).with_context(|| {
        format!("User lookup returned non-UTF-8 output in container: {container}")
    })?;
    let line = stdout.lines().next().unwrap_or_default();
    let record = parse_passwd_record(line)
        .with_context(|| format!("Failed to parse passwd record for user: {user}"))?;

    Ok(Some(record))
}

fn parse_passwd_record(line: &str) -> Result<ContainerUserRecord> {
    let fields = line.split(':').collect::<Vec<_>>();
    if fields.len() < 7 {
        bail!("passwd record must contain at least 7 fields");
    }

    let name = fields[0].trim();
    if name.is_empty() {
        bail!("passwd record user name must not be empty");
    }

    let home = fields[5].trim();
    if home.is_empty() {
        bail!("passwd record home directory must not be empty");
    }
    if !home.starts_with('/') {
        bail!("passwd record home directory must be absolute: {home}");
    }

    Ok(ContainerUserRecord {
        name: name.to_owned(),
        home: home.to_owned(),
    })
}

fn normalize_user(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn normalize_image_config_user(value: Option<&str>) -> Option<String> {
    normalize_user(value).and_then(|user| {
        let user = user
            .split_once(':')
            .map(|(name, _)| name)
            .unwrap_or(&user)
            .trim()
            .to_owned();

        if user.is_empty() { None } else { Some(user) }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bollard::{
        models::ContainerCreateBody,
        query_parameters::{CreateContainerOptionsBuilder, StartContainerOptionsBuilder},
    };

    use crate::docker::{
        build::{DockerBuildInput, DockerBuildOptions, ResolvedBuildContext, build_image},
        container::remove_container,
        image::{PullPolicy, ensure_image, remove_image},
    };

    const DOCKER_TESTS_ENV: &str = "DECUNE_DOCKER_TESTS";

    #[test]
    fn selects_explicit_remote_user_before_image_sources() {
        let selected = select_remote_user(RemoteUserSelectionInput {
            explicit_remote_user: Some("vscode"),
            image_metadata_remote_user: Some("metadata-user"),
            image_config_user: Some("image-user"),
        });

        assert_eq!(
            selected,
            RemoteUserSelection {
                user: "vscode".to_owned(),
                source: RemoteUserSource::Explicit,
            }
        );
    }

    #[test]
    fn falls_back_through_image_metadata_image_config_and_root() {
        let metadata = select_remote_user(RemoteUserSelectionInput {
            explicit_remote_user: None,
            image_metadata_remote_user: Some("metadata-user"),
            image_config_user: Some("image-user"),
        });
        let image = select_remote_user(RemoteUserSelectionInput {
            explicit_remote_user: None,
            image_metadata_remote_user: None,
            image_config_user: Some("image-user"),
        });
        let root = select_remote_user(RemoteUserSelectionInput {
            explicit_remote_user: None,
            image_metadata_remote_user: None,
            image_config_user: None,
        });

        assert_eq!(metadata.user, "metadata-user");
        assert_eq!(metadata.source, RemoteUserSource::ImageMetadata);
        assert_eq!(image.user, "image-user");
        assert_eq!(image.source, RemoteUserSource::ImageConfig);
        assert_eq!(root.user, "root");
        assert_eq!(root.source, RemoteUserSource::RootFallback);
    }

    #[test]
    fn normalizes_image_config_user_group_suffix() {
        let selected = select_remote_user(RemoteUserSelectionInput {
            explicit_remote_user: None,
            image_metadata_remote_user: None,
            image_config_user: Some("node:node"),
        });

        assert_eq!(selected.user, "node");
        assert_eq!(selected.source, RemoteUserSource::ImageConfig);
    }

    #[test]
    fn parses_passwd_record_home_directory() {
        let record = parse_passwd_record("vscode:x:1000:1000::/home/vscode:/bin/sh").unwrap();

        assert_eq!(record.name, "vscode");
        assert_eq!(record.home, "/home/vscode");
    }

    #[test]
    fn rejects_passwd_record_without_home_directory() {
        let error =
            parse_passwd_record("broken:x:1000:1000:::").expect_err("home must be rejected");

        assert!(error.to_string().contains("home directory"));
    }

    #[test]
    fn resolves_root_user_from_root_image_when_docker_tests_are_enabled() {
        if !docker_tests_enabled() {
            eprintln!("skipped: set {DOCKER_TESTS_ENV}=1 to run Docker integration tests");
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let client = DockerClient::connect_from_env().unwrap();
            let name = test_container_name("remote-user-root");
            let result = async {
                ensure_image(&client, "alpine:3.20", PullPolicy::Missing).await?;
                create_running_user_test_container(&client, &name, "alpine:3.20").await?;

                let user = resolve_remote_user(
                    &client,
                    &name,
                    RemoteUserResolveInput {
                        image: "alpine:3.20",
                        explicit_remote_user: None,
                        image_metadata_remote_user: None,
                    },
                )
                .await?;

                assert_eq!(user.user, "root");
                assert_eq!(user.home, "/root");
                assert_eq!(user.source, RemoteUserSource::RootFallback);
                assert_eq!(user.fallback_from, None);
                assert_eq!(remote_user_home(&client, &name, "root").await?, "/root");

                Ok(())
            }
            .await;

            let cleanup = remove_container(&client, &name, true, true).await;
            result.and(cleanup).unwrap();
        });
    }

    #[test]
    fn resolves_non_root_user_from_image_config_when_docker_tests_are_enabled() {
        if !docker_tests_enabled() {
            eprintln!("skipped: set {DOCKER_TESTS_ENV}=1 to run Docker integration tests");
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let client = DockerClient::connect_from_env().unwrap();
            let image = test_image_tag("remote-user-nonroot");
            let name = test_container_name("remote-user-nonroot");
            let result = async {
                ensure_image(&client, "alpine:3.20", PullPolicy::Missing).await?;
                build_non_root_test_image(&client, &image).await?;
                create_running_user_test_container(&client, &name, &image).await?;

                let user = resolve_remote_user(
                    &client,
                    &name,
                    RemoteUserResolveInput {
                        image: &image,
                        explicit_remote_user: None,
                        image_metadata_remote_user: None,
                    },
                )
                .await?;

                assert_eq!(user.user, "vscode");
                assert_eq!(user.home, "/home/vscode");
                assert_eq!(user.source, RemoteUserSource::ImageConfig);
                assert_eq!(user.fallback_from, None);

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }

    #[test]
    fn falls_back_to_root_when_explicit_user_is_missing_when_docker_tests_are_enabled() {
        if !docker_tests_enabled() {
            eprintln!("skipped: set {DOCKER_TESTS_ENV}=1 to run Docker integration tests");
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let client = DockerClient::connect_from_env().unwrap();
            let name = test_container_name("remote-user-missing");
            let result = async {
                ensure_image(&client, "alpine:3.20", PullPolicy::Missing).await?;
                create_running_user_test_container(&client, &name, "alpine:3.20").await?;

                let user = resolve_remote_user(
                    &client,
                    &name,
                    RemoteUserResolveInput {
                        image: "alpine:3.20",
                        explicit_remote_user: Some("missing-user"),
                        image_metadata_remote_user: None,
                    },
                )
                .await?;

                assert_eq!(user.user, "root");
                assert_eq!(user.home, "/root");
                assert_eq!(user.source, RemoteUserSource::RootFallback);
                assert_eq!(user.fallback_from.as_deref(), Some("missing-user"));

                Ok(())
            }
            .await;

            let cleanup = remove_container(&client, &name, true, true).await;
            result.and(cleanup).unwrap();
        });
    }

    #[test]
    fn resolves_configured_remote_user_without_image_config_inspect_when_image_tag_is_missing() {
        if !docker_tests_enabled() {
            eprintln!("skipped: set {DOCKER_TESTS_ENV}=1 to run Docker integration tests");
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let client = DockerClient::connect_from_env().unwrap();
            let image = test_image_tag("remote-user-missing-image");
            let name = test_container_name("remote-user-missing-image");
            let result = async {
                ensure_image(&client, "alpine:3.20", PullPolicy::Missing).await?;
                build_non_root_test_image(&client, &image).await?;
                create_running_user_test_container(&client, &name, &image).await?;
                remove_image(&client, &image, true).await?;

                let explicit = resolve_remote_user(
                    &client,
                    &name,
                    RemoteUserResolveInput {
                        image: &image,
                        explicit_remote_user: Some("vscode"),
                        image_metadata_remote_user: None,
                    },
                )
                .await?;
                let metadata = resolve_remote_user(
                    &client,
                    &name,
                    RemoteUserResolveInput {
                        image: &image,
                        explicit_remote_user: None,
                        image_metadata_remote_user: Some("vscode"),
                    },
                )
                .await?;

                assert_eq!(explicit.user, "vscode");
                assert_eq!(explicit.home, "/home/vscode");
                assert_eq!(explicit.source, RemoteUserSource::Explicit);
                assert_eq!(explicit.fallback_from, None);
                assert_eq!(metadata.user, "vscode");
                assert_eq!(metadata.home, "/home/vscode");
                assert_eq!(metadata.source, RemoteUserSource::ImageMetadata);
                assert_eq!(metadata.fallback_from, None);

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }

    fn docker_tests_enabled() -> bool {
        std::env::var_os(DOCKER_TESTS_ENV).as_deref() == Some(std::ffi::OsStr::new("1"))
    }

    async fn build_non_root_test_image(client: &DockerClient, image: &str) -> Result<()> {
        let context = tempfile::tempdir()?;
        let dockerfile_path = context.path().join("Dockerfile");
        std::fs::write(
            &dockerfile_path,
            r#"
            FROM alpine:3.20
            RUN adduser -D vscode
            USER vscode
            "#,
        )?;

        build_image(
            client,
            DockerBuildInput {
                image_tag: image.to_owned(),
                labels: std::collections::HashMap::new(),
                context: ResolvedBuildContext {
                    context_dir: context.path().to_path_buf(),
                    dockerfile_path,
                    dockerfile_in_context: "Dockerfile".into(),
                    dockerignore_path: None,
                },
                options: DockerBuildOptions::default(),
            },
        )
        .await
    }

    async fn create_running_user_test_container(
        client: &DockerClient,
        name: &str,
        image: &str,
    ) -> Result<()> {
        remove_container(client, name, true, true).await?;

        let options = CreateContainerOptionsBuilder::default().name(name).build();
        let body = ContainerCreateBody {
            image: Some(image.to_owned()),
            entrypoint: Some(vec!["/bin/sh".to_owned()]),
            cmd: Some(vec![
                "-c".to_owned(),
                "trap 'exit 0' TERM\nwhile sleep 1 & wait $!; do :; done".to_owned(),
            ]),
            ..Default::default()
        };

        client.raw().create_container(Some(options), body).await?;
        client
            .raw()
            .start_container(name, Some(StartContainerOptionsBuilder::default().build()))
            .await?;

        Ok(())
    }

    fn test_container_name(suffix: &str) -> String {
        format!("decune-{suffix}-{}", std::process::id())
    }

    fn test_image_tag(suffix: &str) -> String {
        format!("decune/{suffix}-{}:test", std::process::id())
    }
}
