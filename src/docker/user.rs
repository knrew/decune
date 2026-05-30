#![allow(dead_code)]

use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use bollard::{models::ContainerCreateBody, query_parameters::CreateContainerOptionsBuilder};

use crate::{
    docker::{
        client::DockerClient,
        container::{remove_container, start_container},
        exec::{ExecCommandSpec, ExecOutput, ensure_success_output, exec_capture_output},
    },
    ui,
};

const ROOT_USER: &str = "root";
const USER_LOOKUP_NOT_FOUND_EXIT_CODE: i64 = 42;
static REMOTE_USER_LOOKUP_CONTAINER_SEQUENCE: AtomicU64 = AtomicU64::new(0);

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
    pub(crate) explicit_remote_user: Option<&'a str>,
    pub(crate) image_metadata_remote_user: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedRemoteUser {
    pub(crate) user: String,
    pub(crate) home: String,
    pub(crate) shell: Option<String>,
    pub(crate) source: RemoteUserSource,
    pub(crate) fallback_from: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContainerUserRecord {
    pub(crate) name: String,
    pub(crate) home: String,
    pub(crate) shell: Option<String>,
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
            let image_config_user = container_config_user(client, container).await?;
            select_remote_user(RemoteUserSelectionInput {
                explicit_remote_user: None,
                image_metadata_remote_user: None,
                image_config_user: image_config_user.as_deref(),
            })
        }
    };

    resolve_selected_remote_user(client, container, selection).await
}

pub(crate) async fn resolve_remote_user_from_image(
    client: &DockerClient,
    image: &str,
    input: RemoteUserResolveInput<'_>,
) -> Result<ResolvedRemoteUser> {
    let selection = match select_configured_remote_user(RemoteUserSelectionInput {
        explicit_remote_user: input.explicit_remote_user,
        image_metadata_remote_user: input.image_metadata_remote_user,
        image_config_user: None,
    }) {
        Some(selection) => selection,
        None => {
            let image_config_user = image_config_user(client, image).await?;
            select_remote_user(RemoteUserSelectionInput {
                explicit_remote_user: None,
                image_metadata_remote_user: None,
                image_config_user: image_config_user.as_deref(),
            })
        }
    };

    let container = create_remote_user_lookup_container(client, image).await?;
    let result = async {
        start_container(client, &container).await?;
        resolve_selected_remote_user(client, &container, selection).await
    }
    .await;
    let cleanup = remove_container(client, &container, true, true).await;

    match (result, cleanup) {
        (Ok(user), Ok(())) => Ok(user),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup_error)) => Err(cleanup_error)
            .with_context(|| format!("Failed to remove remote user lookup container: {container}")),
        (Err(error), Err(cleanup_error)) => Err(error.context(format!(
            "Failed to remove remote user lookup container {container}: {cleanup_error:#}"
        ))),
    }
}

pub(crate) async fn remote_user_home(
    client: &DockerClient,
    container: &str,
    user: &str,
) -> Result<String> {
    let lookup_user = docker_user_lookup_key(user);
    let record = lookup_container_user(client, container, lookup_user)
        .await?
        .with_context(|| format!("Remote user does not exist in container {container}: {user}"))?;

    Ok(record.home)
}

pub(crate) fn select_remote_user(input: RemoteUserSelectionInput<'_>) -> RemoteUserSelection {
    if let Some(selection) = select_configured_remote_user(input) {
        return selection;
    }

    if let Some(user) = normalize_user(input.image_config_user) {
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
    let lookup_user = docker_user_lookup_key(&selection.user);
    if let Some(record) = lookup_container_user(client, container, lookup_user).await? {
        return Ok(ResolvedRemoteUser {
            user: selection.user,
            home: record.home,
            shell: record.shell,
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
        shell: root.shell,
        source: RemoteUserSource::RootFallback,
        fallback_from: Some(selection.user),
    })
}

async fn container_config_user(client: &DockerClient, container: &str) -> Result<Option<String>> {
    let inspect = client
        .raw()
        .inspect_container(container, None)
        .await
        .with_context(|| {
            format!("Failed to inspect Docker container for remote user: {container}")
        })?;

    if let Some(user) = inspect
        .config
        .as_ref()
        .and_then(|config| normalize_user(config.user.as_deref()))
    {
        return Ok(Some(user));
    }

    let Some(image_id) = inspect
        .image
        .as_deref()
        .filter(|image| !image.trim().is_empty())
    else {
        return Ok(None);
    };

    image_config_user(client, image_id).await
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

async fn create_remote_user_lookup_container(client: &DockerClient, image: &str) -> Result<String> {
    let container = remote_user_lookup_container_name();
    let options = CreateContainerOptionsBuilder::default()
        .name(&container)
        .build();
    let body = ContainerCreateBody {
        image: Some(image.to_owned()),
        entrypoint: Some(vec!["/bin/sh".to_owned()]),
        cmd: Some(vec![
            "-c".to_owned(),
            "trap 'exit 0' TERM\nwhile sleep 1 & wait $!; do :; done".to_owned(),
        ]),
        user: Some(ROOT_USER.to_owned()),
        ..Default::default()
    };

    client
        .raw()
        .create_container(Some(options), body)
        .await
        .with_context(|| format!("Failed to create remote user lookup container from: {image}"))?;

    Ok(container)
}

fn remote_user_lookup_container_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let sequence = REMOTE_USER_LOOKUP_CONTAINER_SEQUENCE.fetch_add(1, Ordering::Relaxed);

    format!(
        "decune-user-lookup-{}-{nanos}-{sequence}",
        std::process::id()
    )
}

async fn lookup_container_user(
    client: &DockerClient,
    container: &str,
    user: &str,
) -> Result<Option<ContainerUserRecord>> {
    let command = lookup_user_command();
    let output = exec_capture_output(
        client,
        container,
        &ExecCommandSpec {
            command: command.clone(),
            user: None,
            working_dir: None,
            env: BTreeMap::from([("DECUNE_REMOTE_USER".to_owned(), user.to_owned())]),
            tty: false,
        },
    )
    .await
    .with_context(|| format!("Failed to query user in container {container}: {user}"))?;

    handle_user_lookup_output(container, user, &command, output)
}

fn lookup_user_command() -> Vec<String> {
    vec![
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        format!(
            "while IFS=: read -r name passwd uid gid gecos home shell; do if [ \"$name\" = \"$DECUNE_REMOTE_USER\" ] || [ \"$uid\" = \"$DECUNE_REMOTE_USER\" ]; then printf '%s:%s:%s:%s:%s:%s:%s\\n' \"$name\" \"$passwd\" \"$uid\" \"$gid\" \"$gecos\" \"$home\" \"$shell\"; exit 0; fi; done </etc/passwd; status=$?; if [ \"$status\" -eq 0 ]; then exit {USER_LOOKUP_NOT_FOUND_EXIT_CODE}; fi; exit \"$status\""
        ),
    ]
}

fn handle_user_lookup_output(
    container: &str,
    user: &str,
    command: &[String],
    output: ExecOutput,
) -> Result<Option<ContainerUserRecord>> {
    if output.exit_code != 0 {
        if output.exit_code == USER_LOOKUP_NOT_FOUND_EXIT_CODE {
            return Ok(None);
        }

        if let Err(error) = ensure_success_output(container, command, &output) {
            bail!("Failed to query user in container {container}: {user}: {error}");
        }
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
        shell: normalize_user(Some(fields[6])),
    })
}

fn normalize_user(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn docker_user_lookup_key(user: &str) -> &str {
    user.split_once(':')
        .map(|(name, _)| name)
        .unwrap_or(user)
        .trim()
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
    fn preserves_image_config_user_group_suffix_for_exec_user() {
        let selected = select_remote_user(RemoteUserSelectionInput {
            explicit_remote_user: None,
            image_metadata_remote_user: None,
            image_config_user: Some("node:node"),
        });

        assert_eq!(selected.user, "node:node");
        assert_eq!(selected.source, RemoteUserSource::ImageConfig);
    }

    #[test]
    fn preserves_numeric_image_config_user_group_suffix_for_exec_user() {
        let selected = select_remote_user(RemoteUserSelectionInput {
            explicit_remote_user: None,
            image_metadata_remote_user: None,
            image_config_user: Some("1000:2000"),
        });

        assert_eq!(selected.user, "1000:2000");
        assert_eq!(selected.source, RemoteUserSource::ImageConfig);
    }

    #[test]
    fn extracts_user_part_for_container_user_lookup() {
        assert_eq!(docker_user_lookup_key("node:shared"), "node");
        assert_eq!(docker_user_lookup_key("1000:2000"), "1000");
        assert_eq!(docker_user_lookup_key("vscode"), "vscode");
    }

    #[test]
    fn parses_passwd_record_home_directory() {
        let record = parse_passwd_record("vscode:x:1000:1000::/home/vscode:/bin/sh").unwrap();

        assert_eq!(record.name, "vscode");
        assert_eq!(record.home, "/home/vscode");
    }

    #[test]
    fn parses_passwd_record_login_shell() {
        let record = parse_passwd_record("vscode:x:1000:1000::/home/vscode:/bin/bash").unwrap();

        assert_eq!(record.shell.as_deref(), Some("/bin/bash"));
    }

    #[test]
    fn rejects_passwd_record_without_home_directory() {
        let error =
            parse_passwd_record("broken:x:1000:1000:::").expect_err("home must be rejected");

        assert!(error.to_string().contains("home directory"));
    }

    #[test]
    fn lookup_output_returns_none_only_for_not_found_exit_code() {
        let output = crate::docker::exec::ExecOutput {
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit_code: USER_LOOKUP_NOT_FOUND_EXIT_CODE,
        };

        let result = handle_user_lookup_output(
            "test-container",
            "missing-user",
            &lookup_user_command(),
            output,
        )
        .unwrap();

        assert_eq!(result, None);
    }

    #[test]
    fn lookup_output_errors_on_lookup_exec_failure() {
        let output = crate::docker::exec::ExecOutput {
            stdout: b"partial output\n".to_vec(),
            stderr: b"/bin/sh: cannot open /etc/passwd\n".to_vec(),
            exit_code: 2,
        };

        let error =
            handle_user_lookup_output("test-container", "vscode", &lookup_user_command(), output)
                .expect_err("lookup failure must not be treated as missing user");
        let message = error.to_string();

        assert!(message.contains("exit code 2"));
        assert!(message.contains("cannot open /etc/passwd"));
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
    fn resolves_grouped_user_from_image_config_when_docker_tests_are_enabled() {
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
            let image = test_image_tag("remote-user-grouped");
            let name = test_container_name("remote-user-grouped");
            let result = async {
                ensure_image(&client, "alpine:3.20", PullPolicy::Missing).await?;
                build_grouped_user_test_image(&client, &image).await?;
                create_running_user_test_container(&client, &name, &image).await?;

                let user = resolve_remote_user(
                    &client,
                    &name,
                    RemoteUserResolveInput {
                        explicit_remote_user: None,
                        image_metadata_remote_user: None,
                    },
                )
                .await?;

                assert_eq!(user.user, "vscode:shared");
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
                        explicit_remote_user: Some("vscode"),
                        image_metadata_remote_user: None,
                    },
                )
                .await?;
                let metadata = resolve_remote_user(
                    &client,
                    &name,
                    RemoteUserResolveInput {
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

    async fn build_grouped_user_test_image(client: &DockerClient, image: &str) -> Result<()> {
        let context = tempfile::tempdir()?;
        let dockerfile_path = context.path().join("Dockerfile");
        std::fs::write(
            &dockerfile_path,
            r#"
            FROM alpine:3.20
            RUN adduser -D vscode && addgroup -S shared && addgroup vscode shared
            USER vscode:shared
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
