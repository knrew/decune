use std::{
    env, fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};

use crate::support::TempWorkspace;

#[test]
fn install_script_uses_portable_mktemp_template_on_darwin() {
    let workspace = TempWorkspace::new().unwrap();
    let fake_bin = workspace.create_dir("bin").unwrap();
    let install_dir = workspace.create_dir("install").unwrap();
    let tmp_base = workspace.create_dir("tmp").unwrap();
    let mktemp_args = workspace.path().join("mktemp-args");

    write_executable(
        &fake_bin.join("uname"),
        r#"#!/bin/sh
case "$1" in
  -s) printf '%s\n' Darwin ;;
  -m) printf '%s\n' arm64 ;;
  *) exit 64 ;;
esac
"#,
    );
    write_executable(
        &fake_bin.join("mktemp"),
        r#"#!/bin/sh
if [ "$#" -ne 2 ] || [ "$1" != "-d" ]; then
  echo "mktemp requires -d and a template" >&2
  exit 64
fi
case "$2" in
  */decune.XXXXXXXXXX) ;;
  *)
    echo "unexpected mktemp template: $2" >&2
    exit 64
    ;;
esac
printf '%s\n' "$1" "$2" > "$DECUNE_TEST_MKTEMP_ARGS"
dir="${2%XXXXXXXXXX}darwin"
mkdir -p "$dir"
printf '%s\n' "$dir"
"#,
    );
    write_executable(
        &fake_bin.join("curl"),
        r#"#!/bin/sh
out=
url=
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o)
      out="$2"
      shift 2
      ;;
    -*)
      shift
      ;;
    *)
      url="$1"
      shift
      ;;
  esac
done
if [ -z "$out" ] || [ -z "$url" ]; then
  echo "curl fake requires -o and url" >&2
  exit 64
fi
case "$url" in
  */SHA256SUMS)
    printf '%s\n' "0000000000000000000000000000000000000000000000000000000000000000  decune-v1.2.3-aarch64-apple-darwin.tar.gz" > "$out"
    ;;
  *)
    printf '%s\n' archive > "$out"
    ;;
esac
"#,
    );
    write_executable(
        &fake_bin.join("sha256sum"),
        r"#!/bin/sh
exit 0
",
    );
    write_executable(
        &fake_bin.join("tar"),
        r#"#!/bin/sh
archive=
while [ "$#" -gt 0 ]; do
  case "$1" in
    -xzf)
      archive="$2"
      shift 2
      ;;
    *)
      shift
      ;;
  esac
done
if [ -z "$archive" ]; then
  echo "tar fake requires -xzf archive" >&2
  exit 64
fi
name="${archive##*/}"
root="${name%.tar.gz}"
mkdir -p "$root"
printf '%s\n' '#!/bin/sh' 'exit 0' > "$root/decune"
chmod +x "$root/decune"
"#,
    );

    let mut paths = vec![fake_bin];
    paths.extend(env::split_paths(&env::var_os("PATH").unwrap_or_default()));
    let test_path = env::join_paths(paths).unwrap();

    let output = Command::new("sh")
        .arg(workspace_file("scripts/install.sh"))
        .arg("--version")
        .arg("1.2.3")
        .arg("--dir")
        .arg(&install_dir)
        .env("PATH", test_path)
        .env("TMPDIR", &tmp_base)
        .env("DECUNE_TEST_MKTEMP_ARGS", &mktemp_args)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "install script failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(install_dir.join("decune").is_file());
    assert_eq!(
        fs::read_to_string(&mktemp_args).unwrap(),
        format!("-d\n{}/decune.XXXXXXXXXX\n", tmp_base.display())
    );
}

#[test]
fn readme_documents_writable_install_directory_for_script_install() {
    let readme = fs::read_to_string(workspace_file("README.md")).unwrap();

    assert!(readme.contains("mkdir -p \"$HOME/.local/bin\""));
    assert!(readme.contains("--dir \"$HOME/.local/bin\""));
}

fn write_executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

fn workspace_file(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(path)
}
