use std::{
    env, fs, io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use tempfile::TempDir;

#[test]
fn packages_artifacts_from_cargo_target_dir() {
    let harness = ScriptHarness::new().unwrap();

    let output = harness
        .command()
        .arg("linux-amd64")
        .current_dir(repo_root())
        .output()
        .unwrap();

    assert_success(&output);
    assert_eq!(
        read_artifact(&harness, "linux-amd64/git-credential-decune"),
        "built:x86_64-unknown-linux-musl:git-credential-decune\n"
    );
    assert_eq!(
        read_artifact(&harness, "linux-amd64/decune-forward-agent"),
        "built:x86_64-unknown-linux-musl:decune-forward-agent\n"
    );
    assert!(harness.out_dir().join("manifest.json").is_file());
    assert!(harness.out_dir().join("SHA256SUMS").is_file());
}

#[test]
fn runs_cargo_from_repo_root_when_invoked_elsewhere() {
    let harness = ScriptHarness::new().unwrap();
    let outside_repo = harness.temp.path().join("outside-repo");
    fs::create_dir(&outside_repo).unwrap();

    let output = harness
        .command()
        .arg("linux-amd64")
        .current_dir(&outside_repo)
        .output()
        .unwrap();

    assert_success(&output);
    assert_eq!(
        read_artifact(&harness, "linux-amd64/git-credential-decune"),
        "built:x86_64-unknown-linux-musl:git-credential-decune\n"
    );
}

struct ScriptHarness {
    temp: TempDir,
    out_dir: PathBuf,
    cargo_target_dir: PathBuf,
    fake_cargo_bin: PathBuf,
}

impl ScriptHarness {
    fn new() -> io::Result<Self> {
        let temp = TempDir::new()?;
        let fake_cargo_bin = temp.path().join("bin");
        fs::create_dir(&fake_cargo_bin)?;
        write_fake_cargo(&fake_cargo_bin)?;

        Ok(Self {
            out_dir: temp.path().join("container-tools"),
            cargo_target_dir: temp.path().join("cargo-target"),
            fake_cargo_bin,
            temp,
        })
    }

    fn command(&self) -> Command {
        let mut command = Command::new(repo_root().join("scripts/build-container-tools.sh"));
        command
            .env("PATH", path_with_front(&self.fake_cargo_bin))
            .env("DECUNE_CONTAINER_TOOLS_OUT", &self.out_dir)
            .env("CARGO_TARGET_DIR", &self.cargo_target_dir)
            .env("FAKE_CARGO_EXPECT_CWD", repo_root());
        command
    }

    fn out_dir(&self) -> &Path {
        &self.out_dir
    }
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn path_with_front(path: &Path) -> std::ffi::OsString {
    let mut paths = vec![path.to_path_buf()];
    paths.extend(env::split_paths(&env::var_os("PATH").unwrap_or_default()));
    env::join_paths(paths).unwrap()
}

fn read_artifact(harness: &ScriptHarness, path: &str) -> String {
    fs::read_to_string(harness.out_dir().join(path)).unwrap()
}

fn write_fake_cargo(bin_dir: &Path) -> io::Result<()> {
    let cargo = bin_dir.join("cargo");
    fs::write(
        &cargo,
        r#"#!/bin/sh
set -eu

case "${1:-}" in
    metadata)
        printf '{"target_directory":"%s"}\n' "$CARGO_TARGET_DIR"
        exit 0
        ;;
    build)
        ;;
    *)
        echo "unexpected cargo command: $*" >&2
        exit 64
        ;;
esac

if [ "$(pwd)" != "$FAKE_CARGO_EXPECT_CWD" ]; then
    echo "unexpected cargo cwd: $(pwd)" >&2
    exit 65
fi

target=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --target)
            shift
            target="${1:-}"
            ;;
    esac
    shift || true
done

if [ -z "$target" ]; then
    echo "missing --target" >&2
    exit 66
fi

artifact_dir="$CARGO_TARGET_DIR/$target/release"
mkdir -p "$artifact_dir"
printf 'built:%s:git-credential-decune\n' "$target" >"$artifact_dir/git-credential-decune"
printf 'built:%s:decune-forward-agent\n' "$target" >"$artifact_dir/decune-forward-agent"
"#,
    )?;
    let mut permissions = fs::metadata(&cargo)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(cargo, permissions)
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
