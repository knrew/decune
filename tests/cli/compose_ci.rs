use std::{fs, path::Path};

#[test]
fn compose_ci_readme_documents_compose_integration_command() {
    let readme = read_workspace_file("README.md");

    assert!(readme.contains("cargo run --locked -p xtask -- compose-integration"));
    assert!(!readme.contains("DECUNE_COMPOSE_INTEGRATION"));
    assert!(readme.contains("docker compose version"));
    assert!(readme.contains("--ignored"));
}

#[test]
fn compose_ci_spec_documents_compose_integration_command() {
    let specification = read_workspace_file("docs/specification.md");

    assert!(specification.contains("cargo run --locked -p xtask -- compose-integration"));
    assert!(!specification.contains("DECUNE_COMPOSE_INTEGRATION"));
    assert!(specification.contains("Docker Compose v2 plugin"));
    assert!(specification.contains("--ignored"));
}

#[test]
fn compose_ci_spec_documents_generated_override_hash_contract() {
    let specification = read_workspace_file("docs/specification.md");

    assert!(specification.contains("generated override semantic hash input"));
    assert!(specification.contains("user Compose files だけを対象にした"));
    assert!(!specification.contains("canonical model には generated override を含めたものを使う"));
    assert!(!specification.contains("generated override plan を含める"));
}

#[test]
fn compose_ci_workflow_exposes_compose_integration_job() {
    let ci = read_workspace_file(".github/workflows/ci.yaml");

    assert!(ci.contains("cargo run --locked -p xtask -- compose-integration"));
    assert!(ci.contains("docker compose version"));
    assert!(ci.contains("cargo run --locked -p xtask -- workspace-test"));
    assert!(!ci.contains("DECUNE_COMPOSE_INTEGRATION"));
}

fn read_workspace_file(path: &str) -> String {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(path)).unwrap()
}
