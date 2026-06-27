use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result};
use serde_json::Value as JsonValue;

use crate::{
    config::{
        ComposeGeneratedOverrideHashInput, ConfigHashInput, ConfigLayer, StartupCommandHashInput,
        canonical::{CanonicalWriter, sha256_hex},
        config_hash,
        layer::LayerFeature,
        resolve_config,
        resolved::{ResolvedConfig, ResolvedDevcontainerSource, ResolvedPortAttributes},
        types::{GitHttpsMode, GithubCredentialsMode, MountType, SshAgentMode},
        variables::expand_container_env_tracked,
    },
    devcontainer::features::{prepare_feature_install_plan, remove_feature_lock_file},
    docker::{
        build::build_hash_input,
        client::DockerClient,
        container::{ContainerCreateSpec, ContainerHostConfig, create_container, start_container},
        image::{
            ImageStartupCommand, LocalImagePresence, PullPolicy, ensure_image,
            image_devcontainer_metadata_layers_if_present_with_forward_ports,
            image_devcontainer_metadata_layers_with_forward_ports, image_startup_command,
            local_image_presence, remove_image, tag_image,
        },
        mounts::{DockerMountSpec, devcontainer_mount_type},
        resource::DockerResources,
        user::{
            EffectiveUserResolveInput, HostPlatform, current_host_user_ids, image_config_user,
            resolve_effective_users_from_image, resolve_effective_users_with_compose_service_user,
            resolve_remote_user_from_image, resolve_uid_gid_sync_plan_from_image,
        },
    },
    host::credentials::host_github_auth_token_available,
    runtime::{
        compose_cli::ComposeConfigService,
        compose_ports::{
            ComposePublishedPortDiagnostic, ComposePublishedPortOverride, ComposePublishedPortPlan,
            ComposePublishedPortPlanningInput, ComposePublishedPortReservation,
            compose_published_port_override, plan_compose_published_ports_with_existing_project,
        },
    },
    ui,
    up::{
        build::{
            build_feature_layer_image, build_workspace_image_layers,
            plan_requires_final_image_layer, plan_requires_workspace_layer,
            prepare_base_image_for_plan,
        },
        mounts::{
            WorkspaceLocationValidation, mount_variable_context, resolve_workspace_location,
            workspace_mount_plan_from_resolved,
        },
        plan::{
            add_internal_hash_versions, base_image_source,
            build_up_plan_with_forwarding_resolution,
            build_up_plan_with_image_metadata_and_forwarding_resolution,
            expand_runtime_devcontainer_fields, expand_static_plan_fields,
            feature_lock_hash_inputs, final_image_source,
            rebuild_up_plan_with_image_metadata_layers,
        },
        start::wait_for_container_exit_code,
        types::{ForwardingResolution, MountResolution, UpPlan, UpPlanResolution},
        uid_gid::{
            effective_user_input_from_plan, effective_users_depend_on_image_config_user,
            plan_requires_uid_gid_sync_layer, uid_gid_sync_hash_input,
            uid_gid_sync_plan_requires_layer, uid_gid_sync_warning,
        },
    },
    workspace::Workspace,
};

const GITHUB_CLI_FEATURE_REF: &str = "ghcr.io/devcontainers/features/github-cli:1";
const GITHUB_CLI_FEATURE_CANONICAL_ID: &str = "ghcr.io/devcontainers/features/github-cli";
static IMAGE_COMMAND_PROBE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

mod compose_override_hash;
mod feature;
pub(in crate::up) mod finalize;
pub(in crate::up) mod github_cli;
mod image;
pub(in crate::up) mod messages;
mod resources;
mod startup_command;

pub(in crate::up) use finalize::{
    ComposePublishedPortFinalization, FinalizeUpPlanMountsOptions, finalize_up_plan_mounts,
};
pub(in crate::up) use image::{
    build_existing_container_decision_plan, existing_remote_user_image_for_decision,
    prepare_compose_image_metadata, prepare_image_based_metadata,
};
pub(in crate::up) use messages::report_deferred_config_messages;
pub(in crate::up) use startup_command::effective_startup_command;

use compose_override_hash::compose_generated_override_hash_input;
use feature::prepare_feature_metadata_for_plan;
use github_cli::{ImageLookupPreparation, maybe_auto_add_github_cli_feature_to_plan};
use image::dockerfile_image_metadata_for_plan;
#[cfg(test)]
use resources::finalized_compose_published_ports;
use resources::{finalize_mounts_and_resources_for_plan, resolve_effective_users_for_image};
use startup_command::startup_command_hash_input;

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::{
        config::{
            ConfigHashInput, ConfigLayer, ConfigMergeInput, config_hash,
            layer::{LayerDevcontainerCompose, LayerRunArg},
            types::PortProtocol,
        },
        docker::{
            build::DockerBuildOptions,
            ports::ResolvedForwardPort,
            resource::DockerResources,
            user::{EffectiveUsers, UidGidSyncPlan},
        },
        runtime::compose_ports::{
            classify_compose_published_ports, compose_published_port_planning_input,
        },
        up::{plan::build_up_plan, types::UpPlan},
    };

    struct EnvVarGuard {
        name: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn capture(name: &'static str) -> Self {
            Self {
                name,
                previous: std::env::var_os(name),
            }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => unsafe {
                    std::env::set_var(self.name, value);
                },
                None => unsafe {
                    std::env::remove_var(self.name);
                },
            }
        }
    }

    #[test]
    fn effective_startup_command_uses_compose_overrides() {
        let image_startup = ImageStartupCommand {
            entrypoint: vec!["/image-entrypoint.sh".to_owned()],
            command: vec!["image-cmd".to_owned()],
        };
        let service = ComposeConfigService {
            entrypoint: Some(vec!["/service-entrypoint.sh".to_owned()]),
            command: Some(vec!["service-cmd".to_owned()]),
            ..ComposeConfigService::default()
        };

        let startup = effective_startup_command(image_startup, Some(&service));

        assert_eq!(
            startup.entrypoint,
            vec!["/service-entrypoint.sh".to_owned()]
        );
        assert_eq!(startup.command, vec!["service-cmd".to_owned()]);
    }

    #[test]
    fn feature_metadata_refresh_preserves_static_expansion() {
        let env_name = "DECUNE_TEST_FEATURE_STATIC_BUILD_ARG";
        let _guard = EnvVarGuard::capture(env_name);
        unsafe {
            std::env::set_var(env_name, "first-secret");
        }
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Feature Static");
        fs::create_dir(&root).unwrap();
        let workspace = Workspace::resolve(&root).unwrap();
        let devcontainer_dir = root.join(".devcontainer");
        fs::create_dir_all(devcontainer_dir.join("features/noop")).unwrap();
        fs::write(
            devcontainer_dir.join("Dockerfile"),
            "FROM alpine\nARG TOKEN\n",
        )
        .unwrap();
        fs::write(
            devcontainer_dir.join("devcontainer.json"),
            format!(
                r#"
                {{
                  "build": {{
                    "dockerfile": "Dockerfile",
                    "args": {{
                      "TOKEN": "${{localEnv:{env_name}}}"
                    }},
                    "target": "stage-${{localWorkspaceFolderBasename}}",
                    "cacheFrom": "type=registry,ref=example.test/${{localWorkspaceFolderBasename}}:cache"
                  }},
                  "features": {{
                    "./features/noop": {{}}
                  }},
                  "runArgs": [
                    "--add-host", "api.${{localWorkspaceFolderBasename}}:127.0.0.1",
                    "--dns=dns-${{localWorkspaceFolderBasename}}"
                  ]
                }}
                "#
            ),
        )
        .unwrap();
        fs::write(
            devcontainer_dir.join("features/noop/devcontainer-feature.json"),
            r#"{"id":"noop","version":"1.0.0","name":"Noop"}"#,
        )
        .unwrap();
        fs::write(
            devcontainer_dir.join("features/noop/install.sh"),
            "set -eu\n",
        )
        .unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let first = runtime.block_on(prepared_feature_plan(&workspace));
        assert_eq!(
            first
                .build_options
                .build_args
                .get("TOKEN")
                .map(String::as_str),
            Some("first-secret")
        );
        assert_eq!(
            first.build_options.target.as_deref(),
            Some("stage-Feature Static")
        );
        assert_eq!(
            first.build_options.cache_from,
            vec!["type=registry,ref=example.test/Feature Static:cache"]
        );
        assert!(first.sensitive_build_args.contains_key("TOKEN"));
        assert!(
            first
                .build_options
                .build_arg_redactions
                .iter()
                .any(|value| value == "first-secret")
        );
        assert_eq!(
            first.config.devcontainer.run_args,
            vec![
                LayerRunArg::AddHost("api.Feature Static:127.0.0.1".to_owned()),
                LayerRunArg::Dns("dns-Feature Static".to_owned()),
            ]
        );

        unsafe {
            std::env::set_var(env_name, "second-secret");
        }
        let second = runtime.block_on(prepared_feature_plan(&workspace));

        assert_eq!(
            second
                .build_options
                .build_args
                .get("TOKEN")
                .map(String::as_str),
            Some("second-secret")
        );
        assert_ne!(
            config_hash_for_static_build_args(&first),
            config_hash_for_static_build_args(&second)
        );
    }

    #[test]
    fn effective_startup_command_falls_back_to_image_parts_independently() {
        let image_startup = ImageStartupCommand {
            entrypoint: vec!["/image-entrypoint.sh".to_owned()],
            command: vec!["image-cmd".to_owned()],
        };
        let service = ComposeConfigService {
            command: Some(vec!["service-cmd".to_owned()]),
            ..ComposeConfigService::default()
        };

        let startup = effective_startup_command(image_startup, Some(&service));

        assert_eq!(startup.entrypoint, vec!["/image-entrypoint.sh".to_owned()]);
        assert_eq!(startup.command, vec!["service-cmd".to_owned()]);
    }

    #[test]
    fn generated_override_semantic_hash_changes_for_meaningful_override_change() {
        let first = compose_generated_override_hash_input(
            PathBuf::from("/state/compose.override.yaml"),
            &compose_hash_plan("stable-hash", "decune/test:first", "1.0.0"),
            &[],
            None,
            &ComposePublishedPortOverride::default(),
        )
        .unwrap();
        let second = compose_generated_override_hash_input(
            PathBuf::from("/state/compose.override.yaml"),
            &compose_hash_plan("stable-hash", "decune/test:first", "1.0.1"),
            &[],
            None,
            &ComposePublishedPortOverride::default(),
        )
        .unwrap();

        assert_ne!(first.content_hash, second.content_hash);
    }

    #[test]
    fn generated_override_semantic_hash_changes_for_published_port_override() {
        let plan = compose_hash_plan("stable-hash", "decune/test:first", "1.0.0");
        let baseline = compose_generated_override_hash_input(
            PathBuf::from("/state/compose.override.yaml"),
            &plan,
            &[],
            None,
            &ComposePublishedPortOverride::default(),
        )
        .unwrap();
        let relocated = ComposePublishedPortOverride::from_service_ports(BTreeMap::from([(
            "app".to_owned(),
            vec![BTreeMap::from([
                ("published".to_owned(), serde_json::json!("3001")),
                ("target".to_owned(), serde_json::json!(3000)),
            ])],
        )]));

        let changed = compose_generated_override_hash_input(
            PathBuf::from("/state/compose.override.yaml"),
            &plan,
            &[],
            None,
            &relocated,
        )
        .unwrap();

        assert_ne!(baseline.content_hash, changed.content_hash);
    }

    #[test]
    fn finalized_compose_published_ports_reserves_final_forward_ports() {
        let mut plan = compose_hash_plan("stable-hash", "decune/test:first", "1.0.0");
        plan.config.compose.published_ports.relocation = true;
        plan.forward_ports = vec![ResolvedForwardPort {
            service: None,
            container: 3000,
            requested_host: 3000,
            host: 3000,
            host_ip: "127.0.0.1".to_owned(),
            protocol: PortProtocol::Tcp,
            require_local: false,
            label: None,
        }];
        let model = serde_json::from_value(serde_json::json!({
            "services": {
                "app": {
                    "ports": [{"host_ip": "127.0.0.1", "target": 3000, "published": "3000"}]
                }
            }
        }))
        .unwrap();
        let port_entries = classify_compose_published_ports(&model);
        let input = compose_published_port_planning_input(&model, &port_entries, "app", &[]);

        let (port_plan, port_override) = finalized_compose_published_ports(
            &plan,
            Some(ComposePublishedPortFinalization {
                input: &input,
                existing_project_published_ports: &[],
            }),
        )
        .unwrap();

        assert_eq!(port_plan.entries[0].planned.host_port, 3001);
        assert!(port_plan.entries[0].relocated);
        assert_eq!(
            port_override.ports_for("app").unwrap()[0].get("published"),
            Some(&serde_json::json!("3001"))
        );
    }

    #[test]
    fn generated_override_semantic_hash_excludes_hash_derived_values() {
        let first = compose_generated_override_hash_input(
            PathBuf::from("/state/compose.override.yaml"),
            &compose_hash_plan("first-hash", "decune/test:first-hash", "1.0.0"),
            &[],
            None,
            &ComposePublishedPortOverride::default(),
        )
        .unwrap();
        let second = compose_generated_override_hash_input(
            PathBuf::from("/state/compose.override.yaml"),
            &compose_hash_plan("second-hash", "decune/test:second-hash", "1.0.0"),
            &[],
            None,
            &ComposePublishedPortOverride::default(),
        )
        .unwrap();

        assert_eq!(first.content_hash, second.content_hash);
    }

    #[test]
    fn generated_override_semantic_hash_distinguishes_unspecified_and_false_security_booleans() {
        let unspecified = compose_hash_plan("stable-hash", "decune/test:first", "1.0.0");
        let mut explicit_false = compose_hash_plan("stable-hash", "decune/test:first", "1.0.0");
        explicit_false.config.devcontainer.init = Some(false);
        explicit_false.config.devcontainer.privileged = Some(false);

        let first = compose_generated_override_hash_input(
            PathBuf::from("/state/compose.override.yaml"),
            &unspecified,
            &[],
            None,
            &ComposePublishedPortOverride::default(),
        )
        .unwrap();
        let second = compose_generated_override_hash_input(
            PathBuf::from("/state/compose.override.yaml"),
            &explicit_false,
            &[],
            None,
            &ComposePublishedPortOverride::default(),
        )
        .unwrap();

        assert_ne!(first.content_hash, second.content_hash);
    }

    fn compose_hash_plan(config_hash: &str, image: &str, version: &str) -> UpPlan {
        let mut config = ResolvedConfig::default();
        config.devcontainer.source = Some(ResolvedDevcontainerSource::Compose(
            LayerDevcontainerCompose {
                files: vec!["compose.yaml".to_owned()],
                service: "app".to_owned(),
                run_services: None,
            },
        ));

        UpPlan {
            image: image.to_owned(),
            base_image: "alpine:3.20".to_owned(),
            build_context: None,
            build_options: DockerBuildOptions::default(),
            feature_install: None,
            feature_build_context_dir: None,
            uid_gid_sync_build_context_dir: None,
            resources: DockerResources {
                container_name: "decune-test".to_owned(),
                image_tag: image.to_owned(),
                workspace_volume_name: "decune-test-workspace".to_owned(),
                labels: BTreeMap::from([
                    ("decune.managed".to_owned(), "true".to_owned()),
                    ("decune.workspace_id".to_owned(), "workspace-id".to_owned()),
                    ("decune.config_hash".to_owned(), config_hash.to_owned()),
                    ("decune.version".to_owned(), version.to_owned()),
                    (
                        "com.docker.compose.project".to_owned(),
                        "user-project".to_owned(),
                    ),
                ]),
                config_hash: config_hash.to_owned(),
            },
            pre_uid_gid_sync_resources: None,
            compose_project: None,
            config_layers: ConfigMergeInput::default(),
            config,
            sensitive_container_env: Default::default(),
            sensitive_build_args: Default::default(),
            compose_interpolation_env: Default::default(),
            compose_interpolation_redactions: Vec::new(),
            effective_users: EffectiveUsers::root(),
            uid_gid_sync_plan: UidGidSyncPlan::default(),
            workspace_folder: "/workspace".to_owned(),
            mounts: Vec::new(),
            dotfile_skeletons: Vec::new(),
            forward_ports: Vec::new(),
            ignored_detached_forwarding: false,
        }
    }

    async fn prepared_feature_plan(workspace: &Workspace) -> UpPlan {
        let plan = build_up_plan(workspace, None, ConfigLayer::default()).unwrap();
        prepare_feature_metadata_for_plan(workspace, plan, false)
            .await
            .unwrap()
    }

    fn config_hash_for_static_build_args(plan: &UpPlan) -> String {
        let mut input = ConfigHashInput::new(&plan.config);
        input.sensitive_build_arg_keys = plan
            .sensitive_build_args
            .iter()
            .map(|(key, _)| key.clone())
            .collect();
        config_hash(&input)
    }
}
