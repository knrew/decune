use super::*;

pub(super) struct ComposeGeneratedOverrideRuntime<'a> {
    pub(super) compose_primary_service: Option<&'a ComposeConfigService>,
    pub(super) service_forward: &'a [ServiceForwardRuntime],
    pub(super) published_port_override: &'a ComposePublishedPortOverride,
    pub(super) name_rewrite_plan: &'a ComposeIsolationNameRewritePlan,
    pub(super) subnet_plan: &'a ComposeIsolationSubnetPlan,
    pub(super) endpoint_plan: &'a ComposeIsolationEndpointPlan,
}

#[derive(Clone, Copy)]
struct ComposeIsolationOverridePlans<'a> {
    names: &'a ComposeIsolationNameRewritePlan,
    subnets: &'a ComposeIsolationSubnetPlan,
    endpoints: &'a ComposeIsolationEndpointPlan,
}

pub(super) async fn write_generated_compose_override(
    client: &DockerClient,
    project: &ComposeProjectPlan,
    primary_service: &str,
    plan: &UpPlan,
    runtime: ComposeGeneratedOverrideRuntime<'_>,
) -> Result<()> {
    let output_path = project.generated_override_path();
    let startup = compose_override_startup(client, plan, runtime.compose_primary_service).await?;
    let override_patch = generated_compose_override_patch(
        primary_service,
        plan,
        startup,
        runtime.service_forward,
        runtime.published_port_override,
        ComposeIsolationOverridePlans {
            names: runtime.name_rewrite_plan,
            subnets: runtime.subnet_plan,
            endpoints: runtime.endpoint_plan,
        },
    )?;
    write_compose_override(&output_path, &override_patch)
}

#[cfg(test)]
pub(in crate::up) fn generated_compose_override_content(
    primary_service: &str,
    plan: &UpPlan,
) -> Result<String> {
    let startup = if plan.config.devcontainer.override_command {
        let (entrypoint, command) = devcontainer_keepalive_command();
        Some(ComposeOverrideStartup {
            entrypoint,
            command,
        })
    } else {
        None
    };
    generated_compose_override_content_with_startup(
        primary_service,
        plan,
        startup,
        &[],
        &ComposePublishedPortOverride::default(),
    )
}

async fn compose_override_startup(
    client: &DockerClient,
    plan: &UpPlan,
    compose_primary_service: Option<&ComposeConfigService>,
) -> Result<Option<ComposeOverrideStartup>> {
    if !plan.config.devcontainer.entrypoints.is_empty() {
        let command = if plan.config.devcontainer.override_command {
            let (entrypoint, command) = devcontainer_keepalive_command();
            let mut wrapped_command = vec![entrypoint.join(" ")];
            wrapped_command.extend(command);
            wrapped_command
        } else {
            let image_startup = image_startup_command(client, &plan.image).await?;
            let startup = crate::up::metadata::effective_startup_command(
                image_startup,
                compose_primary_service,
            );
            let mut wrapped_command = startup.entrypoint;
            wrapped_command.extend(startup.command);
            wrapped_command
        };
        return Ok(Some(ComposeOverrideStartup {
            entrypoint: vec![FEATURE_ENTRYPOINT_WRAPPER.to_owned()],
            command,
        }));
    }

    if plan.config.devcontainer.override_command {
        let (entrypoint, command) = devcontainer_keepalive_command();
        return Ok(Some(ComposeOverrideStartup {
            entrypoint,
            command,
        }));
    }

    Ok(None)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ComposeOverrideStartup {
    entrypoint: Vec<String>,
    command: Vec<String>,
}

pub(super) fn warn_on_compose_published_port_relocations(
    plan: &UpPlan,
    port_plan: &ComposePublishedPortPlan,
) {
    for message in compose_published_port_relocation_warning_messages(plan, port_plan) {
        ui::warn(&message);
    }
}

fn compose_published_port_relocation_warning_messages(
    plan: &UpPlan,
    port_plan: &ComposePublishedPortPlan,
) -> Vec<String> {
    if !plan.config.compose.published_ports.warn_on_relocation {
        return Vec::new();
    }

    port_plan
        .entries
        .iter()
        .filter(|entry| entry.relocated)
        .map(|entry| {
            format!(
                "Compose published port relocation changed service `{}` target {}/{} from {} to {}",
                entry.service,
                entry.target_port,
                compose_port_protocol_name(&entry.protocol),
                compose_published_port_endpoint_display(&entry.requested),
                compose_published_port_endpoint_display(&entry.planned)
            )
        })
        .collect()
}

fn compose_published_port_endpoint_display(endpoint: &ComposePublishedPortEndpoint) -> String {
    match &endpoint.host_ip {
        ComposePublishedPortHostIp::Omitted => endpoint.host_port.to_string(),
        ComposePublishedPortHostIp::Explicit(value) => {
            format!("{}:{}", value, endpoint.host_port)
        }
    }
}

pub(super) fn compose_port_protocol_name(protocol: &ComposePortProtocol) -> &str {
    match protocol {
        ComposePortProtocol::Tcp => "tcp",
        ComposePortProtocol::Udp => "udp",
        ComposePortProtocol::Other(value) | ComposePortProtocol::Invalid(value) => value,
    }
}

#[cfg(test)]
fn generated_compose_override_content_with_startup(
    primary_service: &str,
    plan: &UpPlan,
    startup: Option<ComposeOverrideStartup>,
    service_forward: &[ServiceForwardRuntime],
    published_port_override: &ComposePublishedPortOverride,
) -> Result<String> {
    generated_compose_override_patch(
        primary_service,
        plan,
        startup,
        service_forward,
        published_port_override,
        ComposeIsolationOverridePlans {
            names: &ComposeIsolationNameRewritePlan::default(),
            subnets: &ComposeIsolationSubnetPlan::default(),
            endpoints: &ComposeIsolationEndpointPlan::default(),
        },
    )?
    .to_yaml()
}

fn generated_compose_override_patch(
    primary_service: &str,
    plan: &UpPlan,
    startup: Option<ComposeOverrideStartup>,
    service_forward: &[ServiceForwardRuntime],
    published_port_override: &ComposePublishedPortOverride,
    isolation: ComposeIsolationOverridePlans<'_>,
) -> Result<ComposeOverridePatch> {
    let mut service = ComposeOverrideServicePatch::new(primary_service)
        .image(&plan.image)
        .labels(&plan.resources.labels)
        .cap_add(&plan.config.devcontainer.cap_add)
        .security_opt(&plan.config.devcontainer.security_opt)
        .mounts(&plan.mounts);
    if let Some(ports) = published_port_override.ports_for(primary_service) {
        service = service.ports_override(ports.to_vec());
    }
    let mut used_placeholders = BTreeSet::new();
    for (key, value) in &plan.config.devcontainer.container_env {
        if let Some(sensitive) = plan.sensitive_container_env.get(key) {
            let placeholder = compose_container_env_placeholder(key, &mut used_placeholders);
            service =
                service.interpolated_environment(key, placeholder, sensitive.redactions.clone());
        } else {
            service = service.environment(key, value);
        }
    }
    if plan.image != plan.base_image {
        service = service.pull_policy_never();
    }
    if let Some(user) = compose_override_user(plan)? {
        service = service.user(user);
    }
    if let Some(init) = plan.config.devcontainer.init {
        service = service.init(init);
    }
    if let Some(privileged) = plan.config.devcontainer.privileged {
        service = service.privileged(privileged);
    }
    if let Some(startup) = startup {
        service = service
            .entrypoint(startup.entrypoint)
            .command(startup.command);
    }
    let mut patch = ComposeOverridePatch::new(service);
    for runtime in service_forward {
        let mut service = ComposeOverrideServicePatch::new(runtime.service())
            .labels(&compose_service_forward_labels(&plan.resources.labels))
            .mount(runtime.mount().clone().into());
        if let Some(ports) = published_port_override.ports_for(runtime.service()) {
            service = service.ports_override(ports.to_owned());
        }
        patch = patch.service(service);
    }
    for (service_name, ports) in published_port_override.services() {
        if service_name == primary_service
            || service_forward
                .iter()
                .any(|runtime| runtime.service() == service_name)
        {
            continue;
        }
        patch = patch
            .service(ComposeOverrideServicePatch::new(service_name).ports_override(ports.clone()));
    }
    patch = apply_compose_name_rewrites(patch, isolation.names);
    patch = apply_compose_subnet_plan(patch, isolation.subnets);
    patch = apply_compose_endpoint_plan(patch, isolation.endpoints);
    Ok(patch)
}

fn apply_compose_endpoint_plan(
    mut patch: ComposeOverridePatch,
    endpoint_plan: &ComposeIsolationEndpointPlan,
) -> ComposeOverridePatch {
    for (service, environment) in &endpoint_plan.services {
        for (key, value) in environment {
            patch = patch.service_environment(service, key, value);
        }
    }
    patch
}

fn apply_compose_subnet_plan(
    mut patch: ComposeOverridePatch,
    subnet_plan: &ComposeIsolationSubnetPlan,
) -> ComposeOverridePatch {
    for allocation in &subnet_plan.allocations {
        patch = patch.network_ipam_override(
            &allocation.network,
            ComposeOverrideNetworkIpamConfig {
                subnet: allocation.planned_subnet.clone(),
                gateway: allocation.planned_gateway.clone(),
                ip_range: allocation.planned_ip_range.clone(),
                aux_addresses: allocation.planned_aux_addresses.clone(),
            },
        );
    }
    patch
}

fn apply_compose_name_rewrites(
    mut patch: ComposeOverridePatch,
    name_rewrite_plan: &ComposeIsolationNameRewritePlan,
) -> ComposeOverridePatch {
    for rewrite in &name_rewrite_plan.services {
        patch = patch.service_container_name(
            &rewrite.service,
            &rewrite.rewritten_name,
            &rewrite.original_name,
            &rewrite.networks,
        );
    }
    for rewrite in &name_rewrite_plan.service_references {
        patch = patch.service_container_references(
            &rewrite.service,
            ComposeOverrideContainerReferences {
                network_mode: rewrite.network_mode.as_deref(),
                ipc: rewrite.ipc.as_deref(),
                pid: rewrite.pid.as_deref(),
                volumes_from: rewrite.volumes_from.as_deref(),
                external_links: rewrite.external_links.as_deref(),
            },
        );
    }
    for rewrite in &name_rewrite_plan.resources {
        patch = match rewrite.kind {
            ComposeIsolationResourceKind::Network => {
                patch.network_name(&rewrite.resource, &rewrite.rewritten_name)
            }
            ComposeIsolationResourceKind::Volume => {
                patch.volume_name(&rewrite.resource, &rewrite.rewritten_name)
            }
            ComposeIsolationResourceKind::Config => {
                patch.config_name(&rewrite.resource, &rewrite.rewritten_name)
            }
            ComposeIsolationResourceKind::Secret => {
                patch.secret_name(&rewrite.resource, &rewrite.rewritten_name)
            }
            ComposeIsolationResourceKind::ServiceContainer => patch,
        };
    }
    patch
}

pub(super) fn attach_compose_interpolation_env_to_plan(plan: &mut UpPlan) {
    let (env, redactions) = compose_interpolation_env(&plan.sensitive_container_env);
    plan.compose_interpolation_env = env.clone();
    plan.compose_interpolation_redactions
        .clone_from(&redactions);
    if let Some(project) = plan.compose_project.take() {
        plan.compose_project = Some(project.with_generated_override_env(env, redactions));
    }
}

fn compose_interpolation_env(
    sensitive_env: &crate::config::variables::SensitiveEnvMap,
) -> (BTreeMap<String, String>, Vec<String>) {
    let mut env = BTreeMap::new();
    let mut redactions = Vec::new();
    let mut used_placeholders = BTreeSet::new();
    for (key, value) in sensitive_env.iter() {
        let placeholder = compose_container_env_placeholder(key, &mut used_placeholders);
        env.insert(placeholder, value.value.clone());
        redactions.extend(value.redactions.clone());
    }

    (env, redactions)
}

fn compose_container_env_placeholder(key: &str, used: &mut BTreeSet<String>) -> String {
    let mut safe = String::new();
    for ch in key.chars() {
        if ch.is_ascii_alphanumeric() {
            safe.push(ch.to_ascii_uppercase());
        } else {
            safe.push('_');
        }
    }
    if safe.is_empty() || safe.as_bytes()[0].is_ascii_digit() {
        safe.insert(0, '_');
    }

    let base = format!("DECUNE_CONTAINER_ENV_{safe}");
    if used.insert(base.clone()) {
        return base;
    }
    for index in 2.. {
        let candidate = format!("{base}_{index}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
    }
    unreachable!("placeholder collision loop always returns");
}

fn compose_service_forward_labels(labels: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut service_labels = BTreeMap::from([("decune.managed".to_owned(), "true".to_owned())]);
    if let Some(workspace_id) = labels.get("decune.workspace_id") {
        service_labels.insert("decune.workspace_id".to_owned(), workspace_id.clone());
    }
    service_labels
}

fn compose_override_user(plan: &UpPlan) -> Result<Option<String>> {
    if plan.config.devcontainer.container_user.is_some() {
        return uid_gid_sync_runtime_user(
            &plan.effective_users.container_user.user,
            &plan.uid_gid_sync_plan,
        )
        .map(Some);
    }

    if !matches!(
        plan.effective_users.container_user.source,
        crate::docker::user::RemoteUserSource::ComposeService
    ) {
        return Ok(None);
    }

    let runtime_user = uid_gid_sync_runtime_user(
        &plan.effective_users.container_user.user,
        &plan.uid_gid_sync_plan,
    )?;
    if runtime_user == plan.effective_users.container_user.user {
        return Ok(None);
    }

    Ok(Some(runtime_user))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::super::test_support::{generated_override_test_plan, sync_plan};
    use super::*;
    use crate::{
        config::{
            ConfigMergeInput, resolved::ResolvedConfig, types::MountType,
            variables::SensitiveEnvValue,
        },
        docker::{
            build::DockerBuildOptions,
            mounts::{DockerMountSpec, MountBindOptions, MountBindPropagation, MountVolumeOptions},
            resource::DockerResources,
            user::{
                EffectiveUserResolveInput, EffectiveUsers, UidGidSyncPlan, resolve_effective_users,
                resolve_effective_users_with_compose_service_user,
            },
        },
        host::forward::ServiceForwardRuntime,
        runtime::compose_ports::{
            ComposePortEligibility, ComposePortEntry, ComposePortHostIp, ComposePortProtocol,
            ComposePortSyntax, ComposePublishedHostPort, ComposePublishedPortAllocationReason,
            ComposePublishedPortEndpoint, ComposePublishedPortHostIp, ComposePublishedPortPlan,
            ComposePublishedPortPlanEntry, ComposePublishedPortPlanEntryType,
            ComposePublishedPortPlanSource, ComposePublishedPortPlannedEndpointProbe,
            compose_published_port_override,
        },
        up::UpPlan,
    };

    #[test]
    fn generated_compose_override_patches_only_primary_service() {
        let mut config = ResolvedConfig::default();
        config.devcontainer.container_env =
            BTreeMap::from([("FROM_DECUNE".to_owned(), "1".to_owned())]);
        config.devcontainer.override_command = false;
        let mut resources = DockerResources {
            container_name: "unused".to_owned(),
            image_tag: "decune/test:hash".to_owned(),
            workspace_volume_name: "unused-volume".to_owned(),
            labels: BTreeMap::new(),
            config_hash: "hash".to_owned(),
        };
        resources
            .labels
            .insert("decune.managed".to_owned(), "true".to_owned());
        let plan = UpPlan {
            image: "decune/test:hash".to_owned(),
            base_image: "alpine:3.20".to_owned(),
            build_context: None,
            build_options: DockerBuildOptions::default(),
            feature_install: None,
            feature_build_context_dir: None,
            uid_gid_sync_build_context_dir: None,
            resources,
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
            workspace_folder: "/workspace".to_owned(),
            mounts: vec![DockerMountSpec {
                source: Some("/host/cache".to_owned()),
                target: "/cache".to_owned(),
                mount_type: MountType::Bind,
                read_only: true,
                consistency: None,
                bind_options: None,
                volume_options: None,
            }],
            dotfile_skeletons: Vec::new(),
            forward_ports: Vec::new(),
            ignored_detached_forwarding: false,
        };

        let content = generated_compose_override_content("app", &plan).unwrap();

        assert!(content.contains("'app':"));
        assert!(content.contains("image: 'decune/test:hash'"));
        assert!(content.contains("pull_policy: 'never'"));
        assert!(content.contains("'FROM_DECUNE': '1'"));
        assert!(content.contains("'decune.managed': 'true'"));
        assert!(content.contains("target: '/cache'"));
        assert!(!content.contains("sidecar"));
    }

    #[test]
    fn generated_compose_override_labels_explicit_sidecar_forwarding_service() {
        let mut plan = generated_override_test_plan(Vec::new());
        plan.resources.labels = BTreeMap::from([
            ("decune.managed".to_owned(), "true".to_owned()),
            ("decune.workspace_id".to_owned(), "workspace-id".to_owned()),
            ("decune.config_hash".to_owned(), "hash".to_owned()),
        ]);
        let service_forward = vec![ServiceForwardRuntime::for_test(
            "db",
            DockerMountSpec {
                source: Some("/tmp/decune-runtime/forward/db".to_owned()),
                target: "/run/decune".to_owned(),
                mount_type: MountType::Bind,
                read_only: false,
                consistency: None,
                bind_options: None,
                volume_options: None,
            },
        )];

        let content = generated_compose_override_content_with_startup(
            "app",
            &plan,
            None,
            &service_forward,
            &ComposePublishedPortOverride::default(),
        )
        .unwrap();

        assert!(content.contains("  'db':\n"));
        assert!(content.contains("'decune.managed': 'true'"));
        assert!(content.contains("'decune.workspace_id': 'workspace-id'"));
        assert!(content.contains("target: '/run/decune'"));
    }

    #[test]
    fn generated_compose_override_does_not_override_pull_policy_for_original_image() {
        let mut plan = generated_override_test_plan(Vec::new());
        plan.image = "alpine:3.20".to_owned();
        plan.base_image = "alpine:3.20".to_owned();

        let content = generated_compose_override_content("app", &plan).unwrap();

        assert!(content.contains("image: 'alpine:3.20'"));
        assert!(!content.contains("pull_policy:"));
    }

    #[test]
    fn generated_compose_override_writes_explicit_false_security_booleans() {
        let mut plan = generated_override_test_plan(Vec::new());
        plan.config.devcontainer.init = Some(false);
        plan.config.devcontainer.privileged = Some(false);

        let content = generated_compose_override_content("app", &plan).unwrap();

        assert!(content.contains("    init: false\n"));
        assert!(content.contains("    privileged: false\n"));
    }

    #[test]
    fn generated_compose_override_writes_synced_container_user() {
        let mut plan = generated_override_test_plan(Vec::new());
        plan.config.devcontainer.container_user = Some("2001:2001".to_owned());
        plan.effective_users = resolve_effective_users(EffectiveUserResolveInput {
            devcontainer_remote: None,
            devcontainer_container: Some("2001:2001"),
            image_metadata_remote: None,
            image_metadata_container: None,
            image_config: None,
        })
        .unwrap();
        plan.uid_gid_sync_plan = sync_plan();

        let content = generated_compose_override_content("app", &plan).unwrap();

        assert!(content.contains("user: 'syncuser:1000'"));
        assert!(!content.contains("user: '2001:2001'"));
    }

    #[test]
    fn generated_compose_override_writes_compose_service_user_only_when_sync_changes_it() {
        let mut unchanged = generated_override_test_plan(Vec::new());
        unchanged.effective_users = resolve_effective_users_with_compose_service_user(
            EffectiveUserResolveInput {
                devcontainer_remote: None,
                devcontainer_container: None,
                image_metadata_remote: None,
                image_metadata_container: None,
                image_config: None,
            },
            Some("syncuser"),
        )
        .unwrap();
        let unchanged_content = generated_compose_override_content("app", &unchanged).unwrap();
        assert!(!unchanged_content.contains("user:"));

        let mut synced = unchanged;
        synced.effective_users = resolve_effective_users_with_compose_service_user(
            EffectiveUserResolveInput {
                devcontainer_remote: None,
                devcontainer_container: None,
                image_metadata_remote: None,
                image_metadata_container: None,
                image_config: None,
            },
            Some("2001:2001"),
        )
        .unwrap();
        synced.uid_gid_sync_plan = sync_plan();
        let synced_content = generated_compose_override_content("app", &synced).unwrap();

        assert!(synced_content.contains("user: 'syncuser:1000'"));
    }

    #[test]
    fn generated_compose_override_preserves_bind_mount_options() {
        let plan = generated_override_test_plan(vec![DockerMountSpec {
            source: Some("/host/tools".to_owned()),
            target: "/tools".to_owned(),
            mount_type: MountType::Bind,
            read_only: false,
            consistency: Some("cached".to_owned()),
            bind_options: Some(MountBindOptions {
                propagation: Some(MountBindPropagation::RShared),
                create_mountpoint: Some(true),
                ..MountBindOptions::default()
            }),
            volume_options: None,
        }]);

        let content = generated_compose_override_content("app", &plan).unwrap();

        assert!(content.contains("consistency: 'cached'"));
        assert!(content.contains("bind:\n"));
        assert!(content.contains("propagation: 'rshared'"));
        assert!(content.contains("create_host_path: true"));
    }

    #[test]
    fn generated_compose_override_disables_default_bind_source_creation() {
        let plan = generated_override_test_plan(vec![DockerMountSpec {
            source: Some("/host/cache".to_owned()),
            target: "/cache".to_owned(),
            mount_type: MountType::Bind,
            read_only: false,
            consistency: None,
            bind_options: None,
            volume_options: None,
        }]);

        let content = generated_compose_override_content("app", &plan).unwrap();

        assert!(content.contains("bind:\n"));
        assert!(content.contains("create_host_path: false"));
    }

    #[test]
    fn generated_compose_override_preserves_volume_mount_options() {
        let plan = generated_override_test_plan(vec![DockerMountSpec {
            source: Some("project-cache".to_owned()),
            target: "/cache".to_owned(),
            mount_type: MountType::Volume,
            read_only: false,
            consistency: None,
            bind_options: None,
            volume_options: Some(MountVolumeOptions {
                no_copy: Some(true),
                subpath: Some("deps".to_owned()),
                ..MountVolumeOptions::default()
            }),
        }]);

        let content = generated_compose_override_content("app", &plan).unwrap();

        assert!(content.contains("volume:\n"));
        assert!(content.contains("nocopy: true"));
        assert!(content.contains("subpath: 'deps'"));
    }

    #[test]
    fn generated_compose_override_redacts_local_env_derived_container_env() {
        let mut plan = generated_override_test_plan(Vec::new());
        plan.config.devcontainer.container_env =
            BTreeMap::from([("NPM_TOKEN".to_owned(), "secret-token".to_owned())]);
        plan.sensitive_container_env.insert(
            "NPM_TOKEN",
            SensitiveEnvValue {
                value: "secret-token".to_owned(),
                redactions: vec!["secret-token".to_owned()],
            },
        );

        let content = generated_compose_override_content("app", &plan).unwrap();

        assert!(content.contains("'NPM_TOKEN': '${DECUNE_CONTAINER_ENV_NPM_TOKEN}'"));
        assert!(!content.contains("secret-token"));
    }

    #[test]
    fn generated_compose_override_applies_published_port_relocation() {
        let plan = generated_override_test_plan(Vec::new());
        let port_entries = vec![
            compose_port_entry(
                "app",
                0,
                3000,
                ComposePublishedHostPort::Single(3000),
                ComposePortHostIp::Explicit("127.0.0.1".to_owned()),
                ComposePortProtocol::Tcp,
                BTreeMap::from([
                    ("app_protocol".to_owned(), serde_json::json!("http")),
                    ("host_ip".to_owned(), serde_json::json!("127.0.0.1")),
                    ("name".to_owned(), serde_json::json!("web")),
                    ("protocol".to_owned(), serde_json::json!("tcp")),
                    ("published".to_owned(), serde_json::json!("3000")),
                    ("target".to_owned(), serde_json::json!(3000)),
                ]),
            ),
            compose_port_entry(
                "app",
                1,
                8125,
                ComposePublishedHostPort::Single(8125),
                ComposePortHostIp::Omitted,
                ComposePortProtocol::Udp,
                BTreeMap::from([
                    ("protocol".to_owned(), serde_json::json!("udp")),
                    ("published".to_owned(), serde_json::json!("8125")),
                    ("target".to_owned(), serde_json::json!(8125)),
                ]),
            ),
            compose_port_entry(
                "app",
                2,
                9000,
                ComposePublishedHostPort::None,
                ComposePortHostIp::Omitted,
                ComposePortProtocol::Tcp,
                BTreeMap::from([
                    ("protocol".to_owned(), serde_json::json!("tcp")),
                    ("target".to_owned(), serde_json::json!(9000)),
                ]),
            ),
            compose_port_entry(
                "worker",
                0,
                4000,
                ComposePublishedHostPort::Single(4000),
                ComposePortHostIp::Omitted,
                ComposePortProtocol::Tcp,
                BTreeMap::from([
                    ("published".to_owned(), serde_json::json!("4000")),
                    ("target".to_owned(), serde_json::json!(4000)),
                ]),
            ),
        ];
        let port_plan = ComposePublishedPortPlan {
            entries: vec![ComposePublishedPortPlanEntry {
                service: "app".to_owned(),
                port_entry_index: 0,
                source: ComposePublishedPortPlanSource::Compose,
                kind: ComposePublishedPortPlanEntryType::Published,
                target_port: 3000,
                protocol: ComposePortProtocol::Tcp,
                requested: ComposePublishedPortEndpoint {
                    host_ip: ComposePublishedPortHostIp::Explicit("127.0.0.1".to_owned()),
                    host_port: 3000,
                },
                planned: ComposePublishedPortEndpoint {
                    host_ip: ComposePublishedPortHostIp::Explicit("127.0.0.1".to_owned()),
                    host_port: 3001,
                },
                planned_endpoint_probe: ComposePublishedPortPlannedEndpointProbe::Available,
                relocated: true,
                allocation_reason: ComposePublishedPortAllocationReason::Unavailable,
            }],
        };
        let port_override = compose_published_port_override(&port_entries, &port_plan).unwrap();

        let content = generated_compose_override_content_with_startup(
            "app",
            &plan,
            None,
            &[],
            &port_override,
        )
        .unwrap();

        assert!(content.contains("    ports: !override\n"));
        assert!(content.contains("        published: '3001'\n"));
        assert!(content.contains("        target: 3000\n"));
        assert!(content.contains("      - app_protocol: 'http'\n"));
        assert!(content.contains("        name: 'web'\n"));
        assert!(content.contains("        published: '8125'\n"));
        assert!(content.contains("        target: 8125\n"));
        assert!(content.contains("        target: 9000\n"));
        assert!(!content.contains("'worker':"));
        assert!(!content.contains("published: '3000'"));
    }

    #[test]
    fn generated_compose_override_applies_sidecar_published_port_relocation() {
        let plan = generated_override_test_plan(Vec::new());
        let port_entries = vec![
            compose_port_entry(
                "app",
                0,
                3000,
                ComposePublishedHostPort::Single(3000),
                ComposePortHostIp::Omitted,
                ComposePortProtocol::Tcp,
                BTreeMap::from([
                    ("published".to_owned(), serde_json::json!("3000")),
                    ("target".to_owned(), serde_json::json!(3000)),
                ]),
            ),
            compose_port_entry(
                "db",
                0,
                5432,
                ComposePublishedHostPort::Single(5432),
                ComposePortHostIp::Omitted,
                ComposePortProtocol::Tcp,
                BTreeMap::from([
                    ("published".to_owned(), serde_json::json!("5432")),
                    ("target".to_owned(), serde_json::json!(5432)),
                ]),
            ),
        ];
        let port_plan = ComposePublishedPortPlan {
            entries: vec![ComposePublishedPortPlanEntry {
                service: "db".to_owned(),
                port_entry_index: 0,
                source: ComposePublishedPortPlanSource::Compose,
                kind: ComposePublishedPortPlanEntryType::Published,
                target_port: 5432,
                protocol: ComposePortProtocol::Tcp,
                requested: ComposePublishedPortEndpoint {
                    host_ip: ComposePublishedPortHostIp::Omitted,
                    host_port: 5432,
                },
                planned: ComposePublishedPortEndpoint {
                    host_ip: ComposePublishedPortHostIp::Omitted,
                    host_port: 5433,
                },
                planned_endpoint_probe: ComposePublishedPortPlannedEndpointProbe::Available,
                relocated: true,
                allocation_reason: ComposePublishedPortAllocationReason::Unavailable,
            }],
        };
        let port_override = compose_published_port_override(&port_entries, &port_plan).unwrap();

        let content = generated_compose_override_content_with_startup(
            "app",
            &plan,
            None,
            &[],
            &port_override,
        )
        .unwrap();

        assert!(content.contains("  'db':\n"));
        assert!(content.contains("    ports: !override\n"));
        assert!(content.contains("      - published: '5433'\n"));
        assert!(content.contains("        target: 5432\n"));
        assert!(!content.contains("published: '5432'"));
        assert!(!content.contains("host_ip: '0.0.0.0'"));
    }

    #[test]
    fn compose_published_port_relocation_warning_messages_follow_config() {
        let mut plan = generated_override_test_plan(Vec::new());
        let port_plan = ComposePublishedPortPlan {
            entries: vec![ComposePublishedPortPlanEntry {
                service: "app".to_owned(),
                port_entry_index: 0,
                source: ComposePublishedPortPlanSource::Compose,
                kind: ComposePublishedPortPlanEntryType::Published,
                target_port: 3000,
                protocol: ComposePortProtocol::Tcp,
                requested: ComposePublishedPortEndpoint {
                    host_ip: ComposePublishedPortHostIp::Omitted,
                    host_port: 3000,
                },
                planned: ComposePublishedPortEndpoint {
                    host_ip: ComposePublishedPortHostIp::Omitted,
                    host_port: 3001,
                },
                planned_endpoint_probe: ComposePublishedPortPlannedEndpointProbe::Available,
                relocated: true,
                allocation_reason: ComposePublishedPortAllocationReason::Unavailable,
            }],
        };

        assert!(compose_published_port_relocation_warning_messages(&plan, &port_plan).is_empty());

        plan.config.compose.published_ports.warn_on_relocation = true;
        let messages = compose_published_port_relocation_warning_messages(&plan, &port_plan);

        assert_eq!(messages.len(), 1);
        assert!(messages[0].contains("Compose published port relocation changed"));
        assert!(messages[0].contains("service `app`"));
        assert!(messages[0].contains("from 3000 to 3001"));
    }

    #[test]
    fn compose_interpolation_env_is_attached_to_generated_override_command_plan() {
        let mut plan = generated_override_test_plan(Vec::new());
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        std::fs::create_dir_all(&root).unwrap();
        let devcontainer_dir = root.join(".devcontainer");
        std::fs::create_dir_all(&devcontainer_dir).unwrap();
        std::fs::write(devcontainer_dir.join("compose.yaml"), "services: {}\n").unwrap();
        let workspace = crate::workspace::Workspace::resolve(&root).unwrap();
        plan.compose_project = Some(
            crate::runtime::compose_cli::ComposeProjectPlan::resolve(
                &workspace,
                &devcontainer_dir,
                &["compose.yaml".to_owned()],
            )
            .unwrap(),
        );
        plan.sensitive_container_env.insert(
            "NPM_TOKEN",
            SensitiveEnvValue {
                value: "secret-token".to_owned(),
                redactions: vec!["secret-token".to_owned()],
            },
        );
        attach_compose_interpolation_env_to_plan(&mut plan);

        let command = plan
            .compose_project
            .as_ref()
            .unwrap()
            .command_plan_with_generated_override()
            .command(["up", "-d"]);

        assert_eq!(
            command
                .env_value("DECUNE_CONTAINER_ENV_NPM_TOKEN")
                .map(String::as_str),
            Some("secret-token")
        );
        assert!(!command.sanitized_display().contains("secret-token"));
    }

    #[test]
    fn generated_compose_override_uses_feature_entrypoint_wrapper_startup() {
        let mut plan = generated_override_test_plan(Vec::new());
        plan.config.devcontainer.entrypoints =
            vec!["touch /tmp/decune-feature-entrypoint".to_owned()];

        let content = generated_compose_override_content_with_startup(
            "app",
            &plan,
            Some(ComposeOverrideStartup {
                entrypoint: vec![
                    "/usr/local/share/decune/feature-entrypoint-wrapper.sh".to_owned(),
                ],
                command: vec!["/docker-entrypoint.sh".to_owned(), "server".to_owned()],
            }),
            &[],
            &ComposePublishedPortOverride::default(),
        )
        .unwrap();

        assert!(content.contains("entrypoint:"));
        assert!(content.contains("'/usr/local/share/decune/feature-entrypoint-wrapper.sh'"));
        assert!(content.contains("command:"));
        assert!(content.contains("'/docker-entrypoint.sh'"));
        assert!(content.contains("'server'"));
    }

    #[test]
    fn generated_compose_override_preserves_multiline_command_values() {
        let plan = generated_override_test_plan(Vec::new());

        let content = generated_compose_override_content_with_startup(
            "app",
            &plan,
            Some(ComposeOverrideStartup {
                entrypoint: vec!["/bin/sh".to_owned()],
                command: vec![
                    "-c".to_owned(),
                    "trap 'exit 0' TERM\nwhile sleep 1 & wait $!; do :; done".to_owned(),
                ],
            }),
            &[],
            &ComposePublishedPortOverride::default(),
        )
        .unwrap();

        assert!(content.contains("\"trap 'exit 0' TERM\\nwhile sleep 1 & wait $!; do :; done\""));
        assert!(!content.contains("TERM\nwhile"));
    }

    fn compose_port_entry(
        service: &str,
        entry_index: usize,
        target_port: u16,
        published_host_port: ComposePublishedHostPort,
        host_ip: ComposePortHostIp,
        protocol: ComposePortProtocol,
        original_fields: BTreeMap<String, serde_json::Value>,
    ) -> ComposePortEntry {
        let eligibility = match (&published_host_port, &protocol) {
            (ComposePublishedHostPort::None, _) => ComposePortEligibility::UnsupportedContainerOnly,
            (_, ComposePortProtocol::Udp) => ComposePortEligibility::UnsupportedUdp,
            _ => ComposePortEligibility::EligibleFixedTcp,
        };
        ComposePortEntry {
            service: service.to_owned(),
            entry_index,
            service_replica_count: 1,
            service_uses_host_network: false,
            syntax: ComposePortSyntax::EffectiveObject,
            target_port: Some(target_port),
            published_host_port,
            host_ip,
            protocol,
            original_fields,
            eligibility,
            unsupported_reason: None,
        }
    }
}
