use super::*;

pub(super) fn prepare_feature_entrypoint_sentinel_runtime(
    plan: &UpPlan,
    runtime_dir: &Path,
) -> Result<()> {
    if plan.config.devcontainer.entrypoints.is_empty() {
        return Ok(());
    }

    fs::create_dir_all(runtime_dir).with_context(|| {
        format!(
            "Failed to create Feature entrypoint runtime directory: {}",
            runtime_dir.display()
        )
    })?;
    fs::set_permissions(
        runtime_dir,
        fs::Permissions::from_mode(FEATURE_ENTRYPOINT_RUNTIME_DIR_MODE),
    )
    .with_context(|| {
        format!(
            "Failed to set Feature entrypoint runtime directory permissions: {}",
            runtime_dir.display()
        )
    })?;

    let sentinel = feature_entrypoint_sentinel_runtime_path(runtime_dir)?;
    let _file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(FEATURE_ENTRYPOINT_SENTINEL_MODE)
        .open(&sentinel)
        .with_context(|| {
            format!(
                "Failed to prepare Feature entrypoint sentinel: {}",
                sentinel.display()
            )
        })?;
    fs::set_permissions(
        &sentinel,
        fs::Permissions::from_mode(FEATURE_ENTRYPOINT_SENTINEL_MODE),
    )
    .with_context(|| {
        format!(
            "Failed to set Feature entrypoint sentinel permissions: {}",
            sentinel.display()
        )
    })?;

    let token = new_feature_entrypoint_token()?;
    let token_path = feature_entrypoint_token_runtime_path(runtime_dir)?;
    let token_mode = feature_entrypoint_token_mode(plan)?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(token_mode)
        .open(&token_path)
        .with_context(|| {
            format!(
                "Failed to prepare Feature entrypoint token: {}",
                token_path.display()
            )
        })?;
    file.write_all(token.as_bytes()).with_context(|| {
        format!(
            "Failed to write Feature entrypoint token: {}",
            token_path.display()
        )
    })?;
    fs::set_permissions(&token_path, fs::Permissions::from_mode(token_mode)).with_context(
        || {
            format!(
                "Failed to set Feature entrypoint token permissions: {}",
                token_path.display()
            )
        },
    )?;

    Ok(())
}

fn feature_entrypoint_sentinel_runtime_path(runtime_dir: &Path) -> Result<PathBuf> {
    feature_entrypoint_runtime_path(runtime_dir, FEATURE_ENTRYPOINT_SENTINEL, "sentinel")
}

fn feature_entrypoint_token_runtime_path(runtime_dir: &Path) -> Result<PathBuf> {
    feature_entrypoint_runtime_path(runtime_dir, FEATURE_ENTRYPOINT_TOKEN, "token")
}

fn feature_entrypoint_runtime_path(
    runtime_dir: &Path,
    target: &'static str,
    description: &str,
) -> Result<PathBuf> {
    let relative = Path::new(target)
        .strip_prefix(DECUNE_RUNTIME_TARGET)
        .with_context(|| {
            format!(
                "Feature entrypoint {description} must be under {DECUNE_RUNTIME_TARGET}: {target}"
            )
        })?;
    Ok(runtime_dir.join(relative))
}

fn new_feature_entrypoint_token() -> Result<String> {
    random_hex(
        FEATURE_ENTRYPOINT_TOKEN_BYTES,
        "Feature entrypoint readiness token",
    )
}

fn feature_entrypoint_token_mode(plan: &UpPlan) -> Result<u32> {
    let runtime_user = uid_gid_sync_runtime_user(
        &plan.effective_users.container_user.user,
        &plan.uid_gid_sync_plan,
    )?;
    if container_runtime_user_is_root(&runtime_user)
        || matches!(plan.uid_gid_sync_plan, UidGidSyncPlan::Sync { .. })
    {
        return Ok(FEATURE_ENTRYPOINT_TOKEN_MODE);
    }

    Ok(FEATURE_ENTRYPOINT_TOKEN_COMPAT_MODE)
}

fn container_runtime_user_is_root(user: &str) -> bool {
    let user = user.split(':').next().unwrap_or(user);
    user == "root" || user == "0"
}

fn random_hex(bytes_len: usize, context: &str) -> Result<String> {
    let mut bytes = vec![0u8; bytes_len];
    fs::File::open("/dev/urandom")
        .with_context(|| format!("Failed to open /dev/urandom for {context}"))?
        .read_exact(&mut bytes)
        .with_context(|| format!("Failed to read {context}"))?;
    Ok(hex_lower(&bytes))
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

pub(super) async fn ensure_feature_entrypoints_completed(
    client: &DockerClient,
    container_name: &str,
) -> Result<()> {
    match select(
        wait_for_container_exit_code(client, container_name).boxed(),
        wait_for_feature_entrypoint_sentinel(client, container_name).boxed(),
    )
    .await
    {
        Either::Left((exit_code, _)) => {
            return Err(container_exited_during_startup_error(
                container_name,
                Some(exit_code?),
            ));
        }
        Either::Right((ready, _)) => {
            ready?;
            ensure_container_running_now(client, container_name).await?;
        }
    }

    Ok(())
}

async fn wait_for_feature_entrypoint_sentinel(
    client: &DockerClient,
    container_name: &str,
) -> Result<()> {
    loop {
        tokio::time::sleep(FEATURE_ENTRYPOINT_SENTINEL_POLL_INTERVAL).await;
        if feature_entrypoint_sentinel_is_current(client, container_name).await? {
            return Ok(());
        }
    }
}

async fn feature_entrypoint_sentinel_is_current(
    client: &DockerClient,
    container_name: &str,
) -> Result<bool> {
    let script = feature_entrypoint_sentinel_check_script();
    let output = match exec_capture_output(
        client,
        container_name,
        &ExecCommandSpec {
            command: vec!["/bin/sh".to_owned(), "-c".to_owned(), script],
            user: None,
            working_dir: None,
            env: std::collections::BTreeMap::new(),
            redactions: Vec::new(),
            tty: false,
        },
    )
    .await
    {
        Ok(output) => output,
        Err(_) => return Ok(false),
    };

    Ok(output.exit_code == 0)
}

fn feature_entrypoint_sentinel_check_script() -> String {
    feature_entrypoint_sentinel_check_script_for(
        FEATURE_ENTRYPOINT_SENTINEL,
        FEATURE_ENTRYPOINT_TOKEN,
        "/proc/1/stat",
    )
}

fn feature_entrypoint_sentinel_check_script_for(
    sentinel: &str,
    token_file: &str,
    proc_stat: &str,
) -> String {
    format!(
        r#"sentinel={sentinel}
token_file={token_file}
proc_stat={proc_stat}
stat_line=$(cat "$proc_stat" 2>/dev/null || true)
stat_tail=${{stat_line#*) }}
set -- $stat_tail
startup_id="${{20:-}}"
token=$(cat "$token_file" 2>/dev/null || true)
expected="$startup_id:$token"
test -n "$startup_id" && test -n "$token" && test -f "$sentinel" && test "$(cat "$sentinel")" = "$expected""#,
        sentinel = shell_word(sentinel),
        token_file = shell_word(token_file),
        proc_stat = shell_word(proc_stat)
    )
}

fn shell_word(value: &str) -> String {
    if value.is_empty() {
        return "''".to_owned();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

    use super::super::test_support::generated_override_test_plan;
    use super::*;
    use crate::docker::user::{EffectiveUserResolveInput, resolve_effective_users};

    #[test]
    fn prepare_feature_entrypoint_runtime_creates_sentinel_and_token() {
        let temp = tempfile::tempdir().unwrap();
        let runtime_dir = temp.path().join("runtime");
        let mut plan = generated_override_test_plan(Vec::new());
        plan.config.devcontainer.entrypoints = vec!["touch /tmp/entrypoint".to_owned()];

        prepare_feature_entrypoint_sentinel_runtime(&plan, &runtime_dir).unwrap();

        let sentinel = feature_entrypoint_sentinel_runtime_path(&runtime_dir).unwrap();
        let token = feature_entrypoint_token_runtime_path(&runtime_dir).unwrap();
        let token_content = fs::read_to_string(&token).unwrap();

        assert!(sentinel.is_file());
        assert!(token.is_file());
        assert_eq!(token_content.len(), 64);
        assert!(
            token_content
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );
        assert_eq!(mode(&runtime_dir), 0o711);
        assert_eq!(mode(&sentinel), 0o666);
        assert_eq!(mode(&token), 0o600);
    }

    #[test]
    fn prepare_feature_entrypoint_runtime_keeps_token_readable_for_nonroot_without_uid_sync() {
        let temp = tempfile::tempdir().unwrap();
        let runtime_dir = temp.path().join("runtime");
        let mut plan = generated_override_test_plan(Vec::new());
        plan.config.devcontainer.entrypoints = vec!["touch /tmp/entrypoint".to_owned()];
        plan.effective_users = resolve_effective_users(EffectiveUserResolveInput {
            devcontainer_remote_user: None,
            devcontainer_container_user: None,
            image_metadata_remote_user: None,
            image_metadata_container_user: None,
            image_config_user: Some("app"),
        })
        .unwrap();

        prepare_feature_entrypoint_sentinel_runtime(&plan, &runtime_dir).unwrap();

        let token = feature_entrypoint_token_runtime_path(&runtime_dir).unwrap();
        assert_eq!(mode(&token), 0o644);
    }

    #[test]
    fn feature_entrypoint_sentinel_check_script_rejects_token_mismatch() {
        let temp = tempfile::tempdir().unwrap();
        let proc_stat = temp.path().join("proc-stat");
        let token = temp.path().join("token");
        let sentinel = temp.path().join("sentinel");
        let startup_id = "123456789";
        let mut stat_fields = (1..20).map(|index| format!("f{index}")).collect::<Vec<_>>();
        stat_fields.push(startup_id.to_owned());
        fs::write(&proc_stat, format!("1 (sh) {}\n", stat_fields.join(" "))).unwrap();
        fs::write(&token, "good-token").unwrap();
        fs::write(&sentinel, format!("{startup_id}:bad-token")).unwrap();

        let mismatch_status = Command::new("/bin/sh")
            .arg("-c")
            .arg(feature_entrypoint_sentinel_check_script_for(
                sentinel.to_str().unwrap(),
                token.to_str().unwrap(),
                proc_stat.to_str().unwrap(),
            ))
            .status()
            .unwrap();
        assert!(!mismatch_status.success());

        fs::write(&sentinel, format!("{startup_id}:good-token")).unwrap();
        let match_status = Command::new("/bin/sh")
            .arg("-c")
            .arg(feature_entrypoint_sentinel_check_script_for(
                sentinel.to_str().unwrap(),
                token.to_str().unwrap(),
                proc_stat.to_str().unwrap(),
            ))
            .status()
            .unwrap();
        assert!(match_status.success());
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }
}
