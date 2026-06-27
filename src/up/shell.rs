use std::future::Future;

use anyhow::{Result, bail};

pub(crate) async fn first_successful_shell_candidate<T, F, Fut>(
    candidates: Vec<String>,
    mut start_candidate: F,
) -> Result<T>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    if candidates.is_empty() {
        bail!("No shell command candidate is available");
    }

    let mut failures = Vec::new();
    for command in candidates {
        match start_candidate(command.clone()).await {
            Ok(result) => return Ok(result),
            Err(error) => failures.push(format!("{command}: {error:#}")),
        }
    }

    bail!(
        "Failed to start any shell command candidate. Tried: {}",
        failures.join("; ")
    )
}

pub(crate) fn shell_command_candidates(
    config_shell: Option<&str>,
    remote_user_shell: Option<&str>,
) -> Vec<String> {
    if let Some(shell) = normalized_shell(config_shell) {
        return vec![shell];
    }

    let mut candidates = Vec::new();
    if let Some(shell) = normalized_shell(remote_user_shell) {
        candidates.push(shell);
    }
    candidates.push("/bin/bash".to_owned());
    candidates.push("/bin/sh".to_owned());
    candidates.dedup();
    candidates
}

fn normalized_shell(shell: Option<&str>) -> Option<String> {
    shell
        .map(str::trim)
        .filter(|shell| !shell.is_empty())
        .map(ToOwned::to_owned)
}

pub(crate) const fn clamp_exit_code(exit_code: i64) -> i32 {
    match exit_code {
        0..=255 => exit_code as i32,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::{first_successful_shell_candidate, shell_command_candidates};

    #[test]
    fn shell_candidates_use_only_explicit_config_shell() {
        assert_eq!(
            shell_command_candidates(Some(" /bin/zsh "), Some("/bin/fish")),
            vec!["/bin/zsh".to_owned()]
        );
    }

    #[test]
    fn shell_candidates_use_remote_login_shell_before_fallbacks() {
        assert_eq!(
            shell_command_candidates(None, Some("/bin/fish")),
            vec![
                "/bin/fish".to_owned(),
                "/bin/bash".to_owned(),
                "/bin/sh".to_owned()
            ]
        );
    }

    #[test]
    fn shell_candidate_fallback_tries_next_auto_candidate_after_start_failure() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let selected = runtime
            .block_on(first_successful_shell_candidate(
                vec!["/bin/bash".to_owned(), "/bin/sh".to_owned()],
                |command| async move {
                    if command == "/bin/bash" {
                        anyhow::bail!("start failed");
                    }

                    Ok::<_, anyhow::Error>(command)
                },
            ))
            .unwrap();

        assert_eq!(selected, "/bin/sh");
    }
    #[test]
    fn shell_candidates_fall_back_to_bash_then_sh() {
        assert_eq!(
            shell_command_candidates(None, None),
            vec!["/bin/bash".to_owned(), "/bin/sh".to_owned()]
        );
    }
}
