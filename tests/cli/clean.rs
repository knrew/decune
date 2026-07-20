use serde_json::Value;

use crate::harness::*;

const WORKSPACE_ID: &str = "123456abcdef";

#[test]
fn clean_dry_run_json_reports_stale_workspace_without_removing_it() {
    let temp = support::TempWorkspace::new().unwrap();
    let paths = CleanTestPaths::new(&temp, WORKSPACE_ID);
    paths.create_workspace_data();
    paths.create_feature_cache();
    let fake_path = fake_docker_path(&temp, "cli/fake-bin/docker-empty-clean.sh");

    let output = decune()
        .env("PATH", &fake_path)
        .env("XDG_CACHE_HOME", &paths.cache_home)
        .env("XDG_STATE_HOME", &paths.state_home)
        .env("XDG_RUNTIME_DIR", &paths.runtime_home)
        .args(["clean", "--dry-run", "--json"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["dry_run"], true);
    assert_eq!(json["include_feature_cache"], false);
    assert_eq!(json["summary"]["remove_candidates"], 1);
    assert_eq!(json["targets"].as_array().unwrap().len(), 1);
    assert_eq!(json["targets"][0]["kind"], "workspace");
    assert_eq!(json["targets"][0]["workspace_id"], WORKSPACE_ID);
    assert_eq!(json["targets"][0]["action"], "remove");
    assert_eq!(json["targets"][0]["reason"], "stale_workspace_data");
    assert!(paths.cache_dir.exists());
    assert!(paths.state_dir.exists());
    assert!(paths.runtime_dir.exists());
    assert!(paths.feature_cache_dir.exists());
}

#[test]
fn clean_no_confirm_removes_stale_workspace_and_keeps_feature_cache_by_default() {
    let temp = support::TempWorkspace::new().unwrap();
    let paths = CleanTestPaths::new(&temp, WORKSPACE_ID);
    paths.create_workspace_data();
    paths.create_feature_cache();
    let fake_path = fake_docker_path(&temp, "cli/fake-bin/docker-empty-clean.sh");

    decune()
        .env("PATH", &fake_path)
        .env("XDG_CACHE_HOME", &paths.cache_home)
        .env("XDG_STATE_HOME", &paths.state_home)
        .env("XDG_RUNTIME_DIR", &paths.runtime_home)
        .args(["clean", "--no-confirm"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "Removed stale decune-managed data",
        ));

    assert!(!paths.cache_dir.exists());
    assert!(!paths.state_dir.exists());
    assert!(!paths.runtime_dir.exists());
    assert!(paths.feature_cache_dir.exists());
}

#[test]
fn clean_include_feature_cache_adds_shared_feature_cache_target() {
    let temp = support::TempWorkspace::new().unwrap();
    let paths = CleanTestPaths::new(&temp, WORKSPACE_ID);
    paths.create_workspace_data();
    paths.create_feature_cache();
    let fake_path = fake_docker_path(&temp, "cli/fake-bin/docker-empty-clean.sh");

    decune()
        .env("PATH", &fake_path)
        .env("XDG_CACHE_HOME", &paths.cache_home)
        .env("XDG_STATE_HOME", &paths.state_home)
        .env("XDG_RUNTIME_DIR", &paths.runtime_home)
        .args(["clean", "--include-feature-cache", "--no-confirm"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "Removed stale decune-managed data",
        ));

    assert!(!paths.cache_dir.exists());
    assert!(!paths.state_dir.exists());
    assert!(!paths.runtime_dir.exists());
    assert!(!paths.feature_cache_dir.exists());
}

#[test]
fn clean_without_no_confirm_fails_non_interactive_before_removal() {
    let temp = support::TempWorkspace::new().unwrap();
    let paths = CleanTestPaths::new(&temp, WORKSPACE_ID);
    paths.create_workspace_data();
    let fake_path = fake_docker_path(&temp, "cli/fake-bin/docker-empty-clean.sh");

    decune()
        .env("PATH", &fake_path)
        .env("XDG_CACHE_HOME", &paths.cache_home)
        .env("XDG_STATE_HOME", &paths.state_home)
        .env("XDG_RUNTIME_DIR", &paths.runtime_home)
        .arg("clean")
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "Cannot confirm clean in a non-interactive terminal",
        ));

    assert!(paths.cache_dir.exists());
    assert!(paths.state_dir.exists());
    assert!(paths.runtime_dir.exists());
}

#[test]
fn clean_skips_workspace_with_reusable_managed_resource() {
    let temp = support::TempWorkspace::new().unwrap();
    let paths = CleanTestPaths::new(&temp, WORKSPACE_ID);
    paths.create_workspace_data();
    let fake_path = fake_docker_path(&temp, "cli/clean/managed-container.sh");

    decune()
        .env("PATH", &fake_path)
        .env("DECUNE_FAKE_WORKSPACE_ID", WORKSPACE_ID)
        .env("XDG_CACHE_HOME", &paths.cache_home)
        .env("XDG_STATE_HOME", &paths.state_home)
        .env("XDG_RUNTIME_DIR", &paths.runtime_home)
        .args(["clean", "--no-confirm", "--json"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .stdout(predicate::str::contains("\"reason\": \"managed_resource\""));

    assert!(paths.cache_dir.exists());
    assert!(paths.state_dir.exists());
    assert!(paths.runtime_dir.exists());
}

#[test]
fn clean_revalidates_managed_resource_before_removal() {
    let temp = support::TempWorkspace::new().unwrap();
    let paths = CleanTestPaths::new(&temp, WORKSPACE_ID);
    paths.create_workspace_data();
    let count_file = temp.path().join("ps-count");
    let fake_path = fake_docker_path(&temp, "cli/clean/becomes-managed-on-second-discovery.sh");

    let output = decune()
        .env("PATH", &fake_path)
        .env("DECUNE_FAKE_WORKSPACE_ID", WORKSPACE_ID)
        .env("DECUNE_FAKE_COUNT_FILE", &count_file)
        .env("XDG_CACHE_HOME", &paths.cache_home)
        .env("XDG_STATE_HOME", &paths.state_home)
        .env("XDG_RUNTIME_DIR", &paths.runtime_home)
        .args(["clean", "--no-confirm", "--json"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["summary"]["remove_candidates"], 0);
    assert_eq!(json["summary"]["removed"], 0);
    assert_eq!(json["summary"]["skipped"], 1);
    assert_eq!(json["targets"][0]["action"], "skip");
    assert_eq!(json["targets"][0]["reason"], "managed_resource");
    assert!(paths.cache_dir.exists());
    assert!(paths.state_dir.exists());
    assert!(paths.runtime_dir.exists());
}

#[test]
fn clean_dry_run_human_output_keeps_workspace_data() {
    let temp = support::TempWorkspace::new().unwrap();
    let paths = CleanTestPaths::new(&temp, WORKSPACE_ID);
    paths.create_workspace_data();
    let fake_path = fake_docker_path(&temp, "cli/fake-bin/docker-empty-clean.sh");

    decune()
        .env("PATH", &fake_path)
        .env("XDG_CACHE_HOME", &paths.cache_home)
        .env("XDG_STATE_HOME", &paths.state_home)
        .env("XDG_RUNTIME_DIR", &paths.runtime_home)
        .args(["clean", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Dry run completed"));

    assert!(paths.cache_dir.exists());
    assert!(paths.state_dir.exists());
    assert!(paths.runtime_dir.exists());
}

struct CleanTestPaths {
    cache_home: PathBuf,
    state_home: PathBuf,
    runtime_home: PathBuf,
    cache_dir: PathBuf,
    state_dir: PathBuf,
    runtime_dir: PathBuf,
    feature_cache_dir: PathBuf,
}

impl CleanTestPaths {
    fn new(temp: &support::TempWorkspace, workspace_id: &str) -> Self {
        let cache_home = temp.path().join("cache-home");
        let state_home = temp.path().join("state-home");
        let runtime_home = temp.path().join("runtime-home");
        let cache_dir = cache_home.join("decune").join(workspace_id);
        let state_dir = state_home.join("decune").join(workspace_id);
        let runtime_dir = runtime_home.join("decune").join(workspace_id);
        let feature_cache_dir = cache_home.join("decune/features");
        Self {
            cache_home,
            state_home,
            runtime_home,
            cache_dir,
            state_dir,
            runtime_dir,
            feature_cache_dir,
        }
    }

    fn create_workspace_data(&self) {
        fs::create_dir_all(&self.cache_dir).must();
        fs::create_dir_all(&self.state_dir).must();
        fs::create_dir_all(&self.runtime_dir).must();
        fs::write(self.cache_dir.join("cache-marker"), "cache\n").must();
        fs::write(self.state_dir.join("state-marker"), "state\n").must();
        fs::write(self.runtime_dir.join("runtime-marker"), "runtime\n").must();
    }

    fn create_feature_cache(&self) {
        fs::create_dir_all(&self.feature_cache_dir).must();
        fs::write(self.feature_cache_dir.join("archive.tgz"), "archive\n").must();
    }
}
