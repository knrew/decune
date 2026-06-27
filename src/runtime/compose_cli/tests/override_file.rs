#![allow(unused_imports)]

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use crate::runtime::command::{FakeRuntimeCommand, RuntimeOutput};
use crate::runtime::compose_cli::{
    ComposeBuildOptions, ComposeCliCapabilities, ComposeCommandPlan, ComposeConfigModel,
    ComposeConfigService, ComposeDownOptions, ComposeIntrospector, ComposeLifecyclePlan,
    ComposeOverrideMount, ComposeOverridePatch, ComposeOverrideServicePatch,
    ComposePrimaryImageResolver, ComposeProjectPlan, ComposePullOptions, ComposeServiceValidation,
    ComposeStopOptions, ComposeUpOptions, DockerComposeCli, resolve_compose_container,
    write_compose_override,
};
use crate::runtime::compose_ports::{
    COMPOSE_PUBLISHED_PORT_COLLISION, ComposePortEligibility, ComposePublishedPortPlan,
    ComposePublishedPortStartupDiagnostics, classify_compose_published_ports,
    compose_published_port_planning_input,
};
use crate::workspace::Workspace;

use super::super::{
    compose_build_command, compose_down_command, compose_pull_command, compose_stop_command,
    compose_up_command, parse_compose_ps_json,
};
use super::test_support::{
    fixture_workspace, lifecycle_command_plan, runtime_error_output, runtime_output,
    valid_compose_capabilities, write_compose_file,
};

#[test]
fn compose_override_yaml_patches_only_primary_service() {
    let patch = ComposeOverridePatch::new(
        ComposeOverrideServicePatch::new("app")
            .label("decune.managed", "true")
            .label("decune.workspace_id", "workspace-id")
            .environment("APP_ENV", "development")
            .user("decune")
            .mount(ComposeOverrideMount::bind(
                "/host/cache",
                "/workspaces/cache",
                true,
            )),
    );

    let yaml = patch.to_yaml().unwrap();

    assert_eq!(
        yaml,
        concat!(
            "services:\n",
            "  'app':\n",
            "    labels:\n",
            "      'decune.managed': 'true'\n",
            "      'decune.workspace_id': 'workspace-id'\n",
            "    environment:\n",
            "      'APP_ENV': 'development'\n",
            "    user: 'decune'\n",
            "    volumes:\n",
            "      - type: bind\n",
            "        source: '/host/cache'\n",
            "        target: '/workspaces/cache'\n",
            "        read_only: true\n",
            "        bind:\n",
            "          create_host_path: false\n",
        )
    );
    assert!(!yaml.contains("sidecar"));
}

#[test]
fn compose_override_yaml_sets_generated_image_and_pull_policy_never() {
    let patch = ComposeOverridePatch::new(
        ComposeOverrideServicePatch::new("app")
            .image("decune/workspace:hash123")
            .pull_policy_never(),
    );

    let yaml = patch.to_yaml().unwrap();

    assert!(yaml.contains("    image: 'decune/workspace:hash123'\n"));
    assert!(yaml.contains("    pull_policy: 'never'\n"));
}

#[test]
fn compose_override_yaml_replaces_ports_with_override_tag() {
    let patch =
        ComposeOverridePatch::new(ComposeOverrideServicePatch::new("app").ports_override(vec![
            BTreeMap::from([
                ("app_protocol".to_owned(), serde_json::json!("http")),
                ("host_ip".to_owned(), serde_json::json!("127.0.0.1")),
                ("mode".to_owned(), serde_json::json!("host")),
                ("name".to_owned(), serde_json::json!("web")),
                ("protocol".to_owned(), serde_json::json!("tcp")),
                ("published".to_owned(), serde_json::json!("3001")),
                ("target".to_owned(), serde_json::json!(3000)),
            ]),
            BTreeMap::from([
                ("protocol".to_owned(), serde_json::json!("udp")),
                ("published".to_owned(), serde_json::json!("8125")),
                ("target".to_owned(), serde_json::json!(8125)),
            ]),
            BTreeMap::from([
                ("protocol".to_owned(), serde_json::json!("tcp")),
                ("target".to_owned(), serde_json::json!(9000)),
            ]),
        ]));

    let yaml = patch.to_yaml().unwrap();

    assert_eq!(
        yaml,
        concat!(
            "services:\n",
            "  'app':\n",
            "    ports: !override\n",
            "      - app_protocol: 'http'\n",
            "        host_ip: '127.0.0.1'\n",
            "        mode: 'host'\n",
            "        name: 'web'\n",
            "        protocol: 'tcp'\n",
            "        published: '3001'\n",
            "        target: 3000\n",
            "      - protocol: 'udp'\n",
            "        published: '8125'\n",
            "        target: 8125\n",
            "      - protocol: 'tcp'\n",
            "        target: 9000\n",
        )
    );
}

#[test]
fn compose_override_command_is_emitted_only_when_requested() {
    let keepalive =
        ComposeOverridePatch::new(ComposeOverrideServicePatch::new("app").keepalive_command(true))
            .to_yaml()
            .unwrap();
    let original =
        ComposeOverridePatch::new(ComposeOverrideServicePatch::new("app").keepalive_command(false))
            .to_yaml()
            .unwrap();

    assert!(keepalive.contains("    command:\n      - 'sleep'\n      - 'infinity'\n"));
    assert!(!original.contains("command:"));
}

#[test]
fn compose_override_secret_leak_regression_does_not_persist_secret_literals() {
    let temp = tempfile::tempdir().unwrap();
    let override_path = temp.path().join("compose.override.yaml");
    let patch = ComposeOverridePatch::new(
        ComposeOverrideServicePatch::new("app")
            .environment("GH_TOKEN_FILE", "/run/decune/secrets/github-token")
            .mount(ComposeOverrideMount::bind(
                "/tmp/decune/secrets/github-token",
                "/run/decune/secrets/github-token",
                true,
            ))
            .secret_value_forbidden("github-test-secret"),
    );

    write_compose_override(&override_path, &patch).unwrap();

    let yaml = fs::read_to_string(override_path).unwrap();
    assert!(yaml.contains("/run/decune/secrets/github-token"));
    assert!(!yaml.contains("github-test-secret"));
}

#[test]
fn compose_override_yaml_uses_placeholder_for_interpolated_environment() {
    let patch = ComposeOverridePatch::new(
        ComposeOverrideServicePatch::new("app").interpolated_environment(
            "NPM_TOKEN",
            "DECUNE_CONTAINER_ENV_NPM_TOKEN",
            vec!["secret-token".to_owned()],
        ),
    );

    let yaml = patch.to_yaml().unwrap();

    assert!(yaml.contains("'NPM_TOKEN': '${DECUNE_CONTAINER_ENV_NPM_TOKEN}'"));
    assert!(!yaml.contains("secret-token"));
}
