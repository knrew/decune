use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    thread,
    time::{Duration, Instant},
};

use serde_json::Value as JsonValue;

use crate::harness::*;

const CONTAINER_CLI: &str = "/usr/local/bin/decune";
const CONTAINER_CLI_DIRECT: &str = "/run/decune/decune";
const HOST_DAEMON_SOCKET: &str = "/run/decune/host-daemon.sock";
const START_TIMEOUT: Duration = Duration::from_secs(90);
const STOP_TIMEOUT: Duration = Duration::from_secs(15);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

static SESSION_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, Copy)]
enum SupportedFixture {
    Image,
    Dockerfile,
    Compose,
}

impl SupportedFixture {
    const fn name(self) -> &'static str {
        match self {
            Self::Image => "container-cli-image",
            Self::Dockerfile => "container-cli-dockerfile",
            Self::Compose => "container-cli-compose",
        }
    }

    const fn mode(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Dockerfile => "dockerfile",
            Self::Compose => "compose",
        }
    }

    const fn remote_uid(self) -> &'static str {
        match self {
            Self::Image | Self::Dockerfile => "0",
            Self::Compose => "20001",
        }
    }
}

struct IsolatedRoots {
    root: tempfile::TempDir,
}

impl IsolatedRoots {
    fn new() -> Self {
        // Keep XDG_RUNTIME_DIR short enough for the host daemon Unix socket's sun_path limit.
        let root = tempfile::Builder::new()
            .prefix("d")
            .tempdir_in("/tmp")
            .must();
        for directory in ["state", "cache", "config", "gh", "logs"] {
            fs::create_dir_all(root.path().join(directory)).must();
        }
        Self { root }
    }

    fn state(&self) -> PathBuf {
        self.root.path().join("state")
    }

    fn cache(&self) -> PathBuf {
        self.root.path().join("cache")
    }

    fn config(&self) -> PathBuf {
        self.root.path().join("config")
    }

    fn runtime(&self) -> PathBuf {
        self.root.path().to_path_buf()
    }

    fn gh(&self) -> PathBuf {
        self.root.path().join("gh")
    }

    fn log_path(&self, stream: &str) -> PathBuf {
        let sequence = SESSION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        self.root
            .path()
            .join("logs")
            .join(format!("{sequence}-{stream}.log"))
    }
}

struct ContainerCliWorkspace {
    workspace: support::TempWorkspace,
    roots: IsolatedRoots,
}

impl ContainerCliWorkspace {
    fn from_fixture(name: &str) -> Self {
        let workspace = support::TempWorkspace::new().must();
        workspace
            .copy_fixture_dir(Path::new("compose").join(name))
            .must();
        Self {
            workspace,
            roots: IsolatedRoots::new(),
        }
    }

    fn supported(fixture: SupportedFixture) -> Self {
        let workspace = Self::from_fixture(fixture.name());
        match fixture {
            SupportedFixture::Compose => {
                let [primary_port, sidecar_port, published_port] = available_localhost_ports();
                let primary_port = primary_port.to_string();
                let sidecar_port = sidecar_port.to_string();
                let published_port = published_port.to_string();
                workspace.render_config(
                    fixture.name(),
                    &[
                        ("\"__PRIMARY_HOST_PORT__\"", primary_port.as_str()),
                        ("\"__SIDECAR_HOST_PORT__\"", sidecar_port.as_str()),
                    ],
                );
                workspace.render_compose(
                    fixture.name(),
                    &[("__PUBLISHED_HOST_PORT__", published_port.as_str())],
                );
            }
            SupportedFixture::Image | SupportedFixture::Dockerfile => {
                let primary_port = available_localhost_port().to_string();
                workspace.render_config(
                    fixture.name(),
                    &[("\"__PRIMARY_HOST_PORT__\"", primary_port.as_str())],
                );
            }
        }
        workspace
    }

    fn render_config(&self, fixture: &str, replacements: &[(&str, &str)]) {
        self.workspace
            .write_fixture_template(
                ".decune/config.toml",
                Path::new("compose")
                    .join(fixture)
                    .join(".decune/config.toml"),
                replacements,
            )
            .must();
    }

    fn render_compose(&self, fixture: &str, replacements: &[(&str, &str)]) {
        self.workspace
            .write_fixture_template(
                ".devcontainer/compose.yaml",
                Path::new("compose")
                    .join(fixture)
                    .join(".devcontainer/compose.yaml"),
                replacements,
            )
            .must();
    }

    fn path(&self) -> PathBuf {
        self.workspace.path().canonicalize().must()
    }

    fn workspace_id(&self) -> String {
        workspace_id(&self.path())
    }

    fn state_path(&self) -> PathBuf {
        self.roots
            .state()
            .join("decune")
            .join(self.workspace_id())
            .join("state.toml")
    }

    fn state(&self) -> toml::Value {
        toml::from_str(&fs::read_to_string(self.state_path()).must()).must()
    }

    fn primary_container_id(&self) -> String {
        self.state()
            .get("container_id")
            .and_then(toml::Value::as_str)
            .map(str::to_owned)
            .must_msg("workspace state did not contain container_id")
    }

    fn compose_project_name(&self) -> String {
        self.state()
            .get("compose_project_name")
            .and_then(toml::Value::as_str)
            .map(str::to_owned)
            .must_msg("workspace state did not contain compose_project_name")
    }

    fn service_container_id(&self, service: &str) -> String {
        let output = docker_output([
            "ps",
            "--all",
            "--filter",
            &format!(
                "label=com.docker.compose.project={}",
                self.compose_project_name()
            ),
            "--filter",
            &format!("label=com.docker.compose.service={service}"),
            "--format",
            "{{.ID}}",
        ])
        .must();
        let containers = output
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(
            containers.len(),
            1,
            "expected one container for Compose service {service}: {output:?}"
        );
        containers[0].to_owned()
    }

    fn command(&self) -> Command {
        let mut command = Command::new(assert_cmd::cargo::cargo_bin("decune"));
        command
            .env("XDG_STATE_HOME", self.roots.state())
            .env("XDG_CACHE_HOME", self.roots.cache())
            .env("XDG_CONFIG_HOME", self.roots.config())
            .env("XDG_RUNTIME_DIR", self.roots.runtime())
            .env("GH_CONFIG_DIR", self.roots.gh())
            .env(
                "DECUNE_FAKE_COMPOSE_CAPABILITIES",
                fake_compose_capabilities_script_path(),
            )
            .env_remove("DECUNE_CONTAINER_TOOLS_DIR")
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .env_remove("GH_ENTERPRISE_TOKEN")
            .env_remove("GITHUB_ENTERPRISE_TOKEN");
        command
    }

    fn run_host(&self, args: &[&str]) -> Output {
        self.command().args(args).arg(self.path()).output().must()
    }

    fn start_attached(&self) -> AttachedSession {
        let stdout_path = self.roots.log_path("stdout");
        let stderr_path = self.roots.log_path("stderr");
        let mut command = self.command();
        command
            .args(["up", "--no-auto-forward"])
            .arg(self.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::from(fs::File::create(&stdout_path).must()))
            .stderr(Stdio::from(fs::File::create(&stderr_path).must()));
        AttachedSession {
            child: command.spawn().must(),
            stdout_path,
            stderr_path,
            stopped: false,
        }
    }

    fn set_container_cli_enabled(&self, enabled: bool) {
        let path = self.path().join(".decune/config.toml");
        let mut config = toml::from_str::<toml::Value>(&fs::read_to_string(&path).must()).must();
        let container_cli = config
            .get_mut("container")
            .and_then(toml::Value::as_table_mut)
            .and_then(|container| container.get_mut("cli"))
            .and_then(toml::Value::as_table_mut)
            .must_msg("container CLI config table was missing");
        container_cli.insert("enabled".to_owned(), toml::Value::Boolean(enabled));
        fs::write(path, toml::to_string_pretty(&config).must()).must();
    }

    fn write_state(&self, state: &toml::Value) {
        fs::write(self.state_path(), toml::to_string(state).must()).must();
    }
}

impl Drop for ContainerCliWorkspace {
    fn drop(&mut self) {
        let output = self
            .command()
            .args(["remove", "--no-confirm", "--images"])
            .arg(self.path())
            .output();
        if let Ok(output) = output
            && !output.status.success()
        {
            eprintln!(
                "container CLI integration cleanup failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        _ = cleanup_workspace_containers(&self.path());
        _ = cleanup_workspace_images(&self.path());
    }
}

struct AttachedSession {
    child: Child,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    stopped: bool,
}

impl AttachedSession {
    fn wait_for_started(&mut self) {
        let deadline = Instant::now() + START_TIMEOUT;
        while Instant::now() < deadline {
            let stderr = self.stderr();
            if stderr.contains("Started dev container")
                || stderr.contains("Done: Reusing running dev container")
            {
                return;
            }
            self.assert_running("waiting for attached up to start");
            thread::sleep(POLL_INTERVAL);
        }
        test_fail(format_args!(
            "attached up did not start before timeout; stdout={:?}; stderr={:?}",
            self.stdout(),
            self.stderr()
        ));
    }

    fn wait_for_query(&mut self, workspace: &ContainerCliWorkspace, user: &str) -> String {
        let deadline = Instant::now() + START_TIMEOUT;
        let mut last = None;
        while Instant::now() < deadline {
            if workspace.state_path().is_file() {
                let container = workspace.primary_container_id();
                let output = exec_container_cli(&container, Some(user), CONTAINER_CLI, &["status"]);
                if output.code == 0 {
                    return container;
                }
                last = Some(output);
            }
            self.assert_running("waiting for container CLI query");
            thread::sleep(POLL_INTERVAL);
        }
        test_fail(format_args!(
            "container CLI query did not become ready; last={last:?}; stdout={:?}; stderr={:?}",
            self.stdout(),
            self.stderr()
        ));
    }

    fn assert_running(&mut self, context: &str) {
        if let Some(status) = self.child.try_wait().must() {
            test_fail(format_args!(
                "attached up exited while {context}: {status}; stdout={:?}; stderr={:?}",
                self.stdout(),
                self.stderr()
            ));
        }
    }

    fn stdout(&self) -> String {
        fs::read_to_string(&self.stdout_path).unwrap_or_default()
    }

    fn stderr(&self) -> String {
        fs::read_to_string(&self.stderr_path).unwrap_or_default()
    }

    fn stop(&mut self) {
        if self.stopped {
            return;
        }
        drop(self.child.stdin.take());
        let deadline = Instant::now() + STOP_TIMEOUT;
        while Instant::now() < deadline {
            if self.child.try_wait().must().is_some() {
                self.stopped = true;
                return;
            }
            thread::sleep(POLL_INTERVAL);
        }
        self.child.kill().must();
        _ = self.child.wait().must();
        self.stopped = true;
    }
}

impl Drop for AttachedSession {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Debug)]
struct ContainerCommandOutput {
    code: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl ContainerCommandOutput {
    fn stdout_text(&self) -> String {
        String::from_utf8(self.stdout.clone()).must()
    }

    fn stderr_text(&self) -> String {
        String::from_utf8(self.stderr.clone()).must()
    }
}

#[test]
#[ignore = "requires Docker daemon and Docker Compose v2 plugin"]
fn compose_integration_container_cli_supported_query_and_rejection_matrix() {
    for fixture in [
        SupportedFixture::Image,
        SupportedFixture::Dockerfile,
        SupportedFixture::Compose,
    ] {
        let workspace = ContainerCliWorkspace::supported(fixture);
        let mut session = workspace.start_attached();
        let container = session.wait_for_query(&workspace, fixture.remote_uid());
        session.wait_for_started();

        assert_supported_queries(&workspace, &container, fixture);
        // Rejections are runtime-mode independent, so one fixture keeps this Docker E2E bounded.
        if matches!(fixture, SupportedFixture::Image) {
            assert_rejection_matrix(&container, fixture.remote_uid());
        }
        if matches!(fixture, SupportedFixture::Compose) {
            assert_compose_topology_and_uid_policy(&workspace, &container);
        }
    }
}

#[test]
#[ignore = "requires Docker daemon and Docker Compose v2 plugin"]
fn compose_integration_container_cli_attached_detached_disabled_and_overlap_lifecycle() {
    let workspace = ContainerCliWorkspace::supported(SupportedFixture::Image);

    let mut enabled = workspace.start_attached();
    let container = enabled.wait_for_query(&workspace, "0");
    enabled.wait_for_started();
    docker_status([
        "exec",
        &container,
        "/bin/sh",
        "-c",
        "cp /run/decune/decune /tmp/decune-stale-client && chmod 0755 /tmp/decune-stale-client",
    ])
    .must();

    let overlap = workspace.run_host(&["up", "--detach", "--no-auto-forward"]);
    assert!(!overlap.status.success());
    assert!(overlap.stdout.is_empty());
    assert!(String::from_utf8_lossy(&overlap.stderr).contains(
        "An active decune up session uses a different container CLI policy or query context"
    ));

    enabled.stop();
    assert_canonical_unavailable(&exec_container_cli(
        &container,
        Some("0"),
        CONTAINER_CLI_DIRECT,
        &["status"],
    ));

    workspace.set_container_cli_enabled(false);
    let mut disabled = workspace.start_attached();
    disabled.wait_for_started();
    assert_path_test(&container, "! -e /run/decune/decune");
    assert_path_test(&container, "! -e /usr/local/bin/decune");
    let stale = exec_container_cli(
        &container,
        Some("0"),
        "/tmp/decune-stale-client",
        &["status"],
    );
    assert_eq!(stale.code, 1);
    assert!(stale.stdout.is_empty());
    assert_eq!(
        stale.stderr_text(),
        "Error: Container CLI queries are disabled\n"
    );
    disabled.stop();

    workspace.set_container_cli_enabled(true);
    let mut reenabled = workspace.start_attached();
    let container = reenabled.wait_for_query(&workspace, "0");
    reenabled.wait_for_started();
    assert_path_test(
        &container,
        "test \"$(readlink /usr/local/bin/decune)\" = /run/decune/decune",
    );
    reenabled.stop();

    let detached = workspace.run_host(&["up", "--detach", "--no-auto-forward"]);
    assert!(
        detached.status.success(),
        "detached up failed: {}",
        String::from_utf8_lossy(&detached.stderr)
    );
    assert_canonical_unavailable(&exec_container_cli(
        &container,
        Some("0"),
        CONTAINER_CLI_DIRECT,
        &["status"],
    ));
}

#[test]
#[ignore = "requires Docker daemon and Docker Compose v2 plugin"]
fn compose_integration_container_cli_symlink_collision_and_read_only_fallbacks() {
    let cases = [
        (
            "container-cli-collision-regular",
            "test -f /usr/local/bin/decune",
        ),
        (
            "container-cli-collision-symlink",
            "test \"$(readlink /usr/local/bin/decune)\" = /bin/false",
        ),
        (
            "container-cli-collision-directory",
            "test -d /usr/local/bin/decune",
        ),
        ("container-cli-read-only", "test ! -e /usr/local/bin/decune"),
    ];

    for (fixture, preserved_destination) in cases {
        let workspace = ContainerCliWorkspace::from_fixture(fixture);
        let mut session = workspace.start_attached();
        session.wait_for_started();
        let container = workspace.primary_container_id();

        assert!(
            session.stderr().contains(
                "Any existing destination was left unchanged. Direct command: /run/decune/decune"
            ),
            "missing direct-path warning for {fixture}: {}",
            session.stderr()
        );
        assert_path_test(&container, preserved_destination);
        let direct =
            exec_container_cli(&container, Some("0"), CONTAINER_CLI_DIRECT, &["--version"]);
        assert_eq!(direct.code, 0, "fixture: {fixture}; output: {direct:?}");
        assert!(direct.stderr.is_empty());
        assert_single_trailing_newline(&direct.stdout);
    }
}

#[test]
#[ignore = "requires Docker daemon and Docker Compose v2 plugin"]
fn compose_integration_container_cli_aggregates_forwarding_and_survives_daemon_handoff() {
    let workspace = ContainerCliWorkspace::supported(SupportedFixture::Compose);
    let mut owner = workspace.start_attached();
    let container = owner.wait_for_query(&workspace, "20001");
    owner.wait_for_started();
    let first = wait_for_forwarded_port_count(&container, "20001", 2);
    assert_eq!(first.len(), 2);

    let mut peer = workspace.start_attached();
    let peer_container = peer.wait_for_query(&workspace, "20001");
    peer.wait_for_started();
    assert_eq!(peer_container, container);
    let aggregated = wait_for_forwarded_port_count(&container, "20001", 4);
    assert_eq!(aggregated.len(), 4);

    owner.stop();
    let after_handoff = wait_for_forwarded_port_count(&container, "20001", 2);
    assert_eq!(after_handoff.len(), 2);
    let status = exec_container_cli(&container, Some("20001"), CONTAINER_CLI, &["status"]);
    assert_eq!(status.code, 0, "status after daemon handoff: {status:?}");
    peer.assert_running("checking daemon handoff");
}

#[test]
#[ignore = "requires Docker daemon and Docker Compose v2 plugin"]
fn compose_integration_container_cli_security_boundary_ignores_live_and_recorded_host_paths() {
    const SECRET_MARKER: &str = "DECUNE_E2E_SECRET_MARKER";
    let workspace = ContainerCliWorkspace::supported(SupportedFixture::Image);
    let mut session = workspace.start_attached();
    let container = session.wait_for_query(&workspace, "0");
    session.wait_for_started();
    let baseline = exec_container_cli(&container, Some("0"), CONTAINER_CLI, &["status"]);
    assert_eq!(baseline.code, 0, "baseline status: {baseline:?}");
    assert!(baseline.stderr.is_empty());

    let original_state = workspace.state();
    let mut mismatch_state = original_state.clone();
    mismatch_state.as_table_mut().must().insert(
        "config_hash".to_owned(),
        toml::Value::String(format!("{SECRET_MARKER}-raw-hash")),
    );
    workspace.write_state(&mismatch_state);
    let mismatch = exec_container_cli(&container, Some("0"), CONTAINER_CLI, &["status"]);
    assert_eq!(mismatch.code, 0, "mismatch status: {mismatch:?}");
    let mismatch_stdout = mismatch.stdout_text();
    assert!(mismatch_stdout.contains("Config snapshot: runtime-mismatch"));
    assert!(mismatch_stdout.contains("Action (run on host)"));
    assert!(!mismatch_stdout.contains(SECRET_MARKER));
    assert!(!mismatch.stderr_text().contains(SECRET_MARKER));

    workspace.write_state(&original_state);
    workspace
        .workspace
        .write_fixture_file(
            ".devcontainer/devcontainer.json",
            "compose/container-cli-security/devcontainer.json",
        )
        .must();
    let mut path_state = original_state;
    let table = path_state.as_table_mut().must();
    table.insert(
        "workspace".to_owned(),
        toml::Value::String(format!("/host/{SECRET_MARKER}/workspace")),
    );
    table.insert(
        "config_file".to_owned(),
        toml::Value::String(format!("/host/{SECRET_MARKER}/devcontainer.json")),
    );
    workspace.write_state(&path_state);

    let after = exec_container_cli(&container, Some("0"), CONTAINER_CLI, &["status"]);
    assert_eq!(after.code, 0, "status after host path mutation: {after:?}");
    assert_eq!(after.stdout, baseline.stdout);
    assert_eq!(after.stderr, baseline.stderr);
    assert!(!after.stdout_text().contains(SECRET_MARKER));

    let state_path = workspace.state_path();
    fs::set_permissions(&state_path, fs::Permissions::from_mode(0o000)).must();
    let warning = exec_container_cli(&container, Some("0"), CONTAINER_CLI, &["ports", "--json"]);
    fs::set_permissions(&state_path, fs::Permissions::from_mode(0o600)).must();
    assert_eq!(warning.code, 0, "warning response: {warning:?}");
    assert_eq!(
        warning.stderr_text(),
        "Warning: Recorded workspace state is unavailable\n"
    );
    assert_single_trailing_newline(&warning.stdout);
    serde_json::from_slice::<Vec<JsonValue>>(&warning.stdout).must();
}

fn assert_supported_queries(
    workspace: &ContainerCliWorkspace,
    container: &str,
    fixture: SupportedFixture,
) {
    assert_supported_status(workspace, container, fixture);
    assert_supported_ports(container, fixture);
    assert_local_help_and_version(container, fixture);
}

fn assert_supported_status(
    workspace: &ContainerCliWorkspace,
    container: &str,
    fixture: SupportedFixture,
) {
    let status = exec_container_cli(
        container,
        Some(fixture.remote_uid()),
        CONTAINER_CLI,
        &["status"],
    );
    assert_eq!(status.code, 0, "fixture: {fixture:?}; output: {status:?}");
    assert!(status.stderr.is_empty(), "fixture: {fixture:?}");
    assert_single_trailing_newline(&status.stdout);
    let status_text = status.stdout_text();
    assert!(status_text.contains(&format!("Workspace ID: {}", workspace.workspace_id())));
    assert!(status_text.contains(&format!("Mode: {}", fixture.mode())));
    assert!(status_text.contains("Config snapshot: consistent"));
    assert!(status_text.contains("Live workspace: not checked"));

    let direct = exec_container_cli(
        container,
        Some(fixture.remote_uid()),
        CONTAINER_CLI_DIRECT,
        &["status"],
    );
    assert_eq!(direct.code, 0, "fixture: {fixture:?}; output: {direct:?}");
    assert_eq!(direct.stdout, status.stdout);
    assert_eq!(direct.stderr, status.stderr);
}

fn assert_supported_ports(container: &str, fixture: SupportedFixture) {
    let ports = exec_container_cli(
        container,
        Some(fixture.remote_uid()),
        CONTAINER_CLI,
        &["ports"],
    );
    assert_eq!(ports.code, 0, "fixture: {fixture:?}; output: {ports:?}");
    assert!(ports.stderr.is_empty(), "fixture: {fixture:?}");
    assert_single_trailing_newline(&ports.stdout);
    assert!(ports.stdout_text().starts_with("LOCAL"));

    let json = exec_container_cli(
        container,
        Some(fixture.remote_uid()),
        CONTAINER_CLI,
        &["ports", "--json"],
    );
    assert_eq!(json.code, 0, "fixture: {fixture:?}; output: {json:?}");
    assert!(json.stderr.is_empty(), "fixture: {fixture:?}");
    assert_single_trailing_newline(&json.stdout);
    let entries = serde_json::from_slice::<Vec<JsonValue>>(&json.stdout).must();
    assert!(!entries.is_empty(), "fixture: {fixture:?}");
    for entry in &entries {
        assert!(entry.get("workspace").is_none(), "entry: {entry}");
        assert!(entry.get("workspace_id").is_none(), "entry: {entry}");
        assert!(entry.get("host_port").is_some(), "entry: {entry}");
        assert!(entry.get("container_port").is_some(), "entry: {entry}");
        assert!(entry.get("type").is_some(), "entry: {entry}");
    }
}

fn assert_local_help_and_version(container: &str, fixture: SupportedFixture) {
    for (args, expected) in [
        (&["--help"][..], "Usage: decune <COMMAND>"),
        (&["help", "status"][..], "Usage: decune status"),
        (&["ports", "--help"][..], "Usage: decune ports [--json]"),
        (
            &["help", "up"][..],
            "`decune up` can only be run on the host.",
        ),
        (&["--version"][..], "decune "),
    ] {
        let output = exec_container_cli(container, Some(fixture.remote_uid()), CONTAINER_CLI, args);
        assert_eq!(output.code, 0, "fixture: {fixture:?}; output: {output:?}");
        assert!(output.stderr.is_empty(), "fixture: {fixture:?}");
        assert!(output.stdout_text().contains(expected));
        assert_single_trailing_newline(&output.stdout);
    }
}

fn assert_rejection_matrix(container: &str, user: &str) {
    let cases = [
        (
            &["status", "--json"][..],
            "not supported inside a container",
        ),
        (&["status", "."][..], "does not accept a workspace argument"),
        (&["ports", "."][..], "does not accept a workspace argument"),
        (&["ports", "--all"][..], "cannot be run inside a container"),
        (
            &["ports", "--all", "--json"][..],
            "cannot be run inside a container",
        ),
        (&["up"][..], "run it on the host"),
        (&["rebuild"][..], "run it on the host"),
        (&["down"][..], "run it on the host"),
        (&["remove"][..], "run it on the host"),
        (&["rm"][..], "run it on the host"),
        (&["clean"][..], "run it on the host"),
        (&["unknown-command"][..], "unknown decune command"),
        (&["--unknown-option"][..], "unknown option for decune"),
    ];

    for (args, expected) in cases {
        let output = exec_container_cli(container, Some(user), CONTAINER_CLI, args);
        assert_eq!(output.code, 2, "args: {args:?}; output: {output:?}");
        assert!(output.stdout.is_empty(), "args: {args:?}");
        assert!(output.stderr_text().contains(expected), "args: {args:?}");
        assert_single_trailing_newline(&output.stderr);
    }
}

fn assert_compose_topology_and_uid_policy(
    workspace: &ContainerCliWorkspace,
    primary_container: &str,
) {
    let lifecycle = docker_output([
        "exec",
        "--user",
        "20001",
        primary_container,
        "cat",
        "/tmp/decune-lifecycle-status",
    ])
    .must();
    assert!(lifecycle.contains("Live workspace: not checked"));

    let plain = workspace.service_container_id("plain");
    assert_path_test(&plain, "test ! -e /run/decune");
    assert_path_test(&plain, "test ! -e /usr/local/bin/decune");

    let worker = workspace.service_container_id("worker");
    assert_path_test(&worker, "test -x /run/decune/decune-forward-agent");
    assert_path_test(&worker, "test ! -e /run/decune/decune");
    assert_path_test(
        &worker,
        &format!("test ! -e {HOST_DAEMON_SOCKET} && test ! -e /usr/local/bin/decune"),
    );

    for user in ["0", "65534"] {
        let rejected =
            exec_container_cli(primary_container, Some(user), CONTAINER_CLI, &["status"]);
        assert_eq!(rejected.code, 1, "user: {user}; output: {rejected:?}");
        assert!(rejected.stdout.is_empty(), "user: {user}");
        let stderr = rejected.stderr_text();
        assert!(
            stderr.contains("host daemon"),
            "user: {user}; stderr: {stderr}"
        );
        for detail in ["UID", "uid", "root", "permission", "authorization"] {
            assert!(
                !stderr.contains(detail),
                "authorization detail leaked for user {user}: {stderr}"
            );
        }
    }
}

fn forwarded_ports(container: &str, user: &str) -> Vec<JsonValue> {
    let output = exec_container_cli(container, Some(user), CONTAINER_CLI, &["ports", "--json"]);
    assert_eq!(output.code, 0, "ports output: {output:?}");
    assert!(output.stderr.is_empty());
    serde_json::from_slice::<Vec<JsonValue>>(&output.stdout)
        .must()
        .into_iter()
        .filter(|entry| entry.get("type").and_then(JsonValue::as_str) == Some("forwarded"))
        .collect()
}

fn wait_for_forwarded_port_count(container: &str, user: &str, expected: usize) -> Vec<JsonValue> {
    let deadline = Instant::now() + START_TIMEOUT;
    let mut last = Vec::new();
    while Instant::now() < deadline {
        last = forwarded_ports(container, user);
        if last.len() == expected {
            return last;
        }
        thread::sleep(POLL_INTERVAL);
    }
    test_fail(format_args!(
        "forwarded port count did not reach {expected}: {last:?}"
    ));
}

fn assert_canonical_unavailable(output: &ContainerCommandOutput) {
    assert_eq!(output.code, 1, "output: {output:?}");
    assert!(output.stdout.is_empty());
    assert_eq!(
        output.stderr_text(),
        "Error: decune host daemon is unavailable; keep an active attached \"decune up\" session running on the host (detached mode is not supported)\n"
    );
}

fn assert_path_test(container: &str, condition: &str) {
    let output = Command::new("docker")
        .args(["exec", container, "/bin/sh", "-c", condition])
        .output()
        .must();
    assert!(
        output.status.success(),
        "container path assertion failed: {condition}; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn exec_container_cli(
    container: &str,
    user: Option<&str>,
    binary: &str,
    args: &[&str],
) -> ContainerCommandOutput {
    let mut command = Command::new("docker");
    command.arg("exec");
    if let Some(user) = user {
        command.args(["--user", user]);
    }
    let output = command
        .arg(container)
        .arg(binary)
        .args(args)
        .output()
        .must();
    ContainerCommandOutput {
        code: output.status.code().must_msg(format_args!(
            "container CLI terminated without an exit code: container={container}; binary={binary}; args={args:?}"
        )),
        stdout: output.stdout,
        stderr: output.stderr,
    }
}

fn assert_single_trailing_newline(output: &[u8]) {
    assert!(output.ends_with(b"\n"), "output: {output:?}");
    assert!(!output.ends_with(b"\n\n"), "output: {output:?}");
}
