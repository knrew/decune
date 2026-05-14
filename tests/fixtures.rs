mod support;

use std::{ffi::OsStr, fs};

use support::TempWorkspace;

#[test]
fn temp_workspace_creates_unique_directory() {
    let first = TempWorkspace::new().unwrap();
    let second = TempWorkspace::new().unwrap();

    assert!(first.path().is_dir());
    assert!(second.path().is_dir());
    assert_ne!(first.path(), second.path());
}

#[test]
fn temp_workspace_writes_nested_files() {
    let workspace = TempWorkspace::new().unwrap();

    let file = workspace
        .write_file(".devcontainer/devcontainer.json", br#"{"image":"ubuntu"}"#)
        .unwrap();

    assert_eq!(fs::read(&file).unwrap(), br#"{"image":"ubuntu"}"#);
    assert!(workspace.path().join(".devcontainer").is_dir());
}

#[test]
fn temp_workspace_creates_nested_directories() {
    let workspace = TempWorkspace::new().unwrap();

    let directory = workspace.create_dir(".decune/cache").unwrap();

    assert_eq!(directory, workspace.path().join(".decune/cache"));
    assert!(directory.is_dir());
}

#[test]
fn temp_workspace_rejects_paths_outside_workspace() {
    let workspace = TempWorkspace::new().unwrap();

    let error = workspace.write_file("../outside", b"contents").unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn docker_test_flag_requires_exact_one() {
    assert!(support::docker_tests_enabled_from(Some(OsStr::new("1"))));
    assert!(!support::docker_tests_enabled_from(Some(OsStr::new(
        "true"
    ))));
    assert!(!support::docker_tests_enabled_from(Some(OsStr::new(""))));
    assert!(!support::docker_tests_enabled_from(None));
}

#[test]
fn docker_skip_helper_matches_current_environment() {
    if support::docker_tests_enabled() {
        assert!(!support::skip_unless_docker_tests_enabled());
    } else {
        assert!(support::skip_unless_docker_tests_enabled());
    }
}
