use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Result, anyhow};
use sha2::{Digest, Sha256};

use crate::error::ResultExt;

const FALLBACK_WORKSPACE_BASENAME: &str = "workspace";
const SAFE_WORKSPACE_SLUG_MAX_LEN: usize = 48;
const WORKSPACE_ID_HEX_LEN: usize = 12;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Workspace {
    root: PathBuf,
    basename: String,
    safe_slug: String,
    id: String,
    paths: WorkspacePaths,
}

impl Workspace {
    pub(crate) fn resolve(path: impl AsRef<Path>) -> Result<Self> {
        let path = absolute_path(path.as_ref())?;
        ensure_existing_directory(&path)?;

        let root = match git_repository_root(&path) {
            Some(root) => root,
            None => path,
        };
        let root = root
            .canonicalize()
            .with_path_context("canonicalize workspace root", &root)?;
        let basename = workspace_basename(&root);
        let safe_slug = safe_workspace_slug(&basename);
        let id = workspace_id(&root);
        let paths = WorkspacePaths::resolve(&root, &id)?;

        Ok(Self {
            root,
            basename,
            safe_slug,
            id,
            paths,
        })
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn basename(&self) -> &str {
        &self.basename
    }

    pub(crate) fn safe_slug(&self) -> &str {
        &self.safe_slug
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn paths(&self) -> &WorkspacePaths {
        &self.paths
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspacePaths {
    state_dir: PathBuf,
    runtime_dir: PathBuf,
    cache_dir: PathBuf,
    feature_archive_cache_dir: PathBuf,
    global_config_path: PathBuf,
    project_config_path: PathBuf,
}

impl WorkspacePaths {
    pub(crate) fn resolve(workspace_root: &Path, workspace_id: &str) -> Result<Self> {
        Self::from_roots(workspace_root, workspace_id, &PathRoots::from_env()?)
    }

    fn from_roots(workspace_root: &Path, workspace_id: &str, roots: &PathRoots) -> Result<Self> {
        Ok(Self {
            state_dir: roots.state_root()?.join("decune").join(workspace_id),
            runtime_dir: roots.runtime_dir(workspace_id),
            cache_dir: roots.cache_root()?.join("decune").join(workspace_id),
            feature_archive_cache_dir: roots.cache_root()?.join("decune").join("features"),
            global_config_path: roots.config_root()?.join("decune").join("config.toml"),
            project_config_path: workspace_root.join(".decune").join("config.toml"),
        })
    }

    pub(crate) fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    pub(crate) fn runtime_dir(&self) -> &Path {
        &self.runtime_dir
    }

    pub(crate) fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    pub(crate) fn feature_archive_cache_dir(&self) -> &Path {
        &self.feature_archive_cache_dir
    }

    pub(crate) fn global_config_path(&self) -> &Path {
        &self.global_config_path
    }

    pub(crate) fn project_config_path(&self) -> &Path {
        &self.project_config_path
    }
}

pub(crate) fn decune_state_root() -> Result<PathBuf> {
    Ok(PathRoots::from_env()?.state_root()?.join("decune"))
}

pub(crate) fn state_dir_for_workspace_id(workspace_id: &str) -> Result<PathBuf> {
    Ok(decune_state_root()?.join(workspace_id))
}

pub(crate) fn runtime_dir_for_workspace_id(workspace_id: &str) -> Result<PathBuf> {
    Ok(PathRoots::from_env()?.runtime_dir(workspace_id))
}

pub(crate) fn safe_workspace_slug_for_name(name: &str) -> String {
    safe_workspace_slug(name)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PathRoots {
    home: Option<PathBuf>,
    xdg_config_home: Option<PathBuf>,
    xdg_state_home: Option<PathBuf>,
    xdg_cache_home: Option<PathBuf>,
    xdg_runtime_dir: Option<PathBuf>,
    uid: u32,
}

impl PathRoots {
    fn from_env() -> Result<Self> {
        Ok(Self {
            home: env_path("HOME"),
            xdg_config_home: env_path("XDG_CONFIG_HOME"),
            xdg_state_home: env_path("XDG_STATE_HOME"),
            xdg_cache_home: env_path("XDG_CACHE_HOME"),
            xdg_runtime_dir: env_path("XDG_RUNTIME_DIR"),
            uid: current_uid(),
        })
    }

    fn config_root(&self) -> Result<PathBuf> {
        self.xdg_config_home
            .clone()
            .or_else(|| self.home.as_ref().map(|home| home.join(".config")))
            .ok_or_else(|| anyhow!("HOME is not set; cannot resolve global config path"))
    }

    fn state_root(&self) -> Result<PathBuf> {
        self.xdg_state_home
            .clone()
            .or_else(|| {
                self.home
                    .as_ref()
                    .map(|home| home.join(".local").join("state"))
            })
            .ok_or_else(|| anyhow!("HOME is not set; cannot resolve state path"))
    }

    fn cache_root(&self) -> Result<PathBuf> {
        self.xdg_cache_home
            .clone()
            .or_else(|| self.home.as_ref().map(|home| home.join(".cache")))
            .ok_or_else(|| anyhow!("HOME is not set; cannot resolve cache path"))
    }

    fn runtime_dir(&self, workspace_id: &str) -> PathBuf {
        match &self.xdg_runtime_dir {
            Some(runtime_root) => runtime_root.join("decune").join(workspace_id),
            None => PathBuf::from("/tmp")
                .join(format!("decune-{}", self.uid))
                .join(workspace_id),
        }
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(env::current_dir()
            .with_resource_context("resolve current directory", "process working directory")?
            .join(path))
    }
}

fn ensure_existing_directory(path: &Path) -> Result<()> {
    let metadata = fs::metadata(path).with_path_context("read workspace path metadata", path)?;

    if metadata.is_dir() {
        Ok(())
    } else {
        Err(anyhow!(
            "Workspace path is not a directory: {}",
            path.display()
        ))
    }
}

fn git_repository_root(path: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .arg("rev-parse")
        .arg("--show-toplevel")
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    let root = git_stdout_line(&stdout);

    if root.is_empty() {
        None
    } else {
        Some(PathBuf::from(root))
    }
}

fn git_stdout_line(stdout: &str) -> &str {
    stdout
        .strip_suffix("\r\n")
        .or_else(|| stdout.strip_suffix('\n'))
        .unwrap_or(stdout)
}

fn workspace_basename(root: &Path) -> String {
    root.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(FALLBACK_WORKSPACE_BASENAME)
        .to_owned()
}

fn safe_workspace_slug(basename: &str) -> String {
    let mut output = String::new();
    let mut previous_was_hyphen = false;

    for character in basename.chars() {
        let character = character.to_ascii_lowercase();

        if character.is_ascii_alphanumeric() {
            output.push(character);
            previous_was_hyphen = false;
        } else {
            push_collapsed_hyphen(&mut output, &mut previous_was_hyphen);
        }
    }

    trim_safe_slug_separators(&mut output);
    truncate_safe_workspace_slug(&mut output);

    if output.is_empty() {
        FALLBACK_WORKSPACE_BASENAME.to_owned()
    } else {
        output
    }
}

fn push_collapsed_hyphen(output: &mut String, previous_was_hyphen: &mut bool) {
    if !output.is_empty() && !*previous_was_hyphen {
        output.push('-');
        *previous_was_hyphen = true;
    }
}

fn truncate_safe_workspace_slug(output: &mut String) {
    output.truncate(SAFE_WORKSPACE_SLUG_MAX_LEN);
    trim_safe_slug_separators(output);
}

fn trim_safe_slug_separators(output: &mut String) {
    while output.ends_with('-') {
        output.pop();
    }

    let trim_start = output.bytes().take_while(|byte| *byte == b'-').count();
    if trim_start > 0 {
        output.drain(..trim_start);
    }
}

fn workspace_id(root: &Path) -> String {
    let digest = Sha256::digest(root.to_string_lossy().as_bytes());
    let mut id = String::with_capacity(WORKSPACE_ID_HEX_LEN);

    for byte in digest.iter().take(WORKSPACE_ID_HEX_LEN / 2) {
        push_hex_byte(&mut id, *byte);
    }

    id
}

fn push_hex_byte(output: &mut String, byte: u8) {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    output.push(HEX[(byte >> 4) as usize] as char);
    output.push(HEX[(byte & 0x0f) as usize] as char);
}

fn env_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(unix)]
fn current_uid() -> u32 {
    // fallback runtime path の一部として使うだけなので，失敗しない libc 呼び出しに閉じる．
    unsafe { libc::getuid() }
}

#[cfg(not(unix))]
fn current_uid() -> u32 {
    0
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        process::Command,
    };

    #[cfg(unix)]
    use std::os::unix::fs as unix_fs;

    use super::{
        PathRoots, Workspace, WorkspacePaths, git_stdout_line, safe_workspace_slug, workspace_id,
    };

    fn fixture_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir()
            .join("decune-workspace-tests")
            .join(format!("{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn resolves_existing_directory_as_canonical_workspace_root() {
        let root = fixture_root("plain");

        let workspace = Workspace::resolve(&root).unwrap();

        assert_eq!(workspace.root(), root.canonicalize().unwrap());
        assert_eq!(
            workspace.basename(),
            root.file_name().unwrap().to_str().unwrap()
        );
        assert_eq!(workspace.id().len(), 12);
        assert_eq!(
            workspace.paths().project_config_path(),
            workspace.root().join(".decune/config.toml")
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_workspace_uses_same_canonical_id() {
        let root = fixture_root("symlink-target");
        let link = root.with_file_name("symlink-link");
        let _ = fs::remove_file(&link);
        unix_fs::symlink(&root, &link).unwrap();

        let target_workspace = Workspace::resolve(&root).unwrap();
        let link_workspace = Workspace::resolve(&link).unwrap();

        assert_eq!(target_workspace.root(), link_workspace.root());
        assert_eq!(target_workspace.id(), link_workspace.id());
    }

    #[test]
    fn git_repository_subdirectory_resolves_to_repository_root() {
        if Command::new("git").arg("--version").output().is_err() {
            eprintln!("skipped: git is not available");
            return;
        }

        let root = fixture_root("git-root");
        let child = root.join("nested/project");
        fs::create_dir_all(&child).unwrap();

        let init = Command::new("git")
            .arg("-C")
            .arg(&root)
            .arg("init")
            .output()
            .unwrap();
        assert!(
            init.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&init.stderr)
        );

        let workspace = Workspace::resolve(&child).unwrap();

        assert_eq!(workspace.root(), root.canonicalize().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn git_repository_root_preserves_trailing_space_in_path() {
        if Command::new("git").arg("--version").output().is_err() {
            eprintln!("skipped: git is not available");
            return;
        }

        let parent = fixture_root("git-root-with-trailing-space-parent");
        let root = parent.join("repo-with-trailing-space ");
        let child = root.join("nested/project");
        fs::create_dir_all(&child).unwrap();

        let init = Command::new("git")
            .arg("-C")
            .arg(&root)
            .arg("init")
            .output()
            .unwrap();
        assert!(
            init.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&init.stderr)
        );

        let workspace = Workspace::resolve(&child).unwrap();

        assert_eq!(workspace.root(), root.canonicalize().unwrap());
    }

    #[test]
    fn missing_workspace_path_returns_contextual_error() {
        let missing = fixture_root("missing-parent").join("missing");

        let error = Workspace::resolve(&missing).unwrap_err();
        let message = format!("{error:#}");

        assert!(message.contains("Failed to read workspace path metadata"));
        assert!(message.contains(&missing.display().to_string()));
    }

    #[test]
    fn file_workspace_path_is_rejected() {
        let root = fixture_root("file-path");
        let file = root.join("not-a-directory");
        fs::write(&file, b"contents").unwrap();

        let error = Workspace::resolve(&file).unwrap_err();

        assert_eq!(
            error.to_string(),
            format!("Workspace path is not a directory: {}", file.display())
        );
    }

    #[test]
    fn xdg_paths_prefer_environment_roots() {
        let roots = PathRoots {
            home: Some(PathBuf::from("/home/user")),
            xdg_config_home: Some(PathBuf::from("/xdg/config")),
            xdg_state_home: Some(PathBuf::from("/xdg/state")),
            xdg_cache_home: Some(PathBuf::from("/xdg/cache")),
            xdg_runtime_dir: Some(PathBuf::from("/run/user/1000")),
            uid: 1000,
        };

        let paths =
            WorkspacePaths::from_roots(Path::new("/workspace/project"), "abc123def456", &roots)
                .unwrap();

        assert_eq!(
            paths.state_dir(),
            Path::new("/xdg/state/decune/abc123def456")
        );
        assert_eq!(
            paths.runtime_dir(),
            Path::new("/run/user/1000/decune/abc123def456")
        );
        assert_eq!(
            paths.cache_dir(),
            Path::new("/xdg/cache/decune/abc123def456")
        );
        assert_eq!(
            paths.feature_archive_cache_dir(),
            Path::new("/xdg/cache/decune/features")
        );
        assert_eq!(
            paths.global_config_path(),
            Path::new("/xdg/config/decune/config.toml")
        );
        assert_eq!(
            paths.project_config_path(),
            Path::new("/workspace/project/.decune/config.toml")
        );
    }

    #[test]
    fn xdg_paths_fall_back_to_home_and_tmp_runtime() {
        let roots = PathRoots {
            home: Some(PathBuf::from("/home/user")),
            xdg_config_home: None,
            xdg_state_home: None,
            xdg_cache_home: None,
            xdg_runtime_dir: None,
            uid: 1000,
        };

        let paths =
            WorkspacePaths::from_roots(Path::new("/workspace"), "abc123def456", &roots).unwrap();

        assert_eq!(
            paths.state_dir(),
            Path::new("/home/user/.local/state/decune/abc123def456")
        );
        assert_eq!(
            paths.runtime_dir(),
            Path::new("/tmp/decune-1000/abc123def456")
        );
        assert_eq!(
            paths.cache_dir(),
            Path::new("/home/user/.cache/decune/abc123def456")
        );
        assert_eq!(
            paths.feature_archive_cache_dir(),
            Path::new("/home/user/.cache/decune/features")
        );
        assert_eq!(
            paths.global_config_path(),
            Path::new("/home/user/.config/decune/config.toml")
        );
    }

    #[test]
    fn config_state_and_cache_roots_require_home_when_xdg_roots_are_missing() {
        let roots = PathRoots {
            home: None,
            xdg_config_home: None,
            xdg_state_home: None,
            xdg_cache_home: None,
            xdg_runtime_dir: None,
            uid: 1000,
        };

        assert!(
            WorkspacePaths::from_roots(Path::new("/workspace"), "abc123def456", &roots).is_err()
        );
        assert_eq!(
            roots.runtime_dir("abc123def456"),
            Path::new("/tmp/decune-1000/abc123def456")
        );
    }

    #[test]
    fn git_stdout_line_removes_only_line_ending() {
        assert_eq!(
            git_stdout_line("/tmp/repo-with-trailing-space \n"),
            "/tmp/repo-with-trailing-space "
        );
        assert_eq!(git_stdout_line("/tmp/repo\r\n"), "/tmp/repo");
        assert_eq!(git_stdout_line("\n"), "");
    }

    #[test]
    fn workspace_id_uses_sha256_hex_prefix() {
        assert_eq!(
            workspace_id(Path::new("/workspace/project")),
            "e3af8a725158"
        );
    }

    #[test]
    fn safe_workspace_slug_normalizes_workspace_basename_for_docker_resources() {
        assert_eq!(safe_workspace_slug("Project Name"), "project-name");
        assert_eq!(safe_workspace_slug("日本語"), "workspace");
        assert_eq!(safe_workspace_slug("!!!"), "workspace");
        assert_eq!(safe_workspace_slug("APP__Name...v2"), "app-name-v2");
        assert_eq!(safe_workspace_slug(" - Project__Name.. "), "project-name");
    }

    #[test]
    fn safe_workspace_slug_collapses_hyphens_and_truncates_to_48_chars() {
        assert_eq!(safe_workspace_slug("a !@# b"), "a-b");
        assert_eq!(safe_workspace_slug(&"A".repeat(80)), "a".repeat(48));
        assert_eq!(
            safe_workspace_slug(&format!("{}!!!", "A".repeat(48))),
            "a".repeat(48)
        );
    }
}
