use std::{fs, path::Path};

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
