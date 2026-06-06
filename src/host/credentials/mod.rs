mod git;
mod github;
mod runtime;
mod ssh;

#[cfg(test)]
pub(crate) use git::GitCredentialCommand;
pub(crate) use git::{
    GitCredentialExecutor, SystemGitCredentialExecutor, handle_git_credential_request,
    invoked_as_git_credential_helper, prepare_git_credential_runtime, run_git_credential_helper,
    setup_git_credentials,
};
#[cfg(test)]
pub(crate) use github::prepare_github_cli_runtime_with_token;
pub(crate) use github::{
    cleanup_github_cli_token_file, host_github_auth_token_available, prepare_github_cli_runtime,
    setup_github_cli_credentials,
};
pub(crate) use runtime::{
    DECUNE_RUNTIME_TARGET, GITHUB_CLI_CONFIG_TARGET, GITHUB_CLI_TOKEN_DIR_TARGET,
    GitCredentialRuntime, GithubCliRuntime, SSH_AGENT_SOCKET_TARGET, SshAgentRuntime,
    install_staged_host_gitconfig, remove_staged_host_gitconfig,
};
pub(crate) use ssh::prepare_ssh_agent_runtime;
#[cfg(test)]
pub(crate) use ssh::prepare_ssh_agent_runtime_with_socket;
