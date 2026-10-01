use serde_json::Value;

use crate::harness::*;

const WORKSPACE_ID: &str = "123456abcdef";

// Compose の状態が多数あっても、各プロジェクトの volume を調べるのは初回と削除直前だけ。
// Docker CLI の呼び出しが候補数の二乗で増えると、リモート Docker で通信待ちが累積する
#[test]
fn clean_revalidates_only_the_target_compose_project() {
    let temp = support::TempWorkspace::new().unwrap();
    let paths = CleanTestPaths::new(&temp, WORKSPACE_ID);
    for index in 0..10 {
        let workspace_id = format!("{index:012x}");
        let project = format!("project-{index}");
        temp.write_fixture_template(
            format!("state-home/decune/{workspace_id}/state.toml"),
            "cli/harness/compose-state.toml",
            &[("__PROJECT__", project.as_str())],
        )
        .unwrap();
    }
    let log = temp.path().join("project-queries");
    let fake_path = fake_docker_path(&temp, "cli/clean/compose-project-revalidation.sh");

    let output = decune()
        .env("PATH", &fake_path)
        .env("DECUNE_FAKE_COMMAND_LOG", &log)
        .env("XDG_CACHE_HOME", &paths.cache_home)
        .env("XDG_STATE_HOME", &paths.state_home)
        .env("XDG_RUNTIME_DIR", &paths.runtime_home)
        .args(["clean", "--no-confirm", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["summary"]["removed"], 10);
    let queries = fs::read_to_string(log).unwrap();
    assert!(queries.lines().count() <= 20, "{queries}");
}

// 初回の探索後に Compose volume が現れたワークスペースは、削除直前の再確認で保護する
#[test]
fn clean_keeps_compose_volume_discovered_before_removal() {
    let temp = support::TempWorkspace::new().unwrap();
    let paths = CleanTestPaths::new(&temp, WORKSPACE_ID);
    temp.write_fixture_template(
        format!("state-home/decune/{WORKSPACE_ID}/state.toml"),
        "cli/harness/compose-state.toml",
        &[("__PROJECT__", "project")],
    )
    .unwrap();
    let fake_path = fake_docker_path(&temp, "cli/clean/compose-project-revalidation.sh");

    let output = decune()
        .env("PATH", &fake_path)
        .env(
            "DECUNE_FAKE_COMMAND_LOG",
            temp.path().join("project-queries"),
        )
        .env("DECUNE_FAKE_NEW_VOLUME_PROJECT", "project")
        .env("XDG_CACHE_HOME", &paths.cache_home)
        .env("XDG_STATE_HOME", &paths.state_home)
        .env("XDG_RUNTIME_DIR", &paths.runtime_home)
        .args(["clean", "--no-confirm", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["summary"]["removed"], 0);
    assert_eq!(json["targets"][0]["reason"], "managed_resource");
    assert!(paths.state_dir.join("state.toml").exists());
}

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

// コンテナが無くても、`up` が作らせた decune-managed ボリュームが残るワークスペースは、
// `clean` が再利用可能なリソースが残っているものとしてスキップする
#[test]
fn clean_skips_workspace_with_only_volume_created_by_up() {
    let workspace = support::TempWorkspace::new().must();
    let container_tools_dir = fake_container_tools_bundle(&workspace);
    let workspace_root = workspace.path().canonicalize().must();
    let workspace_id = workspace_id(&workspace_root);
    let volume = format!("decune-clean-volume-only-{workspace_id}");
    let paths = CleanTestPaths::new(&workspace, &workspace_id);
    write_named_volume_devcontainer(&workspace, &volume);

    with_clean_workspace_containers_images_and_volumes(&workspace_root, || {
        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .env("XDG_STATE_HOME", &paths.state_home)
            .env("DECUNE_CONTAINER_TOOLS_DIR", &container_tools_dir)
            .assert()
            .success();
        cleanup_workspace_containers(&workspace_root).must();
        assert!(paths.state_dir.exists());

        let output = decune()
            .env("XDG_CACHE_HOME", &paths.cache_home)
            .env("XDG_STATE_HOME", &paths.state_home)
            .env("XDG_RUNTIME_DIR", &paths.runtime_home)
            .args(["clean", "--dry-run", "--json"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();

        let json: Value = serde_json::from_slice(&output).must();
        let target = json["targets"]
            .as_array()
            .must()
            .iter()
            .find(|target| target["workspace_id"] == workspace_id.as_str())
            .must_msg(format_args!("missing clean target in {json}"));
        assert_eq!(target["reason"], "managed_resource");
    });
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
