use std::{
    collections::BTreeMap,
    fs, io,
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
    process::{Command, Stdio},
};

use anyhow::{Context, Result};

use crate::{
    config::{
        resolved::{ResolvedConfig, ResolvedGithubCredentials},
        types::{GithubCredentialsMode, MountType},
    },
    docker::{
        exec::{ExecCommandSpec, exec_capture, exec_capture_output},
        mounts::DockerMountSpec,
        user::ResolvedRemoteUser,
    },
    host::{
        credentials::runtime::{
            GITHUB_CLI_CONFIG_TARGET, GITHUB_CLI_LEGACY_TOKEN_DIR_NAME,
            GITHUB_CLI_LEGACY_TOKEN_FILE_NAME, GITHUB_CLI_SECRET_DIR_NAME,
            GITHUB_CLI_TOKEN_FILE_NAME, GITHUB_CLI_TOKEN_TARGET, GithubCliRuntime,
            cleanup_github_cli_token_file_best_effort, shell_quote,
        },
        runtime::prepare_private_runtime_dir,
    },
    ui,
};

const GITHUB_CLI_FEATURE_CANONICAL_ID: &str = "ghcr.io/devcontainers/features/github-cli";

pub(crate) fn prepare_github_cli_runtime(
    config: &ResolvedConfig,
    runtime_dir: &Path,
) -> Result<GithubCliRuntime> {
    if !github_cli_credentials_enabled(&config.credentials.github) {
        remove_github_cli_token_file(runtime_dir)?;
        return Ok(GithubCliRuntime::empty());
    }

    let token = host_github_auth_token()?;
    prepare_github_cli_runtime_with_token(config, runtime_dir, token.as_deref())
}

pub(crate) fn prepare_github_cli_runtime_with_token(
    config: &ResolvedConfig,
    runtime_dir: &Path,
    token: Option<&str>,
) -> Result<GithubCliRuntime> {
    if !github_cli_credentials_enabled(&config.credentials.github) {
        remove_github_cli_token_file(runtime_dir)?;
        return Ok(GithubCliRuntime::empty());
    }

    let Some(token) = token else {
        remove_github_cli_token_file(runtime_dir)?;
        return Ok(GithubCliRuntime::empty());
    };
    let Some(token) = normalize_github_token(token) else {
        remove_github_cli_token_file(runtime_dir)?;
        return Ok(GithubCliRuntime::empty());
    };
    ui::notice(
        "GitHub credential forwarding is enabled; disable [credentials.github] for untrusted repositories.",
    );

    remove_legacy_github_cli_token_file(runtime_dir)?;
    prepare_private_runtime_dir(runtime_dir, "GitHub CLI")?;

    let secret_dir = runtime_dir.join(GITHUB_CLI_SECRET_DIR_NAME);
    fs::create_dir_all(&secret_dir).with_context(|| {
        format!(
            "Failed to create GitHub CLI secret directory: {}",
            secret_dir.display()
        )
    })?;
    fs::set_permissions(&secret_dir, fs::Permissions::from_mode(0o700)).with_context(|| {
        format!(
            "Failed to set GitHub CLI secret directory permissions: {}",
            secret_dir.display()
        )
    })?;

    let token_file = secret_dir.join(GITHUB_CLI_TOKEN_FILE_NAME);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&token_file)
        .with_context(|| {
            format!(
                "Failed to create GitHub CLI token file: {}",
                token_file.display()
            )
        })?;
    file.write_all(token.as_bytes()).with_context(|| {
        format!(
            "Failed to write GitHub CLI token file: {}",
            token_file.display()
        )
    })?;
    file.sync_all().with_context(|| {
        format!(
            "Failed to sync GitHub CLI token file: {}",
            token_file.display()
        )
    })?;
    fs::set_permissions(&token_file, fs::Permissions::from_mode(0o600)).with_context(|| {
        format!(
            "Failed to set GitHub CLI token file permissions: {}",
            token_file.display()
        )
    })?;

    Ok(GithubCliRuntime {
        mounts: vec![
            DockerMountSpec {
                source: Some(token_file.display().to_string()),
                target: GITHUB_CLI_TOKEN_TARGET.to_owned(),
                mount_type: MountType::Bind,
                read_only: true,
                consistency: None,
                bind_options: None,
                volume_options: None,
            },
            DockerMountSpec {
                source: None,
                target: GITHUB_CLI_CONFIG_TARGET.to_owned(),
                mount_type: MountType::Tmpfs,
                read_only: false,
                consistency: None,
                bind_options: None,
                volume_options: None,
            },
        ],
        container_env: BTreeMap::from([(
            "GH_CONFIG_DIR".to_owned(),
            GITHUB_CLI_CONFIG_TARGET.to_owned(),
        )]),
        token_file: Some(token_file),
    })
}

pub(crate) fn remove_github_cli_token_file(runtime_dir: &Path) -> Result<()> {
    for token_file in github_cli_token_cleanup_paths(runtime_dir) {
        match fs::remove_file(&token_file) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "Failed to remove GitHub CLI token file: {}",
                        token_file.display()
                    )
                });
            }
        }
    }
    Ok(())
}

fn remove_legacy_github_cli_token_file(runtime_dir: &Path) -> Result<()> {
    let token_file = runtime_dir
        .join(GITHUB_CLI_LEGACY_TOKEN_DIR_NAME)
        .join(GITHUB_CLI_LEGACY_TOKEN_FILE_NAME);
    match fs::remove_file(&token_file) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| {
            format!(
                "Failed to remove legacy GitHub CLI token file: {}",
                token_file.display()
            )
        }),
    }
}

pub(crate) fn cleanup_github_cli_token_file(runtime_dir: &Path) {
    for token_file in github_cli_token_cleanup_paths(runtime_dir) {
        cleanup_github_cli_token_file_best_effort(&token_file);
    }
}

fn github_cli_token_cleanup_paths(runtime_dir: &Path) -> [std::path::PathBuf; 2] {
    [
        runtime_dir
            .join(GITHUB_CLI_SECRET_DIR_NAME)
            .join(GITHUB_CLI_TOKEN_FILE_NAME),
        runtime_dir
            .join(GITHUB_CLI_LEGACY_TOKEN_DIR_NAME)
            .join(GITHUB_CLI_LEGACY_TOKEN_FILE_NAME),
    ]
}

pub(crate) async fn setup_github_cli_credentials(
    client: &crate::docker::client::DockerClient,
    container: &str,
    config: &ResolvedConfig,
    remote_user: &ResolvedRemoteUser,
) -> Result<()> {
    if !github_cli_credentials_enabled(&config.credentials.github) {
        return Ok(());
    }

    if !github_token_file_accessible(client, container).await {
        clear_github_cli_config_dir(client, container).await;
        return Ok(());
    }

    let Some(remote_home) = remote_user.home.as_deref() else {
        ui::warn(&format!(
            "GitHub CLI token forwarding is unavailable in container: {container}: remote user home is unavailable"
        ));
        return Ok(());
    };

    let Some(github_cli_path) = resolve_github_cli_path(client, container, remote_user).await
    else {
        ui::warn(&format!(
            "GitHub CLI token forwarding is unavailable in container: {container}"
        ));
        if github_cli_feature_is_configured(config) {
            ui::warn("GitHub CLI Feature is configured but gh is not available in the container");
        }
        return Ok(());
    };

    if prepare_github_cli_config_dir(client, container, remote_user)
        .await
        .is_err()
    {
        ui::warn(&format!(
            "GitHub CLI token forwarding is unavailable in container: {container}"
        ));
        return Ok(());
    }

    let login_result = exec_capture(
        client,
        container,
        &ExecCommandSpec {
            command: vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                github_cli_auth_login_script(&config.credentials.github, &github_cli_path),
            ],
            user: Some("root".to_owned()),
            working_dir: None,
            env: BTreeMap::from([(
                "DECUNE_GH_CONFIG_OWNER".to_owned(),
                format!("{}:{}", remote_user.uid, remote_user.gid),
            )]),
            redactions: Vec::new(),
            tty: false,
        },
    )
    .await
    .with_context(|| format!("Failed to login GitHub CLI in container: {container}"));
    if login_result.is_err() {
        ui::warn(&format!(
            "GitHub CLI token forwarding is unavailable in container: {container}"
        ));
        return Ok(());
    }

    let setup_git_result = exec_capture(
        client,
        container,
        &ExecCommandSpec {
            command: vec![
                "/bin/sh".to_owned(),
                "-lc".to_owned(),
                github_cli_setup_git_script(&config.credentials.github, &github_cli_path),
            ],
            user: Some(remote_user.user.clone()),
            working_dir: Some(remote_home.to_owned()),
            env: BTreeMap::from([("HOME".to_owned(), remote_home.to_owned())]),
            redactions: Vec::new(),
            tty: false,
        },
    )
    .await
    .with_context(|| {
        format!("Failed to setup GitHub CLI Git integration in container: {container}")
    });
    if setup_git_result.is_err() {
        ui::warn(&format!(
            "GitHub CLI token forwarding is unavailable in container: {container}"
        ));
    }

    Ok(())
}

async fn prepare_github_cli_config_dir(
    client: &crate::docker::client::DockerClient,
    container: &str,
    remote_user: &ResolvedRemoteUser,
) -> Result<()> {
    let script = format!(
        "set -e\nmkdir -p {config_dir}\nrm -rf {config_dir}/* {config_dir}/.[!.]* {config_dir}/..?* 2>/dev/null || true\nchown {uid}:{gid} {config_dir}\nchmod 700 {config_dir}\n",
        config_dir = shell_quote(GITHUB_CLI_CONFIG_TARGET),
        uid = remote_user.uid,
        gid = remote_user.gid,
    );

    exec_capture(
        client,
        container,
        &ExecCommandSpec {
            command: vec!["/bin/sh".to_owned(), "-c".to_owned(), script],
            user: Some("root".to_owned()),
            working_dir: None,
            env: BTreeMap::new(),
            redactions: Vec::new(),
            tty: false,
        },
    )
    .await
    .with_context(|| {
        format!("Failed to prepare GitHub CLI config directory in container: {container}")
    })?;

    Ok(())
}

async fn clear_github_cli_config_dir(
    client: &crate::docker::client::DockerClient,
    container: &str,
) {
    let config_dir = shell_quote(GITHUB_CLI_CONFIG_TARGET);
    let script = format!(
        "if [ -d {config_dir} ]; then rm -rf {config_dir}/* {config_dir}/.[!.]* {config_dir}/..?* 2>/dev/null || true; fi\n"
    );

    let _ = exec_capture(
        client,
        container,
        &ExecCommandSpec {
            command: vec!["/bin/sh".to_owned(), "-c".to_owned(), script],
            user: Some("root".to_owned()),
            working_dir: None,
            env: BTreeMap::new(),
            redactions: Vec::new(),
            tty: false,
        },
    )
    .await;
}

async fn github_token_file_accessible(
    client: &crate::docker::client::DockerClient,
    container: &str,
) -> bool {
    let output = exec_capture_output(
        client,
        container,
        &ExecCommandSpec {
            command: vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                format!("test -r {}", shell_quote(GITHUB_CLI_TOKEN_TARGET)),
            ],
            user: Some("root".to_owned()),
            working_dir: None,
            env: BTreeMap::new(),
            redactions: Vec::new(),
            tty: false,
        },
    )
    .await;

    matches!(output, Ok(output) if output.exit_code == 0)
}

async fn resolve_github_cli_path(
    client: &crate::docker::client::DockerClient,
    container: &str,
    remote_user: &ResolvedRemoteUser,
) -> Option<String> {
    let remote_home = remote_user.home.as_deref()?;

    let output = exec_capture_output(
        client,
        container,
        &ExecCommandSpec {
            command: vec![
                "/bin/sh".to_owned(),
                "-lc".to_owned(),
                "command -v gh".to_owned(),
            ],
            user: Some(remote_user.user.clone()),
            working_dir: Some(remote_home.to_owned()),
            env: BTreeMap::from([("HOME".to_owned(), remote_home.to_owned())]),
            redactions: Vec::new(),
            tty: false,
        },
    )
    .await;

    let output = match output {
        Ok(output) if output.exit_code == 0 => output,
        _ => return None,
    };
    let stdout = String::from_utf8(output.stdout).ok()?;
    let path = stdout.lines().next()?.trim();
    if path.starts_with('/') {
        Some(path.to_owned())
    } else {
        None
    }
}

fn github_cli_auth_login_script(credentials: &ResolvedGithubCredentials, gh_path: &str) -> String {
    if !github_cli_credentials_enabled(credentials) {
        return String::new();
    }

    format!(
        "set -e\ntoken_file={token_file}\nGH_CONFIG_DIR={config_dir} {gh_path} auth login --with-token < \"$token_file\"\nchown -R {config_owner} {config_dir}\nchmod 700 {config_dir}\n",
        config_dir = shell_quote(GITHUB_CLI_CONFIG_TARGET),
        token_file = shell_quote(GITHUB_CLI_TOKEN_TARGET),
        gh_path = shell_quote(gh_path),
        config_owner = "${DECUNE_GH_CONFIG_OWNER:?}",
    )
}

fn github_cli_setup_git_script(credentials: &ResolvedGithubCredentials, gh_path: &str) -> String {
    if !github_cli_credentials_enabled(credentials) {
        return String::new();
    }

    format!(
        "set -e\nGH_CONFIG_DIR={config_dir} {gh_path} auth setup-git\n",
        config_dir = shell_quote(GITHUB_CLI_CONFIG_TARGET),
        gh_path = shell_quote(gh_path),
    )
}

fn github_cli_credentials_enabled(credentials: &ResolvedGithubCredentials) -> bool {
    credentials.enabled && credentials.mode == GithubCredentialsMode::GhTokenFile
}

fn github_cli_feature_is_configured(config: &ResolvedConfig) -> bool {
    config
        .features
        .iter()
        .any(|feature| feature.canonical_id == GITHUB_CLI_FEATURE_CANONICAL_ID)
}

fn host_github_auth_token() -> Result<Option<String>> {
    host_github_auth_token_from(Path::new("gh"))
}

#[cfg(not(test))]
pub(crate) fn host_github_auth_token_available() -> Result<bool> {
    Ok(host_github_auth_token()?.is_some())
}

#[cfg(test)]
pub(crate) fn host_github_auth_token_available() -> Result<bool> {
    Ok(false)
}

fn host_github_auth_token_from(command: &Path) -> Result<Option<String>> {
    let output = match Command::new(command)
        .args(["auth", "token"])
        .stderr(Stdio::null())
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            ui::warn(&format!(
                "GitHub CLI token forwarding is unavailable: failed to run host gh auth token: {error}"
            ));
            return Ok(None);
        }
    };

    if !output.status.success() {
        return Ok(None);
    }

    let token = match String::from_utf8(output.stdout) {
        Ok(token) => token,
        Err(_) => {
            ui::warn(
                "GitHub CLI token forwarding is unavailable: host gh auth token returned non-UTF-8 output",
            );
            return Ok(None);
        }
    };
    Ok(normalize_github_token(&token))
}

fn normalize_github_token(token: &str) -> Option<String> {
    let token = token.trim_end_matches(['\r', '\n']);
    if token.is_empty() {
        None
    } else {
        Some(format!("{token}\n"))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::{fs::PermissionsExt, fs::symlink},
        path::{Path, PathBuf},
    };

    use tempfile::TempDir;

    use super::*;
    use crate::{
        config::{resolved::ResolvedConfig, types::GithubCredentialsMode},
        host::credentials::runtime::{GITHUB_CLI_CONFIG_TARGET, GITHUB_CLI_TOKEN_TARGET},
    };

    #[test]
    fn missing_host_gh_is_treated_as_absent_token() {
        let missing_gh = PathBuf::from("/definitely/missing/decune-test-gh");

        let token = host_github_auth_token_from(&missing_gh).unwrap();

        assert_eq!(token, None);
    }

    #[test]
    fn unexecutable_host_gh_is_treated_as_absent_token() {
        let temp = TempDir::new().unwrap();
        let gh = temp.path().join("gh");
        fs::write(&gh, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o644)).unwrap();

        let token = host_github_auth_token_from(&gh).unwrap();

        assert_eq!(token, None);
    }

    #[test]
    fn non_utf8_host_gh_token_output_is_treated_as_absent_token() {
        let temp = TempDir::new().unwrap();
        let gh = temp.path().join("gh");
        fs::write(&gh, "#!/bin/sh\nprintf '\\377\\376'\n").unwrap();
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();

        let token = host_github_auth_token_from(&gh).unwrap();

        assert_eq!(token, None);
    }

    #[test]
    fn github_runtime_writes_private_token_file_and_read_only_mount() {
        let temp = TempDir::new().unwrap();
        let runtime_dir = temp.path().join("runtime");

        let runtime = prepare_github_cli_runtime_with_token(
            &ResolvedConfig::default(),
            &runtime_dir,
            Some("test-secret\n"),
        )
        .unwrap();

        assert_eq!(mode(&runtime_dir), 0o700);
        assert_eq!(mode(&runtime_dir.join("secrets")), 0o700);
        assert_eq!(mode(runtime.token_file().unwrap()), 0o600);
        assert_eq!(
            fs::read_to_string(runtime.token_file().unwrap()).unwrap(),
            "test-secret\n"
        );
        assert_eq!(runtime.mounts().len(), 2);
        assert!(
            runtime
                .mounts()
                .iter()
                .any(|mount| mount.target == GITHUB_CLI_TOKEN_TARGET
                    && mount.mount_type == MountType::Bind
                    && mount.read_only)
        );
        assert!(
            runtime
                .mounts()
                .iter()
                .any(|mount| mount.target == GITHUB_CLI_CONFIG_TARGET && !mount.read_only)
        );
    }

    #[test]
    fn github_runtime_rejects_symlink_runtime_dir() {
        let temp = TempDir::new().unwrap();
        let target = temp.path().join("target");
        let runtime_dir = temp.path().join("runtime");
        fs::create_dir(&target).unwrap();
        symlink(&target, &runtime_dir).unwrap();

        let error = prepare_github_cli_runtime_with_token(
            &ResolvedConfig::default(),
            &runtime_dir,
            Some("test-secret\n"),
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("runtime directory must not be a symlink")
        );
    }

    #[test]
    fn github_runtime_scrubs_token_file_on_drop() {
        let temp = TempDir::new().unwrap();
        let runtime_dir = temp.path().join("runtime");
        let token_path;
        {
            let runtime = prepare_github_cli_runtime_with_token(
                &ResolvedConfig::default(),
                &runtime_dir,
                Some("test-secret\n"),
            )
            .unwrap();
            token_path = runtime.token_file().unwrap().to_owned();
            assert!(token_path.exists());
        }

        assert!(token_path.exists());
        assert_eq!(fs::read_to_string(&token_path).unwrap(), "");
        assert_eq!(mode(&token_path), 0o600);
    }

    #[test]
    fn github_runtime_keeps_stable_token_mount_file_for_refresh() {
        let temp = TempDir::new().unwrap();
        let runtime_dir = temp.path().join("runtime");
        let first_source;
        let first_token_path;
        {
            let runtime = prepare_github_cli_runtime_with_token(
                &ResolvedConfig::default(),
                &runtime_dir,
                Some("first-secret\n"),
            )
            .unwrap();
            first_source = runtime
                .mounts()
                .iter()
                .find(|mount| mount.target == GITHUB_CLI_TOKEN_TARGET)
                .and_then(|mount| mount.source.clone())
                .unwrap();
            first_token_path = runtime.token_file().unwrap().to_owned();
        }

        assert!(first_token_path.exists());
        assert_eq!(fs::read_to_string(&first_token_path).unwrap(), "");
        assert!(Path::new(&first_source).parent().unwrap().is_dir());

        let runtime = prepare_github_cli_runtime_with_token(
            &ResolvedConfig::default(),
            &runtime_dir,
            Some("second-secret\n"),
        )
        .unwrap();
        let second_source = runtime
            .mounts()
            .iter()
            .find(|mount| mount.target == GITHUB_CLI_TOKEN_TARGET)
            .and_then(|mount| mount.source.as_deref())
            .unwrap();

        assert_eq!(second_source, first_source);
        assert_eq!(
            fs::read_to_string(runtime.token_file().unwrap()).unwrap(),
            "second-secret\n"
        );
    }

    #[test]
    fn github_token_cleanup_removes_only_token_file_and_keeps_secret_directory() {
        let temp = TempDir::new().unwrap();
        let runtime_dir = temp.path().join("runtime");
        let secret_dir = runtime_dir.join("secrets");
        fs::create_dir_all(&secret_dir).unwrap();
        fs::write(secret_dir.join("github-token"), "test-secret\n").unwrap();
        fs::write(secret_dir.join("metadata"), "keep\n").unwrap();

        remove_github_cli_token_file(&runtime_dir).unwrap();

        assert!(secret_dir.is_dir());
        assert!(!secret_dir.join("github-token").exists());
        assert_eq!(
            fs::read_to_string(secret_dir.join("metadata")).unwrap(),
            "keep\n"
        );
    }

    #[test]
    fn github_token_cleanup_removes_legacy_token_file() {
        let temp = TempDir::new().unwrap();
        let runtime_dir = temp.path().join("runtime");
        let legacy_dir = runtime_dir.join("gh-token");
        fs::create_dir_all(&legacy_dir).unwrap();
        fs::write(legacy_dir.join("token"), "test-secret\n").unwrap();
        fs::write(legacy_dir.join("metadata"), "keep\n").unwrap();

        remove_github_cli_token_file(&runtime_dir).unwrap();

        assert!(legacy_dir.is_dir());
        assert!(!legacy_dir.join("token").exists());
        assert_eq!(
            fs::read_to_string(legacy_dir.join("metadata")).unwrap(),
            "keep\n"
        );
    }

    #[test]
    fn github_token_cleanup_ignores_missing_token_file() {
        let temp = TempDir::new().unwrap();
        let runtime_dir = temp.path().join("runtime");
        let secret_dir = runtime_dir.join("secrets");
        fs::create_dir_all(&secret_dir).unwrap();

        remove_github_cli_token_file(&runtime_dir).unwrap();

        assert!(secret_dir.is_dir());
    }

    #[test]
    fn github_runtime_removes_stale_token_file_when_disabled() {
        let temp = TempDir::new().unwrap();
        let runtime_dir = temp.path().join("runtime");
        let (token_file, marker_file) = seed_stale_github_token_file(&runtime_dir);
        let mut config = ResolvedConfig::default();
        config.credentials.github.enabled = false;

        let runtime =
            prepare_github_cli_runtime_with_token(&config, &runtime_dir, Some("test-secret\n"))
                .unwrap();

        assert!(runtime.mounts().is_empty());
        assert!(runtime.token_file().is_none());
        assert!(!token_file.exists());
        assert_eq!(fs::read_to_string(marker_file).unwrap(), "keep\n");
    }

    #[test]
    fn github_runtime_removes_stale_token_file_when_mode_is_off() {
        let temp = TempDir::new().unwrap();
        let runtime_dir = temp.path().join("runtime");
        let (token_file, marker_file) = seed_stale_github_token_file(&runtime_dir);
        let mut config = ResolvedConfig::default();
        config.credentials.github.mode = GithubCredentialsMode::Off;

        let runtime =
            prepare_github_cli_runtime_with_token(&config, &runtime_dir, Some("test-secret\n"))
                .unwrap();

        assert!(runtime.mounts().is_empty());
        assert!(runtime.token_file().is_none());
        assert!(!token_file.exists());
        assert_eq!(fs::read_to_string(marker_file).unwrap(), "keep\n");
    }

    #[test]
    fn github_runtime_removes_stale_token_file_when_token_is_unavailable() {
        let temp = TempDir::new().unwrap();
        let runtime_dir = temp.path().join("runtime");
        let (token_file, marker_file) = seed_stale_github_token_file(&runtime_dir);

        let runtime =
            prepare_github_cli_runtime_with_token(&ResolvedConfig::default(), &runtime_dir, None)
                .unwrap();

        assert!(runtime.mounts().is_empty());
        assert!(runtime.token_file().is_none());
        assert!(!token_file.exists());
        assert_eq!(fs::read_to_string(marker_file).unwrap(), "keep\n");
    }

    #[test]
    fn github_runtime_removes_stale_token_file_when_token_is_empty() {
        let temp = TempDir::new().unwrap();
        let runtime_dir = temp.path().join("runtime");
        let (token_file, marker_file) = seed_stale_github_token_file(&runtime_dir);

        let runtime = prepare_github_cli_runtime_with_token(
            &ResolvedConfig::default(),
            &runtime_dir,
            Some("\n"),
        )
        .unwrap();

        assert!(runtime.mounts().is_empty());
        assert!(runtime.token_file().is_none());
        assert!(!token_file.exists());
        assert_eq!(fs::read_to_string(marker_file).unwrap(), "keep\n");
    }

    #[test]
    fn github_runtime_omits_mount_when_disabled() {
        let temp = TempDir::new().unwrap();
        let runtime_dir = temp.path().join("runtime");
        let mut config = ResolvedConfig::default();
        config.credentials.github.enabled = false;

        let runtime =
            prepare_github_cli_runtime_with_token(&config, &runtime_dir, Some("test-secret\n"))
                .unwrap();

        assert!(runtime.mounts().is_empty());
        assert!(runtime.token_file().is_none());
    }

    #[test]
    fn github_runtime_omits_mount_when_mode_is_off() {
        let temp = TempDir::new().unwrap();
        let runtime_dir = temp.path().join("runtime");
        let mut config = ResolvedConfig::default();
        config.credentials.github.mode = GithubCredentialsMode::Off;

        let runtime =
            prepare_github_cli_runtime_with_token(&config, &runtime_dir, Some("test-secret\n"))
                .unwrap();

        assert!(runtime.mounts().is_empty());
        assert!(runtime.token_file().is_none());
    }

    #[test]
    fn github_auth_login_script_uses_secret_token_file_without_embedding_token() {
        let script = github_cli_auth_login_script(
            &ResolvedConfig::default().credentials.github,
            "/opt/github cli/bin/gh",
        );

        assert!(script.contains("GH_CONFIG_DIR='/run/decune/gh'"));
        assert!(script.contains("token_file='/run/decune/secrets/github-token'"));
        assert!(
            script.contains("'/opt/github cli/bin/gh' auth login --with-token < \"$token_file\"")
        );
        assert!(!script.contains("gh auth setup-git"));
        assert!(!script.contains(".decune-token"));
        assert!(!script.contains("test-secret"));
    }

    #[test]
    fn github_auth_login_script_does_not_copy_token_into_config_dir() {
        let script = github_cli_auth_login_script(
            &ResolvedConfig::default().credentials.github,
            "/opt/github cli/bin/gh",
        );

        assert!(!script.contains("cp "));
        assert!(!script.contains(".decune-token"));
    }

    #[test]
    fn github_setup_git_script_uses_config_dir_without_token_file() {
        let script = github_cli_setup_git_script(
            &ResolvedConfig::default().credentials.github,
            "/opt/github cli/bin/gh",
        );

        assert!(script.contains("GH_CONFIG_DIR='/run/decune/gh'"));
        assert!(script.contains("'/opt/github cli/bin/gh' auth setup-git"));
        assert!(!script.contains("github-token"));
        assert!(!script.contains("test-secret"));
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn seed_stale_github_token_file(runtime_dir: &Path) -> (PathBuf, PathBuf) {
        let token_dir = runtime_dir.join("secrets");
        fs::create_dir_all(&token_dir).unwrap();
        let token_file = token_dir.join("github-token");
        let marker_file = token_dir.join("marker");
        fs::write(&token_file, "stale-secret\n").unwrap();
        fs::write(&marker_file, "keep\n").unwrap();
        (token_file, marker_file)
    }
}
