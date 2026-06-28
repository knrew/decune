use serde_json::Value;

use crate::harness::*;

const WORKSPACE_ID: &str = "123456abcdef";

#[test]
fn clean_dry_run_json_reports_stale_workspace_without_removing_it() {
    let temp = support::TempWorkspace::new().unwrap();
    let paths = CleanTestPaths::new(&temp, WORKSPACE_ID);
    paths.create_workspace_data();
    paths.create_feature_cache();
    let fake_path = fake_docker_path(&temp, fake_docker_no_managed());

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
    let fake_path = fake_docker_path(&temp, fake_docker_no_managed());

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
            "Removed stale decune generated data",
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
    let fake_path = fake_docker_path(&temp, fake_docker_no_managed());

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
            "Removed stale decune generated data",
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
    let fake_path = fake_docker_path(&temp, fake_docker_no_managed());

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
    let fake_path = fake_docker_path(&temp, fake_docker_with_managed_container(WORKSPACE_ID));

    decune()
        .env("PATH", &fake_path)
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
    let fake_path = fake_docker_path(
        &temp,
        fake_docker_becomes_managed_on_second_discovery(
            WORKSPACE_ID,
            &temp.path().join("ps-count"),
        ),
    );

    let output = decune()
        .env("PATH", &fake_path)
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
    let fake_path = fake_docker_path(&temp, fake_docker_no_managed());

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

fn fake_docker_path(temp: &support::TempWorkspace, content: String) -> String {
    let bin_dir = temp.path().join("bin");
    fs::create_dir_all(&bin_dir).must();
    let docker_path = bin_dir.join("docker");
    fs::write(&docker_path, content).must();
    fs::set_permissions(&docker_path, fs::Permissions::from_mode(0o755)).must();
    format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

fn fake_docker_no_managed() -> String {
    r#"#!/usr/bin/env bash
set -euo pipefail

if [ "${1:-}" = ps ]; then
  exit 0
fi
if [ "${1:-}" = volume ] && [ "${2:-}" = ls ]; then
  exit 0
fi

echo "unexpected fake docker command: $*" >&2
exit 91
"#
    .to_owned()
}

fn fake_docker_with_managed_container(workspace_id: &str) -> String {
    format!(
        r#"#!/usr/bin/env bash
set -euo pipefail

if [ "${{1:-}}" = ps ]; then
  case "$*" in
    *"label=decune.managed=true"*)
      printf '{{"ID":"managed-container"}}\n'
      exit 0
      ;;
  esac
fi

if [ "${{1:-}}" = container ] && [ "${{2:-}}" = inspect ]; then
  printf '[{{"Id":"managed-container","Name":"/managed","Config":{{"Labels":{{"decune.managed":"true","decune.workspace_id":"{workspace_id}"}}}},"State":{{"Running":true}}}}]\n'
  exit 0
fi

if [ "${{1:-}}" = volume ] && [ "${{2:-}}" = ls ]; then
  exit 0
fi

echo "unexpected fake docker command: $*" >&2
exit 91
"#
    )
}

fn fake_docker_becomes_managed_on_second_discovery(
    workspace_id: &str,
    count_file: &std::path::Path,
) -> String {
    format!(
        r#"#!/usr/bin/env bash
set -euo pipefail

count_file='{count_file}'

if [ "${{1:-}}" = ps ]; then
  count=0
  if [ -f "$count_file" ]; then
    count="$(cat "$count_file")"
  fi
  count=$((count + 1))
  printf '%s\n' "$count" > "$count_file"
  if [ "$count" -ge 2 ]; then
    printf '{{"ID":"managed-container"}}\n'
  fi
  exit 0
fi

if [ "${{1:-}}" = container ] && [ "${{2:-}}" = inspect ]; then
  printf '[{{"Id":"managed-container","Name":"/managed","Config":{{"Labels":{{"decune.managed":"true","decune.workspace_id":"{workspace_id}"}}}},"State":{{"Running":true}}}}]\n'
  exit 0
fi

if [ "${{1:-}}" = volume ] && [ "${{2:-}}" = ls ]; then
  exit 0
fi

echo "unexpected fake docker command: $*" >&2
exit 91
"#,
        count_file = count_file.display()
    )
}
