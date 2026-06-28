use std::{collections::BTreeMap, fs, ops::Deref};

use crate::{
    config::{
        ConfigHashInput, ConfigLayer, ConfigMergeInput, config_hash,
        layer::{LayerDevcontainerCompose, LayerDevcontainerMetadata, LayerDevcontainerSource},
        resolved::{ResolvedConfig, ResolvedDevcontainerSource},
        types::{MountType, PortProtocol},
    },
    docker::{
        build::{DockerBuildInput, DockerBuildOptions, ResolvedBuildContext, build_image},
        client::DockerClient,
        container::ContainerMount,
        mounts::DockerMountSpec,
        ports::ResolvedForwardPort,
        resource::DockerResources,
        user::{
            EffectiveUsers, HostUserIds, ResolvedUserIds, UidGidSyncPlan, UidGidSyncTarget,
            UidGidSyncTargetKind,
        },
    },
    up::{UpMountSummary, UpPlan, mount_hash_inputs},
    workspace::Workspace,
};

pub(crate) struct TestWorkspace {
    _directory: tempfile::TempDir,
    workspace: Workspace,
}

impl Deref for TestWorkspace {
    type Target = Workspace;

    fn deref(&self) -> &Self::Target {
        &self.workspace
    }
}

pub(crate) fn test_workspace(name: &str) -> TestWorkspace {
    let directory = tempfile::Builder::new()
        .prefix(&format!("decune-up-test-{name}-"))
        .tempdir()
        .unwrap();
    let root = directory.path().join(name);
    fs::create_dir_all(&root).unwrap();
    let workspace = Workspace::resolve(&root).unwrap();
    TestWorkspace {
        _directory: directory,
        workspace,
    }
}

pub(crate) fn write_devcontainer(workspace: &Workspace, contents: &str) {
    let path = workspace.root().join(".devcontainer/devcontainer.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

pub(crate) fn test_mount() -> DockerMountSpec {
    DockerMountSpec {
        source: Some("/host/cache".to_owned()),
        target: "/cache".to_owned(),
        mount_type: MountType::Bind,
        read_only: false,
        consistency: None,
        bind_options: None,
        volume_options: None,
    }
}

pub(crate) fn test_volume_mount() -> DockerMountSpec {
    DockerMountSpec {
        source: Some("project-cache".to_owned()),
        target: "/cache".to_owned(),
        mount_type: MountType::Volume,
        read_only: false,
        consistency: None,
        bind_options: None,
        volume_options: None,
    }
}

pub(crate) fn config_hash_for_mount(mount: DockerMountSpec) -> String {
    let config = ResolvedConfig::default();
    let mut input = ConfigHashInput::new(&config);
    input.resolved_mounts = mount_hash_inputs(&[mount]);

    config_hash(&input)
}

pub(crate) async fn build_user_image(
    client: &DockerClient,
    image: &str,
    user: &str,
) -> anyhow::Result<()> {
    build_uid_gid_user_image(client, image, user, 2001, 2001).await
}

pub(crate) async fn build_uid_gid_user_image(
    client: &DockerClient,
    image: &str,
    user: &str,
    uid: u32,
    gid: u32,
) -> anyhow::Result<()> {
    let context = tempfile::Builder::new()
        .prefix("decune-up-user-image-")
        .tempdir()
        .unwrap();
    let dockerfile_path = context.path().join("Dockerfile");
    fs::write(
        &dockerfile_path,
        format!(
            "FROM alpine:3.20\nRUN addgroup -g {gid} {user} && adduser -D -u {uid} -G {user} -h /home/{user} {user}\nUSER {user}\n"
        ),
    )
    .unwrap();
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

pub(crate) async fn build_distinct_uid_gid_users_image(
    client: &DockerClient,
    image: &str,
) -> anyhow::Result<()> {
    let context = tempfile::Builder::new()
        .prefix("decune-up-distinct-users-image-")
        .tempdir()
        .unwrap();
    let dockerfile_path = context.path().join("Dockerfile");
    fs::write(
        &dockerfile_path,
        "FROM alpine:3.20\nRUN addgroup -g 2001 remoteuser && adduser -D -u 2001 -G remoteuser -h /home/remoteuser remoteuser && addgroup -g 2002 containeruser && adduser -D -u 2002 -G containeruser -h /home/containeruser containeruser\nUSER containeruser\n",
    )
    .unwrap();
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

pub(crate) async fn build_numeric_uid_gid_user_image(
    client: &DockerClient,
    image: &str,
) -> anyhow::Result<()> {
    let context = tempfile::Builder::new()
        .prefix("decune-up-numeric-user-image-")
        .tempdir()
        .unwrap();
    let dockerfile_path = context.path().join("Dockerfile");
    fs::write(
        &dockerfile_path,
        "FROM alpine:3.20\nRUN addgroup -g 2001 syncuser && adduser -D -u 2001 -G syncuser -h /home/syncuser syncuser\nUSER 2001:2001\n",
    )
    .unwrap();
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

pub(crate) async fn build_named_uid_numeric_gid_user_image(
    client: &DockerClient,
    image: &str,
) -> anyhow::Result<()> {
    let context = tempfile::Builder::new()
        .prefix("decune-up-named-user-numeric-group-image-")
        .tempdir()
        .unwrap();
    let dockerfile_path = context.path().join("Dockerfile");
    fs::write(
        &dockerfile_path,
        "FROM alpine:3.20\nRUN addgroup -g 2001 syncuser && adduser -D -u 2001 -G syncuser -h /home/syncuser syncuser\nUSER syncuser:2001\n",
    )
    .unwrap();
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

pub(crate) async fn build_uid_gid_conflict_user_image(
    client: &DockerClient,
    image: &str,
    conflict_user_id: u32,
    conflict_group_id: u32,
) -> anyhow::Result<()> {
    let context = tempfile::Builder::new()
        .prefix("decune-up-user-image-")
        .tempdir()
        .unwrap();
    let dockerfile_path = context.path().join("Dockerfile");
    fs::write(
        &dockerfile_path,
        format!(
            "FROM alpine:3.20\nRUN addgroup -g {conflict_group_id} conflictuser && adduser -D -u {conflict_user_id} -G conflictuser -h /home/conflictuser conflictuser && addgroup -g 2001 syncuser && adduser -D -u 2001 -G syncuser -h /home/syncuser syncuser\nUSER syncuser\n"
        ),
    )
    .unwrap();
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

pub(crate) async fn build_duplicate_matching_host_ids_image(
    client: &DockerClient,
    image: &str,
    host_user_id: u32,
    host_group_id: u32,
) -> anyhow::Result<()> {
    let context = tempfile::Builder::new()
        .prefix("decune-up-duplicate-matching-ids-image-")
        .tempdir()
        .unwrap();
    let dockerfile_path = context.path().join("Dockerfile");
    fs::write(
        &dockerfile_path,
        format!(
            "FROM alpine:3.20\nRUN addgroup -g {host_group_id} syncgroup && adduser -D -u {host_user_id} -G syncgroup -h /home/syncuser syncuser && echo 'other:x:{host_user_id}:{host_group_id}::/home/other:/bin/sh' >> /etc/passwd && echo 'othergroup:x:{host_group_id}:' >> /etc/group\nUSER syncuser\n"
        ),
    )
    .unwrap();
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

pub(crate) async fn build_missing_target_group_conflict_image(
    client: &DockerClient,
    image: &str,
    conflict_gid: u32,
) -> anyhow::Result<()> {
    let context = tempfile::Builder::new()
        .prefix("decune-up-missing-group-image-")
        .tempdir()
        .unwrap();
    let dockerfile_path = context.path().join("Dockerfile");
    fs::write(
        &dockerfile_path,
        format!(
            "FROM alpine:3.20\nRUN addgroup -g {conflict_gid} conflictgroup && addgroup -g 2001 syncuser && adduser -D -u 2001 -G syncuser -h /home/syncuser syncuser && awk -F: '$3 != 2001 {{ print }}' /etc/group >/tmp/group && cat /tmp/group >/etc/group\nUSER syncuser\n"
        ),
    )
    .unwrap();
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

pub(crate) async fn build_duplicate_old_gid_image(
    client: &DockerClient,
    image: &str,
) -> anyhow::Result<()> {
    let context = tempfile::Builder::new()
        .prefix("decune-up-duplicate-old-gid-image-")
        .tempdir()
        .unwrap();
    let dockerfile_path = context.path().join("Dockerfile");
    fs::write(
        &dockerfile_path,
        "FROM alpine:3.20\nRUN addgroup -g 2001 syncgroup && adduser -D -u 2001 -G syncgroup -h /home/syncuser syncuser && { echo 'othergroup:x:2001:'; cat /etc/group; } >/tmp/group && cat /tmp/group >/etc/group\nUSER syncuser\n",
    )
    .unwrap();
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

pub(crate) fn mount_summary(source: Option<&str>, target: &str) -> UpMountSummary {
    mount_summary_with_type(source, target, MountType::Bind)
}

pub(crate) fn mount_summary_with_type(
    source: Option<&str>,
    target: &str,
    mount_type: MountType,
) -> UpMountSummary {
    mount_summary_with_type_and_read_only(source, target, mount_type, false)
}

pub(crate) fn mount_summary_with_type_and_read_only(
    source: Option<&str>,
    target: &str,
    mount_type: MountType,
    read_only: bool,
) -> UpMountSummary {
    UpMountSummary {
        source: source.map(ToOwned::to_owned),
        target: target.to_owned(),
        mount_type,
        read_only,
    }
}

pub(crate) fn test_up_plan_with_config(config: ResolvedConfig) -> UpPlan {
    UpPlan {
        image: "alpine:3.20".to_owned(),
        base_image: "alpine:3.20".to_owned(),
        build_context: None,
        build_options: crate::docker::build::DockerBuildOptions::default(),
        feature_install: None,
        feature_build_context_dir: None,
        uid_gid_sync_build_context_dir: None,
        resources: DockerResources {
            container_name: "decune-test".to_owned(),
            image_tag: "decune/test:stable-hash".to_owned(),
            workspace_volume_name: "decune-test-workspace".to_owned(),
            labels: BTreeMap::new(),
            config_hash: "stable-hash".to_owned(),
        },
        pre_uid_gid_sync_resources: None,
        compose_project: None,
        config_layers: ConfigMergeInput::default(),
        config,
        sensitive_container_env: crate::config::variables::SensitiveEnvMap::default(),
        sensitive_build_args: crate::config::variables::SensitiveEnvMap::default(),
        compose_interpolation_env: BTreeMap::default(),
        compose_interpolation_redactions: Vec::new(),
        effective_users: EffectiveUsers::root(),
        uid_gid_sync_plan: UidGidSyncPlan::default(),
        workspace_folder: "/workspaces/project".to_owned(),
        mounts: Vec::new(),
        dotfile_skeletons: Vec::new(),
        forward_ports: Vec::new(),
        ignored_detached_forwarding: false,
    }
}

pub(crate) fn compose_config(primary_service: &str) -> ResolvedConfig {
    let mut config = ResolvedConfig::default();
    config.devcontainer.source = Some(ResolvedDevcontainerSource::Compose(
        LayerDevcontainerCompose {
            files: vec!["compose.yml".to_owned()],
            service: primary_service.to_owned(),
            run_services: None,
        },
    ));
    config
}

pub(crate) fn forward_port_for_service(
    service: Option<&str>,
    container: u16,
) -> ResolvedForwardPort {
    ResolvedForwardPort {
        service: service.map(str::to_owned),
        container,
        requested_host: container,
        host: container,
        host_ip: "127.0.0.1".to_owned(),
        protocol: PortProtocol::Tcp,
        require_local: false,
        label: None,
    }
}

pub(crate) fn sync_plan() -> UidGidSyncPlan {
    UidGidSyncPlan::Sync {
        target: UidGidSyncTarget {
            kind: UidGidSyncTargetKind::RemoteUser,
            user: "vscode".to_owned(),
            host: HostUserIds {
                uid: 1000,
                gid: 1000,
            },
        },
        container: ResolvedUserIds {
            name: "vscode".to_owned(),
            uid: 2001,
            gid: 2001,
        },
    }
}

pub(crate) fn test_resources(config_hash: &str) -> DockerResources {
    DockerResources {
        container_name: "decune-test".to_owned(),
        image_tag: format!("decune/test:{config_hash}"),
        workspace_volume_name: "decune-test-workspace".to_owned(),
        labels: BTreeMap::from([
            ("decune.workspace_id".to_owned(), "workspace-id".to_owned()),
            ("decune.config_hash".to_owned(), config_hash.to_owned()),
        ]),
        config_hash: config_hash.to_owned(),
    }
}

pub(crate) fn test_up_plan_with_image_source(image: &str) -> UpPlan {
    let mut config = ResolvedConfig::default();
    config.devcontainer.source = Some(ResolvedDevcontainerSource::Image(image.to_owned()));
    let mut plan = test_up_plan_with_config(config);
    plan.image = image.to_owned();
    plan.base_image = image.to_owned();
    plan.config_layers.devcontainer = Some(ConfigLayer {
        devcontainer: Some(LayerDevcontainerMetadata {
            source: Some(LayerDevcontainerSource::Image(image.to_owned())),
            ..LayerDevcontainerMetadata::default()
        }),
        ..ConfigLayer::default()
    });
    plan
}

pub(crate) fn container_has_mount_target(
    mounts: Option<&Vec<ContainerMount>>,
    target: &str,
) -> bool {
    mounts.is_some_and(|mounts| {
        mounts
            .iter()
            .any(|mount| mount.destination.as_deref() == Some(target))
    })
}
