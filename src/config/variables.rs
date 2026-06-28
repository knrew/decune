use std::{collections::BTreeMap, env, path::PathBuf};

use anyhow::{Result, anyhow};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct SensitiveEnvMap {
    entries: BTreeMap<String, SensitiveEnvValue>,
}

impl SensitiveEnvMap {
    pub(crate) fn insert(&mut self, key: impl Into<String>, value: SensitiveEnvValue) {
        self.entries.insert(key.into(), value);
    }

    pub(crate) fn contains_key(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }

    pub(crate) fn get(&self, key: &str) -> Option<&SensitiveEnvValue> {
        self.entries.get(key)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&String, &SensitiveEnvValue)> {
        self.entries.iter()
    }

    pub(crate) fn redaction_values(&self) -> Vec<String> {
        self.entries
            .values()
            .flat_map(|value| value.redactions.iter().cloned())
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SensitiveEnvValue {
    pub(crate) value: String,
    pub(crate) redactions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExpandedEnvMap {
    pub(crate) values: BTreeMap<String, String>,
    pub(crate) sensitive: SensitiveEnvMap,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExpandedString {
    value: String,
    local_env_fragments: Vec<String>,
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VariableContextInput {
    pub(crate) local_workspace_folder: PathBuf,
    pub(crate) local_workspace_folder_basename: String,
    pub(crate) container_workspace_folder: String,
    pub(crate) container_workspace_folder_basename: String,
    pub(crate) devcontainer_id: String,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) remote_user: String,
    pub(crate) remote_user_home: Option<String>,
}

impl VariableContext {
    pub(crate) fn new(input: VariableContextInput) -> Self {
        Self {
            local_workspace_folder: input.local_workspace_folder,
            local_workspace_folder_basename: input.local_workspace_folder_basename,
            container_workspace_folder: input.container_workspace_folder,
            container_workspace_folder_basename: input.container_workspace_folder_basename,
            devcontainer_id: input.devcontainer_id,
            uid: input.uid,
            gid: input.gid,
            remote_user: input.remote_user,
            remote_user_home: input.remote_user_home,
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

pub(crate) fn expand_remote_env_tracked(
    remote_env: &BTreeMap<String, String>,
    context: &VariableContext,
) -> Result<ExpandedEnvMap> {
    expand_env_map_tracked(remote_env, context)
}

pub(crate) fn expand_string_map_tracked(
    values: &BTreeMap<String, String>,
    context: &VariableContext,
) -> Result<ExpandedEnvMap> {
    expand_env_map_tracked(values, context)
}

pub(crate) fn expand_container_env_tracked(
    container_env: &BTreeMap<String, String>,
    context: &VariableContext,
) -> Result<ExpandedEnvMap> {
    reject_container_env_references(container_env)?;
    expand_env_map_tracked(container_env, context)
}

pub(crate) fn references_remote_user_variable(input: &str) -> Result<bool> {
    references_any_variable(input, &["remoteUser", "remoteUserHome"])
}

pub(crate) fn references_remote_user_home_variable(input: &str) -> Result<bool> {
    references_any_variable(input, &["remoteUserHome"])
}

pub(crate) fn references_any_variable(input: &str, names: &[&str]) -> Result<bool> {
    Ok(variable_expressions(input)?
        .into_iter()
        .any(|expression| names.contains(&expression)))
}

fn expand_env_map_tracked(
    values: &BTreeMap<String, String>,
    context: &VariableContext,
) -> Result<ExpandedEnvMap> {
    expand_env_map_tracked_with(values, context, |name| match env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => Err(anyhow!(
            "Local environment variable is not valid Unicode: {name}"
        )),
    })
}

fn expand_env_map_tracked_with<F>(
    values: &BTreeMap<String, String>,
    context: &VariableContext,
    mut local_env: F,
) -> Result<ExpandedEnvMap>
where
    F: FnMut(&str) -> Result<Option<String>>,
{
    let mut expanded = BTreeMap::new();
    let mut sensitive = SensitiveEnvMap::default();
    for (key, value) in values {
        let value =
            expand_variables_tracked_with(value, context, &mut local_env).map_err(|error| {
                error.context(format!("Failed to expand environment variable: {key}"))
            })?;
        if !value.local_env_fragments.is_empty() {
            let mut redactions = value.local_env_fragments;
            if !redactions.iter().any(|redaction| redaction == &value.value) {
                redactions.push(value.value.clone());
            }
            sensitive.insert(
                key.clone(),
                SensitiveEnvValue {
                    value: value.value.clone(),
                    redactions,
                },
            );
        }
        expanded.insert(key.clone(), value.value);
    }

    Ok(ExpandedEnvMap {
        values: expanded,
        sensitive,
    })
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

fn expand_variables_tracked_with<F>(
    input: &str,
    context: &VariableContext,
    mut local_env: F,
) -> Result<ExpandedString>
where
    F: FnMut(&str) -> Result<Option<String>>,
{
    let mut output = String::with_capacity(input.len());
    let mut local_env_fragments = Vec::new();
    let mut rest = input;

    while let Some(start) = rest.find("${") {
        output.push_str(&rest[..start]);
        let variable_start = start + 2;
        let after_start = &rest[variable_start..];
        let end = after_start
            .find('}')
            .ok_or_else(|| anyhow!("Unclosed variable expression in config string: {input}"))?;
        let expression = &after_start[..end];
        let resolved = resolve_expression_tracked(expression, context, &mut local_env)?;
        output.push_str(&resolved.value);
        local_env_fragments.extend(resolved.local_env_fragments);
        rest = &after_start[end + 1..];
    }

    output.push_str(rest);
    Ok(ExpandedString {
        value: output,
        local_env_fragments,
    })
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

fn resolve_expression_tracked<F>(
    expression: &str,
    context: &VariableContext,
    local_env: &mut F,
) -> Result<ExpandedString>
where
    F: FnMut(&str) -> Result<Option<String>>,
{
    if let Some(rest) = expression.strip_prefix("localEnv:") {
        return resolve_local_env_tracked(rest, local_env);
    }

    Ok(ExpandedString {
        value: resolve_expression(expression, context, local_env)?,
        local_env_fragments: Vec::new(),
    })
}

fn resolve_container_env(expression: &str, context: &VariableContext) -> Result<String> {
    let mut parts = expression.splitn(2, ':');
    let name = parts.next().unwrap_or_default();
    let default = parts.next();

    if name.is_empty() {
        return Err(anyhow!("containerEnv variable name must not be empty"));
    }

    context.container_env.get(name).map_or_else(
        || {
            default
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("Container environment variable is not set: {name}"))
        },
        |value| Ok(value.clone()),
    )
}

fn resolve_local_env_tracked<F>(expression: &str, local_env: &mut F) -> Result<ExpandedString>
where
    F: FnMut(&str) -> Result<Option<String>>,
{
    let mut parts = expression.splitn(2, ':');
    let name = parts.next().unwrap_or_default();
    let default = parts.next();

    if name.is_empty() {
        return Err(anyhow!("localEnv variable name must not be empty"));
    }

    local_env(name)?.map_or_else(
        || {
            default
                .map(|value| ExpandedString {
                    value: value.to_owned(),
                    local_env_fragments: Vec::new(),
                })
                .ok_or_else(|| anyhow!("Local environment variable is not set: {name}"))
        },
        |value| {
            Ok(ExpandedString {
                value: value.clone(),
                local_env_fragments: vec![value],
            })
        },
    )
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

    local_env(name)?.map_or_else(
        || {
            default
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("Local environment variable is not set: {name}"))
        },
        Ok,
    )
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
        VariableContext::new(VariableContextInput {
            local_workspace_folder: PathBuf::from("/workspace/project"),
            local_workspace_folder_basename: "project".to_owned(),
            container_workspace_folder: "/workspaces/project".to_owned(),
            container_workspace_folder_basename: "project".to_owned(),
            devcontainer_id: "abc123def456".to_owned(),
            uid: 1000,
            gid: 1001,
            remote_user: "vscode".to_owned(),
            remote_user_home: Some("/home/vscode".to_owned()),
        })
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
        VariableContext::new(VariableContextInput {
            local_workspace_folder: PathBuf::from("/workspace/project"),
            local_workspace_folder_basename: "project".to_owned(),
            container_workspace_folder: "/workspaces/project".to_owned(),
            container_workspace_folder_basename: "project".to_owned(),
            devcontainer_id: "abc123def456".to_owned(),
            uid: 1000,
            gid: 1001,
            remote_user: "1001:1001".to_owned(),
            remote_user_home: None,
        })
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
    fn tracked_env_marks_values_derived_from_local_env() {
        let values = BTreeMap::from([
            (
                "NPM_TOKEN".to_owned(),
                "prefix-${localEnv:NPM_TOKEN}".to_owned(),
            ),
            (
                "DEFAULTED".to_owned(),
                "${localEnv:DECUNE_MISSING:fallback}".to_owned(),
            ),
        ]);
        let local_env = BTreeMap::from([("NPM_TOKEN".to_owned(), "secret-token".to_owned())]);
        let expanded = expand_env_map_tracked_with(&values, &context(), |name| {
            Ok(local_env.get(name).cloned())
        })
        .unwrap();

        assert_eq!(
            expanded.values.get("NPM_TOKEN").map(String::as_str),
            Some("prefix-secret-token")
        );
        assert_eq!(
            expanded.sensitive.get("NPM_TOKEN").unwrap().redactions,
            vec!["secret-token".to_owned(), "prefix-secret-token".to_owned()]
        );
        assert!(!expanded.sensitive.contains_key("DEFAULTED"));
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
        let error = expand_container_env_tracked(&values, &context()).unwrap_err();

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
