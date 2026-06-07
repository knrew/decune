use std::{collections::BTreeMap, env, path::PathBuf};

use anyhow::{Result, anyhow};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VariableContext {
    local_workspace_folder: PathBuf,
    local_workspace_folder_basename: String,
    container_workspace_folder: String,
    container_workspace_folder_basename: String,
    devcontainer_id: String,
    uid: u32,
    gid: u32,
    remote_user: String,
    remote_user_home: Option<String>,
    container_env: BTreeMap<String, String>,
}

impl VariableContext {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        local_workspace_folder: PathBuf,
        local_workspace_folder_basename: String,
        container_workspace_folder: String,
        container_workspace_folder_basename: String,
        devcontainer_id: String,
        uid: u32,
        gid: u32,
        remote_user: String,
        remote_user_home: Option<String>,
    ) -> Self {
        Self {
            local_workspace_folder,
            local_workspace_folder_basename,
            container_workspace_folder,
            container_workspace_folder_basename,
            devcontainer_id,
            uid,
            gid,
            remote_user,
            remote_user_home,
            container_env: BTreeMap::new(),
        }
    }

    pub(crate) fn with_container_env(mut self, container_env: BTreeMap<String, String>) -> Self {
        self.container_env = container_env;
        self
    }
}

pub(crate) fn expand_variables(input: &str, context: &VariableContext) -> Result<String> {
    expand_variables_with(input, context, |name| match env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => Err(anyhow!(
            "Local environment variable is not valid Unicode: {name}"
        )),
    })
}

pub(crate) fn expand_remote_env(
    remote_env: &BTreeMap<String, String>,
    context: &VariableContext,
) -> Result<BTreeMap<String, String>> {
    expand_env_map(remote_env, context)
}

pub(crate) fn expand_container_env(
    container_env: &BTreeMap<String, String>,
    context: &VariableContext,
) -> Result<BTreeMap<String, String>> {
    reject_container_env_references(container_env)?;
    expand_env_map(container_env, context)
}

fn expand_env_map(
    values: &BTreeMap<String, String>,
    context: &VariableContext,
) -> Result<BTreeMap<String, String>> {
    values
        .iter()
        .map(|(key, value)| {
            expand_variables(value, context)
                .map(|expanded| (key.clone(), expanded))
                .map_err(|error| {
                    error.context(format!("Failed to expand environment variable: {key}"))
                })
        })
        .collect()
}

fn expand_variables_with<F>(
    input: &str,
    context: &VariableContext,
    mut local_env: F,
) -> Result<String>
where
    F: FnMut(&str) -> Result<Option<String>>,
{
    let mut output = String::with_capacity(input.len());
    let mut rest = input;

    while let Some(start) = rest.find("${") {
        output.push_str(&rest[..start]);
        let variable_start = start + 2;
        let after_start = &rest[variable_start..];
        let end = after_start
            .find('}')
            .ok_or_else(|| anyhow!("Unclosed variable expression in config string: {input}"))?;
        let expression = &after_start[..end];
        output.push_str(&resolve_expression(expression, context, &mut local_env)?);
        rest = &after_start[end + 1..];
    }

    output.push_str(rest);
    Ok(output)
}

fn resolve_expression<F>(
    expression: &str,
    context: &VariableContext,
    local_env: &mut F,
) -> Result<String>
where
    F: FnMut(&str) -> Result<Option<String>>,
{
    if let Some(rest) = expression.strip_prefix("localEnv:") {
        return resolve_local_env(rest, local_env);
    }
    if let Some(rest) = expression.strip_prefix("containerEnv:") {
        return resolve_container_env(rest, context);
    }

    match expression {
        "localWorkspaceFolder" => Ok(context
            .local_workspace_folder
            .to_string_lossy()
            .into_owned()),
        "localWorkspaceFolderBasename" => Ok(context.local_workspace_folder_basename.clone()),
        "containerWorkspaceFolder" => Ok(context.container_workspace_folder.clone()),
        "containerWorkspaceFolderBasename" => {
            Ok(context.container_workspace_folder_basename.clone())
        }
        "devcontainerId" => Ok(context.devcontainer_id.clone()),
        "uid" => Ok(context.uid.to_string()),
        "gid" => Ok(context.gid.to_string()),
        "remoteUser" => Ok(context.remote_user.clone()),
        "remoteUserHome" => context.remote_user_home.clone().ok_or_else(|| {
            anyhow!(
                "${{remoteUserHome}} is unavailable because the remote user has no passwd entry"
            )
        }),
        _ => Err(anyhow!("Unknown config variable: ${{{expression}}}")),
    }
}

fn resolve_container_env(expression: &str, context: &VariableContext) -> Result<String> {
    let mut parts = expression.splitn(2, ':');
    let name = parts.next().unwrap_or_default();
    let default = parts.next();

    if name.is_empty() {
        return Err(anyhow!("containerEnv variable name must not be empty"));
    }

    match context.container_env.get(name) {
        Some(value) => Ok(value.clone()),
        None => default
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("Container environment variable is not set: {name}")),
    }
}

fn resolve_local_env<F>(expression: &str, local_env: &mut F) -> Result<String>
where
    F: FnMut(&str) -> Result<Option<String>>,
{
    let mut parts = expression.splitn(2, ':');
    let name = parts.next().unwrap_or_default();
    let default = parts.next();

    if name.is_empty() {
        return Err(anyhow!("localEnv variable name must not be empty"));
    }

    match local_env(name)? {
        Some(value) => Ok(value),
        None => default
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("Local environment variable is not set: {name}")),
    }
}

fn reject_container_env_references(values: &BTreeMap<String, String>) -> Result<()> {
    for (key, value) in values {
        for expression in variable_expressions(value)? {
            if expression.starts_with("containerEnv:") {
                return Err(anyhow!(
                    "containerEnv value must not reference containerEnv because it would create a circular environment dependency: {key}"
                ));
            }
        }
    }

    Ok(())
}

fn variable_expressions(input: &str) -> Result<Vec<&str>> {
    let mut expressions = Vec::new();
    let mut rest = input;

    while let Some(start) = rest.find("${") {
        let variable_start = start + 2;
        let after_start = &rest[variable_start..];
        let end = after_start
            .find('}')
            .ok_or_else(|| anyhow!("Unclosed variable expression in config string: {input}"))?;
        expressions.push(&after_start[..end]);
        rest = &after_start[end + 1..];
    }

    Ok(expressions)
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::PathBuf};

    use proptest::prelude::*;

    use super::*;

    fn context() -> VariableContext {
        VariableContext::new(
            PathBuf::from("/workspace/project"),
            "project".to_owned(),
            "/workspaces/project".to_owned(),
            "project".to_owned(),
            "abc123def456".to_owned(),
            1000,
            1001,
            "vscode".to_owned(),
            Some("/home/vscode".to_owned()),
        )
    }

    fn expand_with_env(input: &str, values: &[(&str, &str)]) -> Result<String> {
        let values = values
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect::<BTreeMap<_, _>>();

        expand_variables_with(input, &context(), |name| Ok(values.get(name).cloned()))
    }

    fn context_with_container_env(values: &[(&str, &str)]) -> VariableContext {
        context().with_container_env(
            values
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
        )
    }

    fn context_without_remote_user_home() -> VariableContext {
        VariableContext::new(
            PathBuf::from("/workspace/project"),
            "project".to_owned(),
            "/workspaces/project".to_owned(),
            "project".to_owned(),
            "abc123def456".to_owned(),
            1000,
            1001,
            "1001:1001".to_owned(),
            None,
        )
    }

    #[test]
    fn expands_supported_variables() {
        let expanded = expand_with_env(
            "${localWorkspaceFolder}:${localWorkspaceFolderBasename}:\
${containerWorkspaceFolder}:${containerWorkspaceFolderBasename}:\
${devcontainerId}:${uid}:${gid}:${remoteUser}:${remoteUserHome}",
            &[],
        )
        .unwrap();

        assert_eq!(
            expanded,
            "/workspace/project:project:/workspaces/project:project:abc123def456:1000:1001:vscode:/home/vscode"
        );
    }

    #[test]
    fn remote_user_home_errors_when_unavailable() {
        let context = context_without_remote_user_home();

        let remote_user = expand_variables_with("${remoteUser}", &context, |_| Ok(None)).unwrap();
        let error = expand_variables_with("${remoteUserHome}", &context, |_| Ok(None))
            .expect_err("remoteUserHome must require a resolved passwd home");

        assert_eq!(remote_user, "1001:1001");
        assert!(
            error
                .to_string()
                .contains("remote user has no passwd entry")
        );
    }

    #[test]
    fn expands_local_env_variable() {
        let expanded =
            expand_with_env("token-${localEnv:DECUNE_TOKEN}", &[("DECUNE_TOKEN", "abc")]).unwrap();

        assert_eq!(expanded, "token-abc");
    }

    #[test]
    fn local_env_default_is_used_when_missing() {
        let expanded = expand_with_env("${localEnv:DECUNE_MISSING:fallback}", &[]).unwrap();

        assert_eq!(expanded, "fallback");
    }

    #[test]
    fn local_env_default_may_contain_colons() {
        let expanded =
            expand_with_env("${localEnv:DECUNE_MISSING:fallback:with:colon}", &[]).unwrap();

        assert_eq!(expanded, "fallback:with:colon");
    }

    #[test]
    fn expands_container_env_variable() {
        let context = context_with_container_env(&[("PATH", "/usr/local/bin:/usr/bin")]);
        let expanded = expand_variables("${containerEnv:PATH}:/extra", &context).unwrap();

        assert_eq!(expanded, "/usr/local/bin:/usr/bin:/extra");
    }

    #[test]
    fn container_env_default_is_used_when_missing() {
        let context = context_with_container_env(&[]);
        let expanded =
            expand_variables("${containerEnv:MISSING:fallback:with:colon}", &context).unwrap();

        assert_eq!(expanded, "fallback:with:colon");
    }

    #[test]
    fn missing_container_env_without_default_is_rejected() {
        let context = context_with_container_env(&[]);
        let error = expand_variables("${containerEnv:MISSING}", &context).unwrap_err();

        assert_eq!(
            error.to_string(),
            "Container environment variable is not set: MISSING"
        );
    }

    #[test]
    fn container_env_value_must_not_reference_container_env() {
        let values =
            BTreeMap::from([("PATH".to_owned(), "${containerEnv:PATH}:/extra".to_owned())]);
        let error = expand_container_env(&values, &context()).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("must not reference containerEnv")
        );
    }

    #[test]
    fn empty_local_env_name_is_rejected() {
        let error = expand_with_env("${localEnv:}", &[]).unwrap_err();

        assert_eq!(
            error.to_string(),
            "localEnv variable name must not be empty"
        );
    }

    #[test]
    fn missing_local_env_without_default_is_rejected() {
        let error = expand_with_env("${localEnv:DECUNE_MISSING}", &[]).unwrap_err();

        assert_eq!(
            error.to_string(),
            "Local environment variable is not set: DECUNE_MISSING"
        );
    }

    #[test]
    fn unknown_variable_is_rejected() {
        let error = expand_with_env("${workspaceFolder}", &[]).unwrap_err();

        assert_eq!(
            error.to_string(),
            "Unknown config variable: ${workspaceFolder}"
        );
    }

    #[test]
    fn unclosed_variable_is_rejected() {
        let error = expand_with_env("${remoteUser", &[]).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Unclosed variable expression in config string")
        );
    }

    proptest! {
        #[test]
        fn strings_without_variable_opening_are_unchanged(input in "[^$]{0,128}") {
            prop_assert_eq!(expand_with_env(&input, &[]).unwrap(), input);
        }
    }
}
