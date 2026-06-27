use std::{fs, path::Path};

use crate::harness::TestUnwrap as _;

#[test]
fn package_dist_strict_version_smoke_is_release_only() {
    let reusable = read_workspace_file(".github/workflows/package-dist-reusable.yaml");
    let ci = read_workspace_file(".github/workflows/ci.yaml");
    let release = read_workspace_file(".github/workflows/release.yaml");

    assert!(reusable.contains("verify-display-version:"));
    assert!(reusable.contains("default: false"));
    assert!(reusable.contains(r#"case "$version_output" in"#));
    assert!(reusable.contains(r#""decune "?*) ;;"#));
    assert!(reusable.contains(r#"if [ "${{ inputs.verify-display-version }}" = "true" ]; then"#));
    assert!(reusable.contains(r#"test "$version_output" = "decune ${{ inputs.version }}""#));

    assert!(ci.contains("version: 0.0.0-dev"));
    assert!(ci.contains("verify-display-version: false"));
    assert!(release.contains("verify-display-version: true"));
}

fn read_workspace_file(path: &str) -> String {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(path)).must()
}
