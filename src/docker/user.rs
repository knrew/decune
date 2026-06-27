use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};

use crate::docker::{
    client::DockerClient,
    container::{ContainerCreateSpec, ContainerHostConfig, remove_container, start_container},
    exec::{ExecCommandSpec, ExecOutput, ensure_success_output, exec_capture_output},
};

const ROOT_USER: &str = "root";
const USER_LOOKUP_NOT_FOUND_EXIT_CODE: i64 = 42;
static REMOTE_USER_LOOKUP_CONTAINER_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteUserSource {
    Explicit,
    ImageMetadata,
    ComposeService,
    ImageConfig,
    RootFallback,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteUserSelection {
    pub(crate) user: String,
    pub(crate) source: RemoteUserSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DockerUserIdentifier {
    Name(String),
    Id(u32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DockerImageConfigUser {
    pub(crate) raw: String,
    pub(crate) user: DockerUserIdentifier,
    pub(crate) group: Option<DockerUserIdentifier>,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RemoteUserSelectionInput<'a> {
    pub(crate) explicit_remote_user: Option<&'a str>,
    pub(crate) image_metadata_remote_user: Option<&'a str>,
    pub(crate) image_config_user: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EffectiveUserResolveInput<'a> {
    pub(crate) devcontainer_remote_user: Option<&'a str>,
    pub(crate) devcontainer_container_user: Option<&'a str>,
    pub(crate) image_metadata_remote_user: Option<&'a str>,
    pub(crate) image_metadata_container_user: Option<&'a str>,
    pub(crate) image_config_user: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EffectiveUser {
    pub(crate) user: String,
    pub(crate) source: RemoteUserSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EffectiveRemoteUserOrigin {
    ExplicitRemote,
    ImageMetadataRemote,
    Container,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EffectiveRemoteUser {
    pub(crate) user: String,
    pub(crate) source: RemoteUserSource,
    pub(crate) origin: EffectiveRemoteUserOrigin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EffectiveUsers {
    pub(crate) container_user: EffectiveUser,
    pub(crate) remote_user: EffectiveRemoteUser,
}

impl EffectiveUsers {
    pub(crate) fn root() -> Self {
        let container_user = EffectiveUser {
            user: ROOT_USER.to_owned(),
            source: RemoteUserSource::RootFallback,
        };
        let remote_user = EffectiveRemoteUser {
            user: container_user.user.clone(),
            source: container_user.source,
            origin: EffectiveRemoteUserOrigin::Container,
        };

        Self {
            container_user,
            remote_user,
        }
    }

    fn remote_selection(&self) -> RemoteUserSelection {
        RemoteUserSelection {
            user: self.remote_user.user.clone(),
            source: self.remote_user.source,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HostUserIds {
    pub(crate) uid: u32,
    pub(crate) gid: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostPlatform {
    Linux,
    NonLinux,
}

impl HostPlatform {
    pub(crate) const fn current() -> Self {
        if cfg!(target_os = "linux") {
            Self::Linux
        } else {
            Self::NonLinux
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UidGidSyncTargetKind {
    RemoteUser,
    ContainerUser,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UidGidSyncTarget {
    pub(crate) kind: UidGidSyncTargetKind,
    pub(crate) user: String,
    pub(crate) host: HostUserIds,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UidGidSyncNoopReason {
    Disabled,
    NonLinuxHost,
    NoExplicitUser,
    NumericUserWithoutPasswd,
    Root,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UidGidSyncDecision {
    Sync(UidGidSyncTarget),
    Noop(UidGidSyncNoopReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedUserIds {
    pub(crate) name: String,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UidGidSyncPlan {
    Sync {
        target: UidGidSyncTarget,
        container: ResolvedUserIds,
    },
    Noop {
        reason: UidGidSyncNoopReason,
    },
}

impl Default for UidGidSyncPlan {
    fn default() -> Self {
        Self::Noop {
            reason: UidGidSyncNoopReason::NoExplicitUser,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedRemoteUser {
    pub(crate) user: String,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) home: Option<String>,
    pub(crate) shell: Option<String>,
    pub(crate) source: RemoteUserSource,
    pub(crate) fallback_from: Option<String>,
}

impl ResolvedRemoteUser {
    pub(crate) fn home(&self) -> Result<&str> {
        self.home.as_deref().with_context(|| {
            format!(
                "Remote user home directory is unavailable because the user has no passwd entry: {}",
                self.user
            )
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContainerUserRecord {
    pub(crate) name: String,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) home: String,
    pub(crate) shell: Option<String>,
}

pub(crate) async fn resolve_remote_user(
    client: &DockerClient,
    container: &str,
    effective_users: &EffectiveUsers,
    uid_gid_sync_plan: &UidGidSyncPlan,
) -> Result<ResolvedRemoteUser> {
    let mut selection = effective_users.remote_selection();
    selection.user = uid_gid_sync_runtime_user(&selection.user, uid_gid_sync_plan)?;
    resolve_selected_remote_user(client, container, selection).await
}

pub(crate) async fn resolve_effective_users_from_image(
    client: &DockerClient,
    image: &str,
    input: EffectiveUserResolveInput<'_>,
) -> Result<EffectiveUsers> {
    let image_config_user = image_config_user(client, image).await?;
    resolve_effective_users(EffectiveUserResolveInput {
        image_config_user: image_config_user.as_deref(),
        ..input
    })
}

pub(crate) async fn resolve_remote_user_from_image(
    client: &DockerClient,
    image: &str,
    effective_users: &EffectiveUsers,
) -> Result<ResolvedRemoteUser> {
    let container = create_remote_user_lookup_container(client, image).await?;
    let result = async {
        start_container(client, &container).await?;
        resolve_selected_remote_user(client, &container, effective_users.remote_selection()).await
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

pub(crate) async fn resolve_uid_gid_sync_plan_from_image(
    client: &DockerClient,
    image: &str,
    effective_users: &EffectiveUsers,
    update_remote_user_uid: bool,
    host_platform: HostPlatform,
    host: HostUserIds,
) -> Result<UidGidSyncPlan> {
    match decide_uid_gid_sync(effective_users, update_remote_user_uid, host_platform, host) {
        UidGidSyncDecision::Noop(reason) => Ok(UidGidSyncPlan::Noop { reason }),
        UidGidSyncDecision::Sync(target) => {
            let container = create_remote_user_lookup_container(client, image).await?;
            let result = async {
                start_container(client, &container).await?;
                let lookup_user = docker_user_lookup_key(&target.user);
                let Some(record) = lookup_container_user(client, &container, lookup_user).await?
                else {
                    if is_numeric_user_identity(&target.user) {
                        return Ok(UidGidSyncPlan::Noop {
                            reason: UidGidSyncNoopReason::NumericUserWithoutPasswd,
                        });
                    }

                    bail!(
                        "UID/GID sync user does not exist in image {image}: {}",
                        target.user
                    );
                };
                Ok(UidGidSyncPlan::Sync {
                    target,
                    container: ResolvedUserIds {
                        name: record.name,
                        uid: record.uid,
                        gid: record.gid,
                    },
                })
            }
            .await;
            let cleanup = remove_container(client, &container, true, true).await;

            match (result, cleanup) {
                (Ok(plan), Ok(())) => Ok(plan),
                (Err(error), Ok(())) => Err(error),
                (Ok(_), Err(cleanup_error)) => Err(cleanup_error).with_context(|| {
                    format!("Failed to remove UID/GID sync lookup container: {container}")
                }),
                (Err(error), Err(cleanup_error)) => Err(error).context(format!(
                    "Failed to remove UID/GID sync lookup container {container}: {cleanup_error:#}"
                )),
            }
        }
    }
}

pub(crate) fn uid_gid_sync_runtime_user(user: &str, plan: &UidGidSyncPlan) -> Result<String> {
    let UidGidSyncPlan::Sync { target, container } = plan else {
        return Ok(user.to_owned());
    };
    let Some(parsed) = parse_docker_image_config_user(Some(user))? else {
        return Ok(user.to_owned());
    };
    let matches_sync_user = match &parsed.user {
        DockerUserIdentifier::Id(uid) => *uid == container.uid,
        DockerUserIdentifier::Name(name) => name == &container.name,
    };
    if !matches_sync_user {
        return Ok(parsed.raw);
    }

    let mut runtime_user = match parsed.user {
        DockerUserIdentifier::Id(_) => container.name.clone(),
        DockerUserIdentifier::Name(name) => name,
    };
    if let Some(group) = parsed.group {
        runtime_user.push(':');
        match group {
            DockerUserIdentifier::Id(gid) if gid == container.gid => {
                runtime_user.push_str(&target.host.gid.to_string());
            }
            DockerUserIdentifier::Id(gid) => runtime_user.push_str(&gid.to_string()),
            DockerUserIdentifier::Name(group) => runtime_user.push_str(&group),
        }
    }

    Ok(runtime_user)
}
pub(crate) fn resolve_effective_users(
    input: EffectiveUserResolveInput<'_>,
) -> Result<EffectiveUsers> {
    resolve_effective_users_with_compose_service_user(input, None)
}

pub(crate) fn resolve_effective_users_with_compose_service_user(
    input: EffectiveUserResolveInput<'_>,
    compose_service_user: Option<&str>,
) -> Result<EffectiveUsers> {
    let image_config_user = parse_docker_image_config_user(input.image_config_user)?;
    let container_user = normalize_user(input.devcontainer_container_user)
        .map(|user| EffectiveUser {
            user,
            source: RemoteUserSource::Explicit,
        })
        .or_else(|| {
            normalize_user(input.image_metadata_container_user).map(|user| EffectiveUser {
                user,
                source: RemoteUserSource::ImageMetadata,
            })
        })
        .or_else(|| {
            normalize_user(compose_service_user).map(|user| EffectiveUser {
                user,
                source: RemoteUserSource::ComposeService,
            })
        })
        .or_else(|| {
            image_config_user.map(|user| EffectiveUser {
                user: user.raw,
                source: RemoteUserSource::ImageConfig,
            })
        })
        .unwrap_or_else(|| EffectiveUser {
            user: ROOT_USER.to_owned(),
            source: RemoteUserSource::RootFallback,
        });

    let remote_user = if let Some(user) = normalize_user(input.devcontainer_remote_user) {
        EffectiveRemoteUser {
            user,
            source: RemoteUserSource::Explicit,
            origin: EffectiveRemoteUserOrigin::ExplicitRemote,
        }
    } else if let Some(user) = normalize_user(input.image_metadata_remote_user) {
        EffectiveRemoteUser {
            user,
            source: RemoteUserSource::ImageMetadata,
            origin: EffectiveRemoteUserOrigin::ImageMetadataRemote,
        }
    } else {
        EffectiveRemoteUser {
            user: container_user.user.clone(),
            source: container_user.source,
            origin: EffectiveRemoteUserOrigin::Container,
        }
    };

    Ok(EffectiveUsers {
        container_user,
        remote_user,
    })
}

pub(crate) fn decide_uid_gid_sync(
    users: &EffectiveUsers,
    update_remote_user_uid: bool,
    host_platform: HostPlatform,
    host: HostUserIds,
) -> UidGidSyncDecision {
    if !update_remote_user_uid {
        return UidGidSyncDecision::Noop(UidGidSyncNoopReason::Disabled);
    }

    if host_platform != HostPlatform::Linux {
        return UidGidSyncDecision::Noop(UidGidSyncNoopReason::NonLinuxHost);
    }

    let target = match users.remote_user.origin {
        EffectiveRemoteUserOrigin::ExplicitRemote
        | EffectiveRemoteUserOrigin::ImageMetadataRemote => Some(UidGidSyncTarget {
            kind: UidGidSyncTargetKind::RemoteUser,
            user: users.remote_user.user.clone(),
            host,
        }),
        EffectiveRemoteUserOrigin::Container
            if matches!(
                users.container_user.source,
                RemoteUserSource::Explicit
                    | RemoteUserSource::ImageMetadata
                    | RemoteUserSource::ComposeService
            ) =>
        {
            Some(UidGidSyncTarget {
                kind: UidGidSyncTargetKind::ContainerUser,
                user: users.container_user.user.clone(),
                host,
            })
        }
        EffectiveRemoteUserOrigin::Container => None,
    };

    let Some(target) = target else {
        return UidGidSyncDecision::Noop(UidGidSyncNoopReason::NoExplicitUser);
    };

    if is_root_user(&target.user) {
        return UidGidSyncDecision::Noop(UidGidSyncNoopReason::Root);
    }

    UidGidSyncDecision::Sync(target)
}

pub(crate) fn current_host_user_ids() -> HostUserIds {
    HostUserIds {
        uid: current_uid(),
        gid: current_gid(),
    }
}

#[cfg(test)]
pub(crate) fn select_remote_user(input: RemoteUserSelectionInput<'_>) -> RemoteUserSelection {
    resolve_effective_users(EffectiveUserResolveInput {
        devcontainer_remote_user: input.explicit_remote_user,
        devcontainer_container_user: None,
        image_metadata_remote_user: input.image_metadata_remote_user,
        image_metadata_container_user: None,
        image_config_user: input.image_config_user,
    })
    .map_or_else(
        |_| RemoteUserSelection {
            user: ROOT_USER.to_owned(),
            source: RemoteUserSource::RootFallback,
        },
        |users| users.remote_selection(),
    )
}

async fn resolve_selected_remote_user(
    client: &DockerClient,
    container: &str,
    selection: RemoteUserSelection,
) -> Result<ResolvedRemoteUser> {
    let lookup_user = docker_user_lookup_key(&selection.user);
    let record = lookup_container_user(client, container, lookup_user).await?;
    let Some(record) = record else {
        if is_numeric_user_identity(&selection.user) {
            let ids = resolve_numeric_remote_user_ids(client, container, &selection.user).await?;
            return Ok(ResolvedRemoteUser {
                user: selection.user,
                uid: ids.uid,
                gid: ids.gid,
                home: None,
                shell: None,
                source: selection.source,
                fallback_from: None,
            });
        }

        bail!(
            "Remote user does not exist in container {container}: {}",
            selection.user
        );
    };

    Ok(ResolvedRemoteUser {
        user: selection.user,
        uid: record.uid,
        gid: record.gid,
        home: Some(record.home),
        shell: record.shell,
        source: selection.source,
        fallback_from: None,
    })
}

pub(crate) async fn image_config_user(
    client: &DockerClient,
    image: &str,
) -> Result<Option<String>> {
    let inspect = client
        .cli()
        .inspect_image(image)
        .await
        .with_context(|| format!("Failed to inspect Docker image for remote user: {image}"))?;

    parse_docker_image_config_user(
        inspect
            .config
            .as_ref()
            .and_then(|config| config.user.as_deref()),
    )
    .map(|user| user.map(|user| user.raw))
}

async fn create_remote_user_lookup_container(client: &DockerClient, image: &str) -> Result<String> {
    let container = remote_user_lookup_container_name();
    let spec = ContainerCreateSpec {
        image: image.to_owned(),
        name: container.clone(),
        entrypoint: Some(vec!["/bin/sh".to_owned()]),
        command: Some(vec![
            "-c".to_owned(),
            "trap 'exit 0' TERM\nwhile sleep 1 & wait $!; do :; done".to_owned(),
        ]),
        labels: BTreeMap::new(),
        env: BTreeMap::new(),
        working_dir: None,
        user: Some(ROOT_USER.to_owned()),
        mounts: Vec::new(),
        publish_ports: Vec::new(),
        host_config: ContainerHostConfig::default(),
    };

    client
        .cli()
        .create_container(&spec)
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
            redactions: Vec::new(),
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
        uid: parse_passwd_id(fields[2], "uid")?,
        gid: parse_passwd_id(fields[3], "gid")?,
        home: home.to_owned(),
        shell: normalize_user(Some(fields[6])),
    })
}

fn parse_passwd_id(value: &str, label: &str) -> Result<u32> {
    value
        .trim()
        .parse()
        .with_context(|| format!("passwd record {label} must be an unsigned integer: {value}"))
}

fn parse_docker_image_config_user(value: Option<&str>) -> Result<Option<DockerImageConfigUser>> {
    let Some(raw) = normalize_user(value) else {
        return Ok(None);
    };

    let mut fields = raw.split(':');
    let user = fields
        .next()
        .expect("split always returns the first field for a non-empty string");
    let group = fields.next();
    if fields.next().is_some() {
        bail!("Docker image user must contain at most one group separator: {raw}");
    }

    let user = parse_docker_user_identifier(user, "user")?;
    let group = group
        .map(|value| parse_docker_user_identifier(value, "group"))
        .transpose()?;

    Ok(Some(DockerImageConfigUser { raw, user, group }))
}

fn parse_docker_user_identifier(value: &str, label: &str) -> Result<DockerUserIdentifier> {
    let value = value.trim();
    if value.is_empty() {
        bail!("Docker image user {label} must not be empty");
    }
    if value.chars().any(char::is_whitespace) || value.chars().any(char::is_control) {
        bail!(
            "Docker image user {label} contains unsupported whitespace or control characters: {value}"
        );
    }

    if value.chars().all(|ch| ch.is_ascii_digit()) {
        return Ok(DockerUserIdentifier::Id(value.parse().with_context(
            || format!("Docker image user {label} must fit in u32: {value}"),
        )?));
    }

    Ok(DockerUserIdentifier::Name(value.to_owned()))
}

fn normalize_user(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn docker_user_lookup_key(user: &str) -> &str {
    user.split_once(':').map_or(user, |(name, _)| name).trim()
}

fn is_numeric_user_identity(user: &str) -> bool {
    let user = docker_user_lookup_key(user);
    !user.is_empty() && user.chars().all(|ch| ch.is_ascii_digit())
}

fn is_root_user(user: &str) -> bool {
    matches!(docker_user_lookup_key(user), ROOT_USER | "0")
}

async fn resolve_numeric_remote_user_ids(
    client: &DockerClient,
    container: &str,
    user: &str,
) -> Result<ResolvedUserIds> {
    let command = vec![
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        "id -u && id -g".to_owned(),
    ];
    let output = exec_capture_output(
        client,
        container,
        &ExecCommandSpec {
            command: command.clone(),
            user: Some(user.to_owned()),
            working_dir: None,
            env: BTreeMap::new(),
            redactions: Vec::new(),
            tty: false,
        },
    )
    .await
    .with_context(|| format!("Failed to query numeric user in container {container}: {user}"))?;
    ensure_success_output(container, &command, &output).with_context(|| {
        format!("Failed to query numeric user in container {container}: {user}")
    })?;

    let stdout = String::from_utf8(output.stdout).with_context(|| {
        format!("Numeric user lookup returned non-UTF-8 output in container: {container}")
    })?;
    let mut lines = stdout.lines();
    let uid = parse_passwd_id(lines.next().unwrap_or_default(), "uid")
        .with_context(|| format!("Failed to parse numeric user uid: {user}"))?;
    let gid = parse_passwd_id(lines.next().unwrap_or_default(), "gid")
        .with_context(|| format!("Failed to parse numeric user gid: {user}"))?;

    Ok(ResolvedUserIds {
        name: docker_user_lookup_key(user).to_owned(),
        uid,
        gid,
    })
}

#[cfg(unix)]
fn current_uid() -> u32 {
    unsafe { libc::getuid() }
}

#[cfg(not(unix))]
fn current_uid() -> u32 {
    0
}

#[cfg(unix)]
fn current_gid() -> u32 {
    unsafe { libc::getgid() }
}

#[cfg(not(unix))]
fn current_gid() -> u32 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::docker::{
        build::{DockerBuildInput, DockerBuildOptions, ResolvedBuildContext, build_image},
        container::{create_container, remove_container, start_container},
        image::{PullPolicy, ensure_image, remove_image},
    };

    #[test]
    fn parses_docker_image_config_user_forms() {
        assert_eq!(parse_docker_image_config_user(None).unwrap(), None);
        assert_eq!(parse_docker_image_config_user(Some("")).unwrap(), None);

        let named = parse_docker_image_config_user(Some("vscode"))
            .unwrap()
            .unwrap();
        assert_eq!(named.raw, "vscode");
        assert_eq!(named.user, DockerUserIdentifier::Name("vscode".to_owned()));
        assert_eq!(named.group, None);

        let named_with_group = parse_docker_image_config_user(Some("vscode:shared"))
            .unwrap()
            .unwrap();
        assert_eq!(named_with_group.raw, "vscode:shared");
        assert_eq!(
            named_with_group.user,
            DockerUserIdentifier::Name("vscode".to_owned())
        );
        assert_eq!(
            named_with_group.group,
            Some(DockerUserIdentifier::Name("shared".to_owned()))
        );

        let numeric = parse_docker_image_config_user(Some("1001"))
            .unwrap()
            .unwrap();
        assert_eq!(numeric.raw, "1001");
        assert_eq!(numeric.user, DockerUserIdentifier::Id(1001));
        assert_eq!(numeric.group, None);

        let numeric_with_group = parse_docker_image_config_user(Some("1001:1002"))
            .unwrap()
            .unwrap();
        assert_eq!(numeric_with_group.raw, "1001:1002");
        assert_eq!(numeric_with_group.user, DockerUserIdentifier::Id(1001));
        assert_eq!(
            numeric_with_group.group,
            Some(DockerUserIdentifier::Id(1002))
        );
    }

    #[test]
    #[cfg(unix)]
    fn reads_current_host_user_ids_from_process_identity() {
        let ids = current_host_user_ids();

        assert_eq!(ids.uid, unsafe { libc::getuid() });
        assert_eq!(ids.gid, unsafe { libc::getgid() });
    }

    #[test]
    #[cfg(not(unix))]
    fn defaults_current_host_user_ids_on_non_unix_builds() {
        assert_eq!(current_host_user_ids(), HostUserIds { uid: 0, gid: 0 });
    }

    #[test]
    fn resolves_effective_container_and_remote_users_from_all_sources() {
        let remote_only = resolve_effective_users(EffectiveUserResolveInput {
            devcontainer_remote_user: Some("remote"),
            devcontainer_container_user: None,
            image_metadata_remote_user: None,
            image_metadata_container_user: None,
            image_config_user: None,
        })
        .unwrap();
        assert_eq!(remote_only.container_user.user, "root");
        assert_eq!(
            remote_only.container_user.source,
            RemoteUserSource::RootFallback
        );
        assert_eq!(remote_only.remote_user.user, "remote");
        assert_eq!(remote_only.remote_user.source, RemoteUserSource::Explicit);
        assert_eq!(
            remote_only.remote_user.origin,
            EffectiveRemoteUserOrigin::ExplicitRemote
        );

        let container_only = resolve_effective_users(EffectiveUserResolveInput {
            devcontainer_remote_user: None,
            devcontainer_container_user: Some("container"),
            image_metadata_remote_user: None,
            image_metadata_container_user: None,
            image_config_user: None,
        })
        .unwrap();
        assert_eq!(container_only.container_user.user, "container");
        assert_eq!(container_only.remote_user.user, "container");
        assert_eq!(
            container_only.remote_user.origin,
            EffectiveRemoteUserOrigin::Container
        );

        let both = resolve_effective_users(EffectiveUserResolveInput {
            devcontainer_remote_user: Some("remote"),
            devcontainer_container_user: Some("container"),
            image_metadata_remote_user: None,
            image_metadata_container_user: None,
            image_config_user: None,
        })
        .unwrap();
        assert_eq!(both.container_user.user, "container");
        assert_eq!(both.remote_user.user, "remote");

        let image_metadata = resolve_effective_users(EffectiveUserResolveInput {
            devcontainer_remote_user: None,
            devcontainer_container_user: None,
            image_metadata_remote_user: Some("metadata-remote"),
            image_metadata_container_user: Some("metadata-container"),
            image_config_user: Some("image-user"),
        })
        .unwrap();
        assert_eq!(image_metadata.container_user.user, "metadata-container");
        assert_eq!(
            image_metadata.container_user.source,
            RemoteUserSource::ImageMetadata
        );
        assert_eq!(image_metadata.remote_user.user, "metadata-remote");
        assert_eq!(
            image_metadata.remote_user.origin,
            EffectiveRemoteUserOrigin::ImageMetadataRemote
        );

        let image_config = resolve_effective_users(EffectiveUserResolveInput {
            devcontainer_remote_user: None,
            devcontainer_container_user: None,
            image_metadata_remote_user: None,
            image_metadata_container_user: None,
            image_config_user: Some("1001:1002"),
        })
        .unwrap();
        assert_eq!(image_config.container_user.user, "1001:1002");
        assert_eq!(
            image_config.container_user.source,
            RemoteUserSource::ImageConfig
        );
        assert_eq!(image_config.remote_user.user, "1001:1002");
        assert_eq!(
            image_config.remote_user.origin,
            EffectiveRemoteUserOrigin::Container
        );

        let compose_service = resolve_effective_users_with_compose_service_user(
            EffectiveUserResolveInput {
                devcontainer_remote_user: None,
                devcontainer_container_user: None,
                image_metadata_remote_user: None,
                image_metadata_container_user: None,
                image_config_user: Some("image-user"),
            },
            Some("compose-user"),
        )
        .unwrap();
        assert_eq!(compose_service.container_user.user, "compose-user");
        assert_eq!(
            compose_service.container_user.source,
            RemoteUserSource::ComposeService
        );
        assert_eq!(compose_service.remote_user.user, "compose-user");
        assert_eq!(
            compose_service.remote_user.origin,
            EffectiveRemoteUserOrigin::Container
        );

        let root = resolve_effective_users(EffectiveUserResolveInput {
            devcontainer_remote_user: None,
            devcontainer_container_user: None,
            image_metadata_remote_user: None,
            image_metadata_container_user: None,
            image_config_user: Some(""),
        })
        .unwrap();
        assert_eq!(root.container_user.user, "root");
        assert_eq!(root.remote_user.user, "root");
    }

    #[test]
    fn decides_uid_gid_sync_target_and_noop_reasons() {
        let host = HostUserIds { uid: 501, gid: 20 };
        let remote_only = resolve_effective_users(EffectiveUserResolveInput {
            devcontainer_remote_user: Some("remote"),
            devcontainer_container_user: None,
            image_metadata_remote_user: None,
            image_metadata_container_user: None,
            image_config_user: None,
        })
        .unwrap();
        assert_eq!(
            decide_uid_gid_sync(&remote_only, true, HostPlatform::Linux, host),
            UidGidSyncDecision::Sync(UidGidSyncTarget {
                kind: UidGidSyncTargetKind::RemoteUser,
                user: "remote".to_owned(),
                host,
            })
        );

        let container_only = resolve_effective_users(EffectiveUserResolveInput {
            devcontainer_remote_user: None,
            devcontainer_container_user: Some("container"),
            image_metadata_remote_user: None,
            image_metadata_container_user: None,
            image_config_user: None,
        })
        .unwrap();
        assert_eq!(
            decide_uid_gid_sync(&container_only, true, HostPlatform::Linux, host),
            UidGidSyncDecision::Sync(UidGidSyncTarget {
                kind: UidGidSyncTargetKind::ContainerUser,
                user: "container".to_owned(),
                host,
            })
        );

        let compose_service_only = resolve_effective_users_with_compose_service_user(
            EffectiveUserResolveInput {
                devcontainer_remote_user: None,
                devcontainer_container_user: None,
                image_metadata_remote_user: None,
                image_metadata_container_user: None,
                image_config_user: None,
            },
            Some("compose-user"),
        )
        .unwrap();
        assert_eq!(
            decide_uid_gid_sync(&compose_service_only, true, HostPlatform::Linux, host),
            UidGidSyncDecision::Sync(UidGidSyncTarget {
                kind: UidGidSyncTargetKind::ContainerUser,
                user: "compose-user".to_owned(),
                host,
            })
        );

        let image_user_only = resolve_effective_users(EffectiveUserResolveInput {
            devcontainer_remote_user: None,
            devcontainer_container_user: None,
            image_metadata_remote_user: None,
            image_metadata_container_user: None,
            image_config_user: Some("image-user"),
        })
        .unwrap();
        assert_eq!(
            decide_uid_gid_sync(&image_user_only, true, HostPlatform::Linux, host),
            UidGidSyncDecision::Noop(UidGidSyncNoopReason::NoExplicitUser)
        );

        let root = resolve_effective_users(EffectiveUserResolveInput {
            devcontainer_remote_user: Some("root"),
            devcontainer_container_user: None,
            image_metadata_remote_user: None,
            image_metadata_container_user: None,
            image_config_user: None,
        })
        .unwrap();
        assert_eq!(
            decide_uid_gid_sync(&root, true, HostPlatform::Linux, host),
            UidGidSyncDecision::Noop(UidGidSyncNoopReason::Root)
        );
        assert_eq!(
            decide_uid_gid_sync(&remote_only, false, HostPlatform::Linux, host),
            UidGidSyncDecision::Noop(UidGidSyncNoopReason::Disabled)
        );
        assert_eq!(
            decide_uid_gid_sync(&remote_only, true, HostPlatform::NonLinux, host),
            UidGidSyncDecision::Noop(UidGidSyncNoopReason::NonLinuxHost)
        );
    }

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
    fn rewrites_numeric_sync_runtime_user_when_it_matches_old_target_uid() {
        let host = HostUserIds {
            uid: 1000,
            gid: 1000,
        };
        let plan = UidGidSyncPlan::Sync {
            target: UidGidSyncTarget {
                kind: UidGidSyncTargetKind::RemoteUser,
                user: "syncuser".to_owned(),
                host,
            },
            container: ResolvedUserIds {
                name: "syncuser".to_owned(),
                uid: 2001,
                gid: 2001,
            },
        };

        assert_eq!(
            uid_gid_sync_runtime_user("2001", &plan).unwrap(),
            "syncuser"
        );
        assert_eq!(
            uid_gid_sync_runtime_user("2001:2001", &plan).unwrap(),
            "syncuser:1000"
        );
        assert_eq!(
            uid_gid_sync_runtime_user("2001:shared", &plan).unwrap(),
            "syncuser:shared"
        );
        assert_eq!(
            uid_gid_sync_runtime_user("2001:3000", &plan).unwrap(),
            "syncuser:3000"
        );
        assert_eq!(
            uid_gid_sync_runtime_user("syncuser:2001", &plan).unwrap(),
            "syncuser:1000"
        );
        assert_eq!(
            uid_gid_sync_runtime_user("syncuser:3000", &plan).unwrap(),
            "syncuser:3000"
        );
        assert_eq!(
            uid_gid_sync_runtime_user("otheruser:2001", &plan).unwrap(),
            "otheruser:2001"
        );
        assert_eq!(
            uid_gid_sync_runtime_user("2002:2001", &plan).unwrap(),
            "2002:2001"
        );
        assert_eq!(
            uid_gid_sync_runtime_user("syncuser", &plan).unwrap(),
            "syncuser"
        );
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
    fn parses_passwd_record_uid_and_gid() {
        let record = parse_passwd_record("vscode:x:1001:1002::/home/vscode:/bin/sh").unwrap();

        assert_eq!(record.uid, 1001);
        assert_eq!(record.gid, 1002);
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
    fn numeric_remote_user_without_passwd_entry_resolves_ids_without_home() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let client = DockerClient::connect_from_env().unwrap();
            let image = test_image_tag("remote-user-numeric-without-passwd");
            let result = async {
                ensure_image(&client, "alpine:3.20", PullPolicy::Missing).await?;
                build_numeric_user_without_passwd_test_image(&client, &image).await?;
                let effective_users = resolve_effective_users_from_image(
                    &client,
                    &image,
                    EffectiveUserResolveInput {
                        devcontainer_remote_user: None,
                        devcontainer_container_user: None,
                        image_metadata_remote_user: None,
                        image_metadata_container_user: None,
                        image_config_user: None,
                    },
                )
                .await?;

                let user =
                    resolve_remote_user_from_image(&client, &image, &effective_users).await?;

                assert_eq!(user.user, "2004:2005");
                assert_eq!(user.uid, 2004);
                assert_eq!(user.gid, 2005);
                assert_eq!(user.home, None);

                Ok(())
            }
            .await;

            let cleanup = remove_image(&client, &image, true).await;
            result.and(cleanup).unwrap();
        });
    }

    #[test]
    fn resolves_root_user_from_root_image() {
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

                let effective_users = EffectiveUsers::root();
                let user = resolve_remote_user(
                    &client,
                    &name,
                    &effective_users,
                    &UidGidSyncPlan::default(),
                )
                .await?;

                assert_eq!(user.user, "root");
                assert_eq!(user.home.as_deref(), Some("/root"));
                assert_eq!(user.source, RemoteUserSource::RootFallback);
                assert_eq!(user.fallback_from, None);

                Ok::<_, anyhow::Error>(())
            }
            .await;

            let cleanup = remove_container(&client, &name, true, true).await;
            result.and(cleanup).unwrap();
        });
    }

    #[test]
    fn resolves_non_root_user_from_image_config() {
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

                let effective_users = resolve_effective_users_from_image(
                    &client,
                    &image,
                    EffectiveUserResolveInput {
                        devcontainer_remote_user: None,
                        devcontainer_container_user: None,
                        image_metadata_remote_user: None,
                        image_metadata_container_user: None,
                        image_config_user: None,
                    },
                )
                .await?;
                let user = resolve_remote_user(
                    &client,
                    &name,
                    &effective_users,
                    &UidGidSyncPlan::default(),
                )
                .await?;

                assert_eq!(user.user, "vscode");
                assert_eq!(user.home.as_deref(), Some("/home/vscode"));
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
    fn resolves_grouped_user_from_image_config() {
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

                let effective_users = resolve_effective_users_from_image(
                    &client,
                    &image,
                    EffectiveUserResolveInput {
                        devcontainer_remote_user: None,
                        devcontainer_container_user: None,
                        image_metadata_remote_user: None,
                        image_metadata_container_user: None,
                        image_config_user: None,
                    },
                )
                .await?;
                let user = resolve_remote_user(
                    &client,
                    &name,
                    &effective_users,
                    &UidGidSyncPlan::default(),
                )
                .await?;

                assert_eq!(user.user, "vscode:shared");
                assert_eq!(user.home.as_deref(), Some("/home/vscode"));
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
    fn errors_when_explicit_user_is_missing() {
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

                let effective_users = resolve_effective_users(EffectiveUserResolveInput {
                    devcontainer_remote_user: Some("missing-user"),
                    devcontainer_container_user: None,
                    image_metadata_remote_user: None,
                    image_metadata_container_user: None,
                    image_config_user: None,
                })?;
                let error = resolve_remote_user(
                    &client,
                    &name,
                    &effective_users,
                    &UidGidSyncPlan::default(),
                )
                .await
                .expect_err("missing explicit user must be a configuration error");
                let message = error.to_string();

                assert!(message.contains("Remote user does not exist"));
                assert!(message.contains("missing-user"));

                Ok(())
            }
            .await;

            let cleanup = remove_container(&client, &name, true, true).await;
            result.and(cleanup).unwrap();
        });
    }

    #[test]
    fn resolves_configured_remote_user_without_image_config_inspect_when_image_tag_is_missing() {
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

                let explicit_users = resolve_effective_users(EffectiveUserResolveInput {
                    devcontainer_remote_user: Some("vscode"),
                    devcontainer_container_user: None,
                    image_metadata_remote_user: None,
                    image_metadata_container_user: None,
                    image_config_user: None,
                })?;
                let metadata_users = resolve_effective_users(EffectiveUserResolveInput {
                    devcontainer_remote_user: None,
                    devcontainer_container_user: None,
                    image_metadata_remote_user: Some("vscode"),
                    image_metadata_container_user: None,
                    image_config_user: None,
                })?;
                let explicit = resolve_remote_user(
                    &client,
                    &name,
                    &explicit_users,
                    &UidGidSyncPlan::default(),
                )
                .await?;
                let metadata = resolve_remote_user(
                    &client,
                    &name,
                    &metadata_users,
                    &UidGidSyncPlan::default(),
                )
                .await?;

                assert_eq!(explicit.user, "vscode");
                assert_eq!(explicit.home.as_deref(), Some("/home/vscode"));
                assert_eq!(explicit.source, RemoteUserSource::Explicit);
                assert_eq!(explicit.fallback_from, None);
                assert_eq!(metadata.user, "vscode");
                assert_eq!(metadata.home.as_deref(), Some("/home/vscode"));
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

    #[test]
    fn creates_uid_gid_sync_plan_for_remote_container_and_different_users() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let client = DockerClient::connect_from_env().unwrap();
            let image = test_image_tag("uid-gid-sync-users");
            let result = async {
                ensure_image(&client, "alpine:3.20", PullPolicy::Missing).await?;
                build_uid_gid_sync_users_test_image(&client, &image).await?;
                let host = HostUserIds { uid: 501, gid: 20 };

                let remote_only = resolve_effective_users_from_image(
                    &client,
                    &image,
                    EffectiveUserResolveInput {
                        devcontainer_remote_user: Some("remote"),
                        devcontainer_container_user: None,
                        image_metadata_remote_user: None,
                        image_metadata_container_user: None,
                        image_config_user: None,
                    },
                )
                .await?;
                assert_eq!(remote_only.container_user.user, "root");
                assert_eq!(remote_only.remote_user.user, "remote");
                assert_sync_plan(
                    resolve_uid_gid_sync_plan_from_image(
                        &client,
                        &image,
                        &remote_only,
                        true,
                        HostPlatform::Linux,
                        host,
                    )
                    .await?,
                    UidGidSyncTargetKind::RemoteUser,
                    "remote",
                    2001,
                    2001,
                    host,
                );

                let container_only = resolve_effective_users_from_image(
                    &client,
                    &image,
                    EffectiveUserResolveInput {
                        devcontainer_remote_user: None,
                        devcontainer_container_user: Some("container"),
                        image_metadata_remote_user: None,
                        image_metadata_container_user: None,
                        image_config_user: None,
                    },
                )
                .await?;
                assert_eq!(container_only.container_user.user, "container");
                assert_eq!(container_only.remote_user.user, "container");
                assert_sync_plan(
                    resolve_uid_gid_sync_plan_from_image(
                        &client,
                        &image,
                        &container_only,
                        true,
                        HostPlatform::Linux,
                        host,
                    )
                    .await?,
                    UidGidSyncTargetKind::ContainerUser,
                    "container",
                    2002,
                    2002,
                    host,
                );

                let both = resolve_effective_users_from_image(
                    &client,
                    &image,
                    EffectiveUserResolveInput {
                        devcontainer_remote_user: Some("remote"),
                        devcontainer_container_user: Some("container"),
                        image_metadata_remote_user: None,
                        image_metadata_container_user: None,
                        image_config_user: None,
                    },
                )
                .await?;
                assert_eq!(both.container_user.user, "container");
                assert_eq!(both.remote_user.user, "remote");
                assert_sync_plan(
                    resolve_uid_gid_sync_plan_from_image(
                        &client,
                        &image,
                        &both,
                        true,
                        HostPlatform::Linux,
                        host,
                    )
                    .await?,
                    UidGidSyncTargetKind::RemoteUser,
                    "remote",
                    2001,
                    2001,
                    host,
                );

                Ok(())
            }
            .await;

            let cleanup = remove_image(&client, &image, true).await;
            result.and(cleanup).unwrap();
        });
    }

    #[test]
    fn uid_gid_sync_plan_noops_for_docker_user_only_and_numeric_user() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let client = DockerClient::connect_from_env().unwrap();
            let named_image = test_image_tag("uid-gid-sync-image-user-only");
            let numeric_image = test_image_tag("uid-gid-sync-numeric-user");
            let result = async {
                ensure_image(&client, "alpine:3.20", PullPolicy::Missing).await?;
                build_non_root_test_image(&client, &named_image).await?;
                build_numeric_user_test_image(&client, &numeric_image).await?;
                let host = HostUserIds { uid: 501, gid: 20 };

                let named_users = resolve_effective_users_from_image(
                    &client,
                    &named_image,
                    EffectiveUserResolveInput {
                        devcontainer_remote_user: None,
                        devcontainer_container_user: None,
                        image_metadata_remote_user: None,
                        image_metadata_container_user: None,
                        image_config_user: None,
                    },
                )
                .await?;
                assert_eq!(named_users.remote_user.user, "vscode");
                assert_eq!(
                    resolve_uid_gid_sync_plan_from_image(
                        &client,
                        &named_image,
                        &named_users,
                        true,
                        HostPlatform::Linux,
                        host,
                    )
                    .await?,
                    UidGidSyncPlan::Noop {
                        reason: UidGidSyncNoopReason::NoExplicitUser
                    }
                );

                let numeric_users = resolve_effective_users_from_image(
                    &client,
                    &numeric_image,
                    EffectiveUserResolveInput {
                        devcontainer_remote_user: None,
                        devcontainer_container_user: None,
                        image_metadata_remote_user: None,
                        image_metadata_container_user: None,
                        image_config_user: None,
                    },
                )
                .await?;
                assert_eq!(numeric_users.remote_user.user, "2003:2003");
                let remote =
                    resolve_remote_user_from_image(&client, &numeric_image, &numeric_users).await?;
                assert_eq!(remote.uid, 2003);
                assert_eq!(remote.gid, 2003);
                assert_eq!(
                    resolve_uid_gid_sync_plan_from_image(
                        &client,
                        &numeric_image,
                        &numeric_users,
                        true,
                        HostPlatform::Linux,
                        host,
                    )
                    .await?,
                    UidGidSyncPlan::Noop {
                        reason: UidGidSyncNoopReason::NoExplicitUser
                    }
                );

                Ok(())
            }
            .await;

            let named_cleanup = remove_image(&client, &named_image, true).await;
            let numeric_cleanup = remove_image(&client, &numeric_image, true).await;
            result.and(named_cleanup).and(numeric_cleanup).unwrap();
        });
    }

    #[test]
    fn uid_gid_sync_plan_noops_for_numeric_target_without_passwd_entry() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let client = DockerClient::connect_from_env().unwrap();
            let image = test_image_tag("uid-gid-sync-numeric-without-passwd");
            let result = async {
                ensure_image(&client, "alpine:3.20", PullPolicy::Missing).await?;
                build_numeric_user_without_passwd_test_image(&client, &image).await?;
                let host = HostUserIds { uid: 501, gid: 20 };
                let users = resolve_effective_users_from_image(
                    &client,
                    &image,
                    EffectiveUserResolveInput {
                        devcontainer_remote_user: Some("2004:2005"),
                        devcontainer_container_user: None,
                        image_metadata_remote_user: None,
                        image_metadata_container_user: None,
                        image_config_user: None,
                    },
                )
                .await?;

                assert_eq!(
                    resolve_uid_gid_sync_plan_from_image(
                        &client,
                        &image,
                        &users,
                        true,
                        HostPlatform::Linux,
                        host,
                    )
                    .await?,
                    UidGidSyncPlan::Noop {
                        reason: UidGidSyncNoopReason::NumericUserWithoutPasswd
                    }
                );

                Ok(())
            }
            .await;

            let cleanup = remove_image(&client, &image, true).await;
            result.and(cleanup).unwrap();
        });
    }

    #[test]
    fn uid_gid_sync_plan_errors_when_explicit_target_user_is_missing() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let client = DockerClient::connect_from_env().unwrap();
            let result = async {
                ensure_image(&client, "alpine:3.20", PullPolicy::Missing).await?;
                let users = resolve_effective_users_from_image(
                    &client,
                    "alpine:3.20",
                    EffectiveUserResolveInput {
                        devcontainer_remote_user: Some("missing-user"),
                        devcontainer_container_user: None,
                        image_metadata_remote_user: None,
                        image_metadata_container_user: None,
                        image_config_user: None,
                    },
                )
                .await?;
                let error = resolve_uid_gid_sync_plan_from_image(
                    &client,
                    "alpine:3.20",
                    &users,
                    true,
                    HostPlatform::Linux,
                    HostUserIds { uid: 501, gid: 20 },
                )
                .await
                .expect_err("missing explicit sync target user must be an error");
                let message = error.to_string();

                assert!(message.contains("UID/GID sync user does not exist"));
                assert!(message.contains("missing-user"));

                Ok::<_, anyhow::Error>(())
            }
            .await;

            result.unwrap();
        });
    }

    async fn build_non_root_test_image(client: &DockerClient, image: &str) -> Result<()> {
        let context = tempfile::tempdir()?;
        let dockerfile_path = context.path().join("Dockerfile");
        std::fs::write(
            &dockerfile_path,
            format!(
                r#"
            FROM alpine:3.20
            LABEL decune.test.image="{image}"
            RUN adduser -D vscode
            USER vscode
            "#
            ),
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
            format!(
                r#"
            FROM alpine:3.20
            LABEL decune.test.image="{image}"
            RUN adduser -D vscode && addgroup -S shared && addgroup vscode shared
            USER vscode:shared
            "#
            ),
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

    async fn build_uid_gid_sync_users_test_image(client: &DockerClient, image: &str) -> Result<()> {
        let context = tempfile::tempdir()?;
        let dockerfile_path = context.path().join("Dockerfile");
        std::fs::write(
            &dockerfile_path,
            format!(
                r#"
            FROM alpine:3.20
            LABEL decune.test.image="{image}"
            RUN addgroup -g 2001 remote \
              && adduser -D -u 2001 -G remote remote \
              && addgroup -g 2002 container \
              && adduser -D -u 2002 -G container container
            "#
            ),
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

    async fn build_numeric_user_test_image(client: &DockerClient, image: &str) -> Result<()> {
        let context = tempfile::tempdir()?;
        let dockerfile_path = context.path().join("Dockerfile");
        std::fs::write(
            &dockerfile_path,
            format!(
                r#"
            FROM alpine:3.20
            LABEL decune.test.image="{image}"
            RUN addgroup -g 2003 numeric && adduser -D -u 2003 -G numeric numeric
            USER 2003:2003
            "#
            ),
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

    async fn build_numeric_user_without_passwd_test_image(
        client: &DockerClient,
        image: &str,
    ) -> Result<()> {
        let context = tempfile::tempdir()?;
        let dockerfile_path = context.path().join("Dockerfile");
        std::fs::write(
            &dockerfile_path,
            format!(
                r#"
            FROM alpine:3.20
            LABEL decune.test.image="{image}"
            USER 2004:2005
            "#
            ),
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

    fn assert_sync_plan(
        plan: UidGidSyncPlan,
        expected_kind: UidGidSyncTargetKind,
        expected_user: &str,
        expected_uid: u32,
        expected_gid: u32,
        expected_host: HostUserIds,
    ) {
        match plan {
            UidGidSyncPlan::Sync { target, container } => {
                assert_eq!(target.kind, expected_kind);
                assert_eq!(target.user, expected_user);
                assert_eq!(target.host, expected_host);
                assert_eq!(container.uid, expected_uid);
                assert_eq!(container.gid, expected_gid);
            }
            UidGidSyncPlan::Noop { reason } => {
                panic!("expected sync plan, got no-op: {reason:?}");
            }
        }
    }

    async fn create_running_user_test_container(
        client: &DockerClient,
        name: &str,
        image: &str,
    ) -> Result<()> {
        remove_container(client, name, true, true).await?;

        let spec = ContainerCreateSpec {
            image: image.to_owned(),
            name: name.to_owned(),
            entrypoint: Some(vec!["/bin/sh".to_owned()]),
            command: Some(vec![
                "-c".to_owned(),
                "trap 'exit 0' TERM\nwhile sleep 1 & wait $!; do :; done".to_owned(),
            ]),
            labels: BTreeMap::new(),
            env: BTreeMap::new(),
            working_dir: None,
            user: None,
            mounts: Vec::new(),
            publish_ports: Vec::new(),
            host_config: ContainerHostConfig::default(),
        };

        create_container(client, &spec).await?;
        start_container(client, name).await?;

        Ok(())
    }

    fn test_container_name(suffix: &str) -> String {
        format!("decune-{suffix}-{}", std::process::id())
    }

    fn test_image_tag(suffix: &str) -> String {
        format!("decune/{suffix}-{}:test", std::process::id())
    }
}
