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

// When `clean` removes the state of a workspace without Docker resources as stale data, the
// exec context goes with it, and `exec` asks for `decune up`.
#[test]
fn exec_asks_for_up_after_clean_removes_stale_state() {
    let temp = support::TempWorkspace::new().must();
    let workspace = temp.create_dir("workspace").must().canonicalize().must();
    let roots = XdgRoots::new(&temp);
    write_state_file(&roots, &workspace, RECORDED_EXEC_CONTEXT);
    let clean_bin = support::TempWorkspace::new().must();
    let exec_bin = support::TempWorkspace::new().must();

    let mut clean = decune();
    roots.apply(&mut clean);
    clean
        .args(["clean", "--no-confirm"])
        .env(
            "PATH",
            fake_docker_path(&clean_bin, "cli/fake-bin/docker-empty-clean.sh"),
        )
        .assert()
        .success();
    assert!(!roots.state_file(&workspace).exists());

    let mut exec = decune();
    roots.apply(&mut exec);
    exec.args(["exec"])
        .arg(&workspace)
        .args(["--", "true"])
        .env(
            "PATH",
            fake_docker_path(&exec_bin, UNREACHABLE_DOCKER_FIXTURE),
        )
        .assert()
        .code(1)
        .stderr(predicate::str::contains("Run decune up"))
        .stderr(predicate::str::contains(UNREACHABLE_DOCKER_MESSAGE).not());
}

const RECORDED_CONTEXT_FIXTURE: &str = "cli/workspaces/exec/recorded-context";
const EXEC_SECRET: &str = "decune-exec-secret-value";

/// Workspace for a Docker test of `exec`, with XDG directories of its own so that the state,
/// runtime, and config files of the test are the only ones decune sees.
struct ExecWorkspace {
    workspace: support::TempWorkspace,
    root: PathBuf,
    _xdg: support::TempWorkspace,
    roots: XdgRoots,
}

impl ExecWorkspace {
    fn new() -> Self {
        let workspace = support::TempWorkspace::new().must();
        let root = workspace.path().canonicalize().must();
        let xdg = support::TempWorkspace::new().must();
        let roots = XdgRoots::new(&xdg);
        Self {
            workspace,
            root,
            _xdg: xdg,
            roots,
        }
    }

    fn from_fixture(fixture: &str) -> Self {
        let workspace = Self::new();
        workspace.workspace.copy_fixture_dir(fixture).must();
        workspace
    }

    fn write_file(&self, path: &str, contents: &str) {
        self.workspace.write_file(path, contents).must();
    }

    fn decune(&self) -> TestCommand {
        let mut command = decune();
        self.roots.apply(&mut command);
        command.current_dir(&self.root);
        command
    }

    fn up_detach(&self) {
        self.decune()
            .args(["up", "--detach"])
            .env("DECUNE_TEST_EXEC_SECRET", EXEC_SECRET)
            .env("DECUNE_TEST_EXEC_LOCAL", "local-at-up")
            .assert()
            .success()
            .stdout(predicate::str::is_empty());
    }

    fn exec(&self, command: &[&str]) -> TestCommand {
        let mut decune = self.decune();
        decune.arg("exec").arg("--").args(command);
        decune
    }

    fn container_id(&self) -> String {
        inspect_single_workspace_container(&self.root)
            .must()
            .id
            .must()
    }

    fn container_path_exists(&self, path: &str) -> bool {
        docker_status(["exec", &self.container_id(), "test", "-e", path]).is_ok()
    }

    fn run(&self, body: impl FnOnce(&Self) + std::panic::UnwindSafe) {
        with_clean_workspace_containers_and_images(&self.root, || body(self));
    }

    fn workspace_folder(&self) -> String {
        format!(
            "/workspaces/{}",
            self.root.file_name().must().to_string_lossy()
        )
    }
}

fn exec_stdout(command: &mut assert_cmd::Command) -> String {
    let output = command.assert().success().get_output().stdout.clone();
    String::from_utf8(output).must()
}

// `exec` runs the command as the recorded remote user in the workspace folder, with the
// userEnvProbe result under remoteEnv, remoteEnv expanded at exec time, stdin connected, and
// no TTY when stdin and stdout are not terminals. Its stdout is the command output alone.
#[test]
fn exec_runs_command_in_the_recorded_context_of_up() {
    let workspace = ExecWorkspace::from_fixture(RECORDED_CONTEXT_FIXTURE);
    workspace.run(|workspace| {
        workspace.up_detach();

        let stdout = exec_stdout(
            workspace
                .exec(&[
                    "sh",
                    "-c",
                    r#"printf '%s\n' "user=$(id -un)" "pwd=$(pwd)" "home_bin=$DECUNE_HOME_BIN" "local=$DECUNE_LOCAL" "probed=$DECUNE_PROBED" "shared=$DECUNE_SHARED"
[ -n "$DECUNE_REMOTE_SECRET" ] && [ "$DECUNE_REMOTE_SECRET" = "$DECUNE_EXEC_SECRET" ] && echo remote_secret=from-container-env
[ -t 1 ] || echo tty=none
cat"#,
                ])
                .env("DECUNE_TEST_EXEC_LOCAL", "local-at-exec")
                .write_stdin("from-stdin\n"),
        );

        assert_eq!(
            stdout,
            format!(
                "user=decune\npwd={}\nhome_bin=/home/decune/bin\nlocal=local-at-exec\nprobed=from-login-shell\nshared=from-remote-env\nremote_secret=from-container-env\ntty=none\nfrom-stdin\n",
                workspace.workspace_folder()
            )
        );
    });
}

// `exec` returns the exit code of the command, including the 127 that `docker exec` returns
// when the command does not exist. Docker writes its own error for that case to stdout, so
// only the exit code is checked there.
#[test]
fn exec_returns_the_exit_code_of_the_command() {
    let workspace = ExecWorkspace::new();
    workspace.write_file(
        ".devcontainer/devcontainer.json",
        r#"{ "image": "alpine:3.20" }"#,
    );
    workspace.run(|workspace| {
        workspace.up_detach();

        workspace
            .exec(&["sh", "-c", "echo out; echo err >&2; exit 7"])
            .assert()
            .code(7)
            .stdout("out\n")
            .stderr(predicate::str::contains("err\n"));
        workspace
            .exec(&["decune-no-such-command"])
            .assert()
            .code(127);
    });
}

// A remoteEnv reference to a local variable that is unset when `exec` runs is an error, and
// the command does not run.
#[test]
fn exec_fails_without_running_when_remote_env_reference_is_unset() {
    let workspace = ExecWorkspace::from_fixture(RECORDED_CONTEXT_FIXTURE);
    workspace.run(|workspace| {
        workspace.up_detach();

        workspace
            .exec(&["touch", "/tmp/decune-exec-ran"])
            .env_remove("DECUNE_TEST_EXEC_LOCAL")
            .assert()
            .code(1)
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("DECUNE_TEST_EXEC_LOCAL"));

        assert!(!workspace.container_path_exists("/tmp/decune-exec-ran"));
    });
}

// Secret-sensitive values that `exec` expands stay out of decune's output, the state
// directory, and the container labels.
#[test]
fn exec_keeps_secret_values_out_of_output_state_and_labels() {
    let workspace = ExecWorkspace::from_fixture(RECORDED_CONTEXT_FIXTURE);
    workspace.run(|workspace| {
        workspace.up_detach();

        let output = workspace
            .exec(&[
                "sh",
                "-c",
                r#"test "$DECUNE_REMOTE_SECRET" = "$DECUNE_EXEC_SECRET""#,
            ])
            .env("DECUNE_TEST_EXEC_LOCAL", "decune-exec-local-value")
            .assert()
            .success()
            .get_output()
            .clone();

        let decune_output = [output.stdout, output.stderr].concat();
        let decune_output = String::from_utf8_lossy(&decune_output);
        let labels = inspect_single_workspace_container(&workspace.root)
            .must()
            .config
            .must()
            .labels
            .unwrap_or_default();
        let state_files = file_contents(&workspace.roots.state);
        for secret in [EXEC_SECRET, "decune-exec-local-value"] {
            assert!(!decune_output.contains(secret), "{decune_output}");
            assert!(
                labels.values().all(|value| !value.contains(secret)),
                "container labels leaked {secret}"
            );
            for (path, contents) in &state_files {
                assert!(
                    !contents.contains(secret),
                    "state file {} leaked {secret}",
                    path.display()
                );
            }
        }
    });
}

fn file_contents(root: &Path) -> Vec<(PathBuf, String)> {
    let mut files = Vec::new();
    for entry in walk(root) {
        if entry.is_file() {
            let contents = String::from_utf8_lossy(&fs::read(&entry).must()).into_owned();
            files.push((entry, contents));
        }
    }
    files
}

fn walk(root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return paths;
    };
    for entry in entries {
        let path = entry.must().path();
        let file_type = fs::symlink_metadata(&path).must().file_type();
        paths.push(path.clone());
        if file_type.is_dir() {
            paths.extend(walk(&path));
        }
    }
    paths.sort();
    paths
}

const ATTACHED_DOCKERFILE: &str = r"
FROM alpine:3.20
RUN adduser -D decune \
    && printf '#!/bin/sh\nexit 3\n' >/usr/local/bin/decune-exit-3 \
    && chmod +x /usr/local/bin/decune-exit-3
";

// An attached `up` records the context before it connects the shell, so the record stays
// after the shell exits with a non-zero code, and `exec` then runs in that context.
#[test]
fn exec_uses_the_context_recorded_by_an_attached_up() {
    let workspace = ExecWorkspace::new();
    workspace.write_file(".devcontainer/Dockerfile", ATTACHED_DOCKERFILE);
    workspace.write_file(
        ".devcontainer/devcontainer.json",
        r#"
        {
          "build": { "dockerfile": "Dockerfile" },
          "remoteUser": "decune",
          "shutdownAction": "none"
        }
        "#,
    );
    workspace.write_file(
        ".decune/config.toml",
        "version = 1\nshell = \"/usr/local/bin/decune-exit-3\"\n",
    );
    workspace.run(|workspace| {
        workspace.decune().arg("up").assert().code(3);

        workspace
            .exec(&["id", "-un"])
            .assert()
            .success()
            .stdout("decune\n");
    });
}

// When userEnvProbe cannot run because the login shell of the remote user does not exist,
// `exec` warns and still runs the command.
#[test]
fn exec_warns_and_runs_the_command_when_user_env_probe_fails() {
    let workspace = ExecWorkspace::new();
    workspace.write_file(
        ".devcontainer/Dockerfile",
        "FROM alpine:3.20\nRUN adduser -D -s /nonexistent/decune-shell decune\n",
    );
    workspace.write_file(
        ".devcontainer/devcontainer.json",
        r#"
        {
          "build": { "dockerfile": "Dockerfile" },
          "remoteUser": "decune",
          "userEnvProbe": "loginShell"
        }
        "#,
    );
    workspace.run(|workspace| {
        workspace.up_detach();

        workspace
            .exec(&["echo", "ran"])
            .assert()
            .success()
            .stdout("ran\n")
            .stderr(predicate::str::contains("User environment probe failed"));
    });
}

// The userEnvProbe failure warning of `exec` carries the exit code and the stderr tail, leaves
// out the probe stdout, and hides the whole value of every secret-sensitive containerEnv key,
// including a key that remoteEnv does not reference.
#[test]
fn exec_user_env_probe_failure_warning_omits_stdout_and_redacts_secrets() {
    let workspace =
        ExecWorkspace::from_fixture("cli/workspaces/lifecycle/user-env-probe-failure-output");
    workspace.run(|workspace| {
        workspace
            .decune()
            .args(["up", "--detach"])
            .env("DECUNE_TEST_PROBE_SECRET", "probe-secret-value")
            .env(
                "DECUNE_TEST_PROBE_UNREFERENCED_SECRET",
                "probe-unreferenced-secret",
            )
            .assert()
            .success();

        let output = workspace
            .exec(&["echo", "ran"])
            .assert()
            .success()
            .stdout("ran\n")
            .get_output()
            .clone();
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert!(stderr.contains("User environment probe failed"), "{stderr}");
        assert!(stderr.contains("exit code 23"), "{stderr}");
        assert!(stderr.contains("decune-probe-startup-failed"), "{stderr}");
        assert!(!stderr.contains("probe-stdout-marker"), "{stderr}");
        assert!(!stderr.contains("Bearer probe-secret-value"), "{stderr}");
        assert!(!stderr.contains("probe-unreferenced-secret"), "{stderr}");
    });
}

// Inside a Git repository, a subdirectory as WORKSPACE or as the current directory resolves
// to the repository root, so `exec` uses the context that `up` recorded there.
#[test]
fn exec_from_a_subdirectory_uses_the_git_repository_root() {
    let workspace = ExecWorkspace::new();
    workspace.write_file(
        ".devcontainer/devcontainer.json",
        r#"{ "image": "alpine:3.20" }"#,
    );
    let subdirectory = workspace.workspace.create_dir("nested/dir").must();
    std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(&workspace.root)
        .status()
        .must();
    workspace.run(|workspace| {
        workspace.up_detach();

        let mut by_argument = workspace.decune();
        by_argument
            .arg("exec")
            .arg(&subdirectory)
            .args(["--", "pwd"]);
        let mut by_current_dir = workspace.exec(&["pwd"]);
        by_current_dir.current_dir(&subdirectory);

        for command in [&mut by_argument, &mut by_current_dir] {
            command
                .assert()
                .success()
                .stdout(format!("{}\n", workspace.workspace_folder()));
        }
    });
}

// `exec` keeps the context of the last `up` after `devcontainer.json` changes or disappears,
// and a `rebuild` replaces the context with its own.
#[test]
fn exec_follows_the_last_up_rather_than_the_current_config() {
    let workspace = ExecWorkspace::new();
    workspace.write_file(
        ".devcontainer/Dockerfile",
        "FROM alpine:3.20\nRUN adduser -D decune\n",
    );
    let config = |remote_user: &str| {
        format!(r#"{{ "build": {{ "dockerfile": "Dockerfile" }}, "remoteUser": "{remote_user}" }}"#)
    };
    workspace.write_file(".devcontainer/devcontainer.json", &config("root"));
    workspace.run(|workspace| {
        workspace.up_detach();

        workspace.write_file(".devcontainer/devcontainer.json", &config("decune"));
        workspace
            .exec(&["id", "-un"])
            .assert()
            .success()
            .stdout("root\n");

        workspace
            .decune()
            .args(["rebuild", "--detach"])
            .assert()
            .success();
        workspace
            .exec(&["id", "-un"])
            .assert()
            .success()
            .stdout("decune\n");

        fs::remove_dir_all(workspace.root.join(".devcontainer")).must();
        workspace
            .exec(&["id", "-un"])
            .assert()
            .success()
            .stdout("decune\n");
    });
}

fn assert_exec_asks_for_up_without_running(workspace: &ExecWorkspace, container: &str) {
    workspace
        .exec(&["touch", "/tmp/decune-exec-ran"])
        .assert()
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Run decune up"));
    let marker = docker_status(["exec", container, "test", "-e", "/tmp/decune-exec-ran"]);
    assert!(marker.is_err(), "exec ran the command in {container}");
}

/// Rewrites the container ID of the exec context in the state file.
fn replace_recorded_container_id(workspace: &ExecWorkspace, from: &str, to: &str) {
    let state_file = workspace.roots.state_file(&workspace.root);
    let state = fs::read_to_string(&state_file).must();
    assert!(state.contains(from), "{state}");
    fs::write(&state_file, state.replace(from, to)).must();
}

// `exec` runs only in the recorded container while it is running and belongs to this
// workspace. Otherwise it asks for `decune up`, starts nothing, and does not run the command.
//
// Scenario:
//   1. Pause the recorded container, then unpause it
//   2. Record a running container without decune labels, then one of another workspace
//   3. Stop the recorded container; it stays stopped
//   4. Replace the container with one of the same name and labels, as a recreation outside
//      decune would
#[test]
fn exec_runs_only_in_the_running_recorded_container_of_the_workspace() {
    let workspace = ExecWorkspace::new();
    workspace.write_file(
        ".devcontainer/devcontainer.json",
        r#"{ "image": "alpine:3.20" }"#,
    );
    workspace.run(|workspace| {
        workspace.up_detach();
        let container = workspace.container_id();

        docker_status(["pause", &container]).must();
        let paused = std::panic::catch_unwind(|| {
            workspace
                .exec(&["true"])
                .assert()
                .code(1)
                .stderr(predicate::str::contains("Run decune up"));
        });
        docker_status(["unpause", &container]).must();
        if let Err(payload) = paused {
            std::panic::resume_unwind(payload);
        }

        let other_workspace_id = "0123456789ab";
        for labels in [
            vec![],
            vec![
                "--label".to_owned(),
                "decune.managed=true".to_owned(),
                "--label".to_owned(),
                format!("decune.workspace_id={other_workspace_id}"),
            ],
        ] {
            let mut args = vec!["run".to_owned(), "--detach".to_owned()];
            args.extend(labels);
            args.extend([
                "alpine:3.20".to_owned(),
                "sleep".to_owned(),
                "600".to_owned(),
            ]);
            let foreign = docker_output(&args).must().trim().to_owned();
            replace_recorded_container_id(workspace, &container, &foreign);
            let result = std::panic::catch_unwind(|| {
                assert_exec_asks_for_up_without_running(workspace, &foreign);
            });
            replace_recorded_container_id(workspace, &foreign, &container);
            _ = docker_status(["rm", "--force", &foreign]);
            if let Err(payload) = result {
                std::panic::resume_unwind(payload);
            }
        }

        docker_status(["stop", &container]).must();
        workspace
            .exec(&["true"])
            .assert()
            .code(1)
            .stderr(predicate::str::contains("Run decune up"));
        assert_container_is_not_running(&container);

        let inspect = inspect_single_workspace_container(&workspace.root).must();
        let name = inspect.name.must().trim_start_matches('/').to_owned();
        let labels = inspect.config.must().labels.must();
        docker_status(["rm", "--force", &container]).must();
        let mut args = vec![
            "run".to_owned(),
            "--detach".to_owned(),
            "--name".to_owned(),
            name,
        ];
        for (key, value) in labels {
            args.extend(["--label".to_owned(), format!("{key}={value}")]);
        }
        args.extend([
            "alpine:3.20".to_owned(),
            "sleep".to_owned(),
            "600".to_owned(),
        ]);
        let replacement = docker_output(&args).must().trim().to_owned();
        assert_exec_asks_for_up_without_running(workspace, &replacement);
    });
}

// A later `up` that reuses the same container keeps the recorded context even when it fails
// in a lifecycle command, so `exec` still runs.
#[test]
fn exec_keeps_working_after_an_up_of_the_same_container_fails() {
    let workspace = ExecWorkspace::new();
    workspace.write_file(
        ".devcontainer/Dockerfile",
        "FROM alpine:3.20\nRUN adduser -D decune\n",
    );
    workspace.write_file(
        ".devcontainer/devcontainer.json",
        r#"
        {
          "build": { "dockerfile": "Dockerfile" },
          "remoteUser": "decune",
          "postAttachCommand": "test ! -e /tmp/decune-fail-post-attach"
        }
        "#,
    );
    workspace.run(|workspace| {
        workspace.up_detach();
        let container = workspace.container_id();
        docker_status(["exec", &container, "touch", "/tmp/decune-fail-post-attach"]).must();

        workspace
            .decune()
            .arg("up")
            .assert()
            .failure()
            .stderr(predicate::str::contains("postAttachCommand"));

        assert_eq!(workspace.container_id(), container);
        workspace
            .exec(&["id", "-un"])
            .assert()
            .success()
            .stdout("decune\n");
    });
}

// A `rebuild` that recreates the container and then fails leaves no usable context, so
// `exec` asks for `decune up` and does not run the command in the new container.
#[test]
fn exec_asks_for_up_after_a_failed_rebuild() {
    let workspace = ExecWorkspace::new();
    workspace.write_file(
        ".devcontainer/devcontainer.json",
        r#"
        {
          "image": "alpine:3.20",
          "postCreateCommand": "test ! -e .decune-fail-post-create"
        }
        "#,
    );
    workspace.run(|workspace| {
        workspace.up_detach();
        workspace.write_file(".decune-fail-post-create", "");

        workspace
            .decune()
            .args(["rebuild", "--detach"])
            .assert()
            .failure()
            .stderr(predicate::str::contains("postCreateCommand"));

        assert_exec_asks_for_up_without_running(workspace, &workspace.container_id());
    });
}

// After `remove`, the recorded context is gone and `exec` asks for `decune up`.
#[test]
fn exec_asks_for_up_after_remove() {
    let workspace = ExecWorkspace::new();
    workspace.write_file(
        ".devcontainer/devcontainer.json",
        r#"{ "image": "alpine:3.20" }"#,
    );
    workspace.run(|workspace| {
        workspace.up_detach();

        workspace
            .decune()
            .args(["remove", "--no-confirm"])
            .assert()
            .success();

        workspace
            .exec(&["true"])
            .assert()
            .code(1)
            .stderr(predicate::str::contains("Run decune up"));
    });
}

/// Runs `decune exec -- true` with a `docker` wrapper first on `PATH` and returns the
/// arguments of each docker call that went through the wrapper.
fn logged_docker_calls(workspace: &ExecWorkspace) -> Vec<String> {
    let real_docker = std::process::Command::new("sh")
        .args(["-c", "command -v docker"])
        .output()
        .must()
        .stdout;
    let real_docker = String::from_utf8(real_docker).must().trim().to_owned();
    let bin = support::TempWorkspace::new().must();
    let fake_path = fake_docker_path(&bin, "cli/exec/docker-argv-log.sh");
    let argv_log = bin.path().join("docker-argv.log");

    workspace
        .exec(&["true"])
        .env("PATH", &fake_path)
        .env("DECUNE_TEST_DOCKER_ARGV_LOG", &argv_log)
        .env("DECUNE_TEST_REAL_DOCKER", &real_docker)
        .env("DECUNE_TEST_EXEC_LOCAL", "local-at-exec")
        .assert()
        .success();

    fs::read_to_string(&argv_log)
        .must()
        .lines()
        .map(ToOwned::to_owned)
        .collect()
}

// Without userEnvProbe, the docker calls of `exec` are the inspect of the recorded container
// and the `docker exec` of the command, and nothing else.
#[test]
fn exec_only_inspects_the_target_and_runs_the_command() {
    let workspace = ExecWorkspace::new();
    workspace.write_file(
        ".devcontainer/devcontainer.json",
        r#"{ "image": "alpine:3.20", "userEnvProbe": "none" }"#,
    );
    workspace.run(|workspace| {
        workspace.up_detach();
        let container = workspace.container_id();

        let calls = logged_docker_calls(workspace);

        assert_eq!(calls.len(), 2, "{calls:#?}");
        assert_eq!(calls[0], format!("container inspect -- {container}"));
        assert!(calls[1].starts_with("exec --interactive "), "{calls:#?}");
        assert!(
            calls[1].ends_with(&format!(" {container} true")),
            "{calls:#?}"
        );
    });
}

// With userEnvProbe, `exec` runs the probe with the recorded login shell after the inspect.
// It calls no Compose command and does not read passwd.
//
// The command itself runs with the probed `PATH`, which can resolve `docker` without the
// wrapper, so only the calls up to the probe are checked in order.
#[test]
fn exec_runs_user_env_probe_without_compose_or_passwd_calls() {
    let workspace = ExecWorkspace::from_fixture(RECORDED_CONTEXT_FIXTURE);
    workspace.run(|workspace| {
        workspace.up_detach();
        let container = workspace.container_id();

        let calls = logged_docker_calls(workspace);

        assert!(calls.len() >= 2, "{calls:#?}");
        assert_eq!(calls[0], format!("container inspect -- {container}"));
        assert_eq!(
            calls[1],
            format!(
                "exec --user decune {container} /usr/local/bin/decune-recorded-login-shell -lc env"
            )
        );
        for call in &calls {
            assert!(
                call.starts_with("container inspect ") || call.starts_with("exec "),
                "{calls:#?}"
            );
            assert!(!call.contains("compose"), "{calls:#?}");
            assert!(!call.contains("passwd"), "{calls:#?}");
        }
    });
}

// `exec` uses the home directory and the login shell that `up` resolved, even after the
// passwd entry of the remote user changes in the container. `$HOME` is not checked, because
// `docker exec --user` sets it from the current passwd.
#[test]
fn exec_uses_the_recorded_home_and_shell_after_passwd_changes() {
    let workspace = ExecWorkspace::from_fixture(RECORDED_CONTEXT_FIXTURE);
    workspace.run(|workspace| {
        workspace.up_detach();
        let container = workspace.container_id();
        docker_status([
            "exec",
            &container,
            "sed",
            "-i",
            "s#^decune:\\(.*\\):/home/decune:.*#decune:\\1:/tmp/decune-new-home:/bin/sh#",
            "/etc/passwd",
        ])
        .must();
        let passwd = docker_output(["exec", &container, "grep", "^decune:", "/etc/passwd"]).must();
        assert!(passwd.contains("/tmp/decune-new-home:/bin/sh"), "{passwd}");

        workspace
            .exec(&[
                "sh",
                "-c",
                r#"printf '%s\n' "home_bin=$DECUNE_HOME_BIN" "probed=$DECUNE_PROBED""#,
            ])
            .env("DECUNE_TEST_EXEC_LOCAL", "local-at-exec")
            .assert()
            .success()
            .stdout("home_bin=/home/decune/bin\nprobed=from-login-shell\n");
    });
}

#[derive(Debug, PartialEq, Eq)]
struct SideEffectSnapshot {
    files: Vec<(PathBuf, Option<Vec<u8>>)>,
    containers: Vec<(Option<String>, Option<String>)>,
    images: Vec<String>,
    volumes: Vec<String>,
    networks: String,
}

/// Captures what `exec` must leave unchanged: the files of the workspace and of the decune
/// state, runtime, and cache directories, and the Docker resources of the workspace.
fn side_effect_snapshot(workspace: &ExecWorkspace) -> SideEffectSnapshot {
    let roots = [
        &workspace.root,
        &workspace.roots.state,
        &workspace.roots.runtime,
        &workspace.roots.cache,
    ];
    let files = roots
        .into_iter()
        .flat_map(|root| walk(root))
        .map(|path| {
            let contents = fs::symlink_metadata(&path)
                .must()
                .is_file()
                .then(|| fs::read(&path).must());
            (path, contents)
        })
        .collect();
    let containers = workspace_containers(&workspace.root)
        .must()
        .into_iter()
        .map(|container| (container.id, container.state))
        .collect();
    let networks = docker_output([
        "network",
        "ls",
        "--filter",
        &format!(
            "label=decune.workspace_id={}",
            workspace_id(&workspace.root)
        ),
        "--format",
        "{{.ID}}",
    ])
    .must();

    SideEffectSnapshot {
        files,
        containers,
        images: workspace_images(&workspace.root).must(),
        volumes: workspace_volumes(&workspace.root).must(),
        networks,
    }
}

// `exec` runs no lifecycle command or decune hook, and changes no state, runtime file, cache,
// lock file, or Docker resource. It starts no decune host daemon, and the container keeps
// running afterwards, because `shutdownAction` does not apply.
#[test]
fn exec_has_no_side_effects_beyond_the_command() {
    let workspace = ExecWorkspace::new();
    workspace.write_file(
        ".devcontainer/devcontainer.json",
        r#"
        {
          "image": "alpine:3.20",
          "initializeCommand": "printf i >>.decune-lifecycle-markers",
          "onCreateCommand": "printf o >>.decune-lifecycle-markers",
          "updateContentCommand": "printf u >>.decune-lifecycle-markers",
          "postCreateCommand": "printf c >>.decune-lifecycle-markers",
          "postStartCommand": "printf s >>.decune-lifecycle-markers",
          "postAttachCommand": "printf a >>.decune-lifecycle-markers"
        }
        "#,
    );
    workspace.write_file(
        ".decune/config.toml",
        r#"
version = 1

[[hooks.after_post_start]]
command = "printf h >>.decune-hook-markers"
where = "host"

[[hooks.before_post_attach]]
command = "printf H >>.decune-hook-markers"
where = "container"
"#,
    );
    workspace.run(|workspace| {
        workspace.up_detach();
        assert!(workspace.root.join(".decune-lifecycle-markers").exists());
        assert!(workspace.root.join(".decune-hook-markers").exists());
        let before = side_effect_snapshot(workspace);

        workspace.exec(&["true"]).assert().success();

        assert_eq!(side_effect_snapshot(workspace), before);
        assert!(
            walk(&workspace.roots.runtime).iter().all(|path| path
                .file_name()
                .is_none_or(|name| name != "host-daemon.sock")),
            "exec left a host daemon socket"
        );
    });
}
