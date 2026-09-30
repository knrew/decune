use crate::harness::*;

const UNREACHABLE_DOCKER_FIXTURE: &str = "cli/fake-bin/docker-unreachable.sh";
const UNREACHABLE_DOCKER_MESSAGE: &str = "Cannot connect to the Docker daemon";

struct XdgRoots {
    state: PathBuf,
    cache: PathBuf,
    config: PathBuf,
    runtime: PathBuf,
}

impl XdgRoots {
    fn new(temp: &support::TempWorkspace) -> Self {
        Self {
            state: temp.create_dir("state").must(),
            cache: temp.create_dir("cache").must(),
            config: temp.create_dir("config").must(),
            runtime: temp.create_dir("runtime").must(),
        }
    }

    fn state_file(&self, workspace_root: &Path) -> PathBuf {
        self.state
            .join("decune")
            .join(workspace_id(workspace_root))
            .join("state.toml")
    }

    fn apply(&self, command: &mut TestCommand) {
        command
            .env("XDG_STATE_HOME", &self.state)
            .env("XDG_CACHE_HOME", &self.cache)
            .env("XDG_CONFIG_HOME", &self.config)
            .env("XDG_RUNTIME_DIR", &self.runtime);
    }
}

fn write_state_file(roots: &XdgRoots, workspace_root: &Path, exec_context: &str) {
    let state_file = roots.state_file(workspace_root);
    fs::create_dir_all(state_file.parent().must()).must();
    fs::write(
        state_file,
        format!(
            r#"version = 1
workspace = "{}"
container_id = "container-id"
image = "decune:test"
config_hash = "hash"
created_at = "unix:1"
last_started_at = "unix:1"
last_used_at = "unix:1"
{exec_context}"#,
            workspace_root.display()
        ),
    )
    .must();
}

const RECORDED_EXEC_CONTEXT: &str = r#"
[exec_context]
container_id = "container-id"
remote_user = "root"
workspace_folder = "/workspaces/project"
user_env_probe = "none"
"#;

// Without a state file, or with a state file that has no exec context, `exec` asks for
// `decune up` and exits with 1 before running Docker.
#[test]
fn exec_without_recorded_context_asks_for_up_without_running_docker() {
    for exec_context in [None, Some("")] {
        let temp = support::TempWorkspace::new().must();
        let workspace = temp.create_dir("workspace").must().canonicalize().must();
        let roots = XdgRoots::new(&temp);
        if let Some(exec_context) = exec_context {
            write_state_file(&roots, &workspace, exec_context);
        }
        let fake_path = fake_docker_path(&temp, UNREACHABLE_DOCKER_FIXTURE);

        let mut command = decune();
        roots.apply(&mut command);
        command
            .args(["exec"])
            .arg(&workspace)
            .args(["--", "true"])
            .env("PATH", &fake_path)
            .assert()
            .code(1)
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Run decune up"))
            .stderr(predicate::str::contains(UNREACHABLE_DOCKER_MESSAGE).not());
    }
}

// When Docker cannot be reached, `exec` fails with decune's own exit code 1.
#[test]
fn exec_reports_unreachable_docker_with_exit_code_one() {
    let temp = support::TempWorkspace::new().must();
    let workspace = temp.create_dir("workspace").must().canonicalize().must();
    let roots = XdgRoots::new(&temp);
    write_state_file(&roots, &workspace, RECORDED_EXEC_CONTEXT);
    let fake_path = fake_docker_path(&temp, UNREACHABLE_DOCKER_FIXTURE);

    let mut command = decune();
    roots.apply(&mut command);
    command
        .args(["exec"])
        .arg(&workspace)
        .args(["--", "true"])
        .env("PATH", &fake_path)
        .assert()
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(UNREACHABLE_DOCKER_MESSAGE));
}
