use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::{
    config::{
        layer::LayerRunArg,
        resolved::{ResolvedConfig, ResolvedDevcontainerSource},
        variables::{
            SensitiveEnvMap, VariableContext, expand_string_map_tracked, expand_variables,
            references_remote_user_home_variable, references_remote_user_variable,
        },
    },
    docker::build::{DockerBuildOptions, ResolvedBuildContext},
    workspace::Workspace,
};

use super::source::dockerfile_build_input;
use crate::up::{
    MountResolution, WorkspaceLocation, WorkspaceLocationValidation,
    mounts::default_workspace_folder, resolve_workspace_location, static_mount_variable_context,
};

pub(in crate::up) struct StaticPlanExpansion {
    pub(in crate::up) workspace_location: WorkspaceLocation,
    pub(in crate::up) build_context: Option<ResolvedBuildContext>,
    pub(in crate::up) build_options: DockerBuildOptions,
    pub(in crate::up) sensitive_build_args: SensitiveEnvMap,
}

pub(in crate::up) fn expand_static_plan_fields(
    workspace: &Workspace,
    devcontainer_file: &Path,
    config: &mut ResolvedConfig,
    workspace_validation: WorkspaceLocationValidation,
    mount_resolution: MountResolution,
) -> Result<StaticPlanExpansion> {
    let preliminary_variables =
        static_mount_variable_context(workspace, &default_workspace_folder(workspace), config);
    expand_static_user_fields(config, &preliminary_variables)?;
    let workspace_location = resolve_workspace_location(
        workspace,
        config,
        workspace_validation,
        mount_resolution,
        |workspace_folder| static_mount_variable_context(workspace, workspace_folder, config),
    )?;
    if should_store_static_workspace_folder(config)? {
        config.devcontainer.workspace_folder = Some(workspace_location.workspace_folder.clone());
    }
    let mount_variables =
        static_mount_variable_context(workspace, &workspace_location.workspace_folder, config);
    let sensitive_build_args = expand_static_devcontainer_fields(config, &mount_variables)?;
    let (build_context, mut build_options) =
        dockerfile_build_input(workspace.root(), devcontainer_file, config)?;
    build_options.build_arg_redactions = sensitive_build_args.redaction_values();

    Ok(StaticPlanExpansion {
        workspace_location,
        build_context,
        build_options,
        sensitive_build_args,
    })
}

fn should_store_static_workspace_folder(config: &ResolvedConfig) -> Result<bool> {
    match config.devcontainer.workspace_folder.as_deref() {
        Some(workspace_folder) => Ok(!references_remote_user_variable(workspace_folder)?),
        None => Ok(true),
    }
}

fn expand_static_user_fields(
    config: &mut ResolvedConfig,
    variables: &VariableContext,
) -> Result<()> {
    if let Some(remote_user) = &mut config.devcontainer.remote_user {
        *remote_user =
            expand_variables(remote_user, variables).context("Failed to expand remoteUser")?;
    }
    if let Some(container_user) = &mut config.devcontainer.container_user {
        *container_user = expand_variables(container_user, variables)
            .context("Failed to expand containerUser")?;
    }

    Ok(())
}

fn expand_static_devcontainer_fields(
    config: &mut ResolvedConfig,
    variables: &VariableContext,
) -> Result<SensitiveEnvMap> {
    let mut sensitive_build_args = SensitiveEnvMap::default();

    if let Some(ResolvedDevcontainerSource::Dockerfile(build)) = &mut config.devcontainer.source {
        reject_runtime_user_home_in_build_value(build.args.values(), "build.args")?;
        reject_runtime_user_home_in_build_value(build.target.iter(), "build.target")?;
        reject_runtime_user_home_in_build_value(build.cache_from.iter(), "build.cacheFrom")?;
        let static_remote_user_available = config.devcontainer.remote_user.is_some()
            || config.devcontainer.container_user.is_some();
        if !static_remote_user_available {
            reject_remote_user_in_build_value(build.args.values(), "build.args")?;
            reject_remote_user_in_build_value(build.target.iter(), "build.target")?;
            reject_remote_user_in_build_value(build.cache_from.iter(), "build.cacheFrom")?;
        }

        let expanded_args = expand_string_map_tracked(&build.args, variables)
            .context("Failed to expand build.args")?;
        build.args = expanded_args.values;
        sensitive_build_args = expanded_args.sensitive;

        if let Some(target) = &mut build.target {
            *target =
                expand_variables(target, variables).context("Failed to expand build.target")?;
        }
        for cache in &mut build.cache_from {
            *cache =
                expand_variables(cache, variables).context("Failed to expand build.cacheFrom")?;
        }
    }

    expand_runtime_independent_string_values(
        &mut config.devcontainer.cap_add,
        variables,
        "runArgs value",
    )?;
    expand_runtime_independent_string_values(
        &mut config.devcontainer.security_opt,
        variables,
        "runArgs value",
    )?;
    expand_runtime_independent_run_args(&mut config.devcontainer.run_args, variables)?;

    Ok(sensitive_build_args)
}

pub(in crate::up) fn expand_runtime_devcontainer_fields(
    config: &mut ResolvedConfig,
    variables: &VariableContext,
) -> Result<()> {
    expand_string_values(&mut config.devcontainer.cap_add, variables, "runArgs value")?;
    expand_string_values(
        &mut config.devcontainer.security_opt,
        variables,
        "runArgs value",
    )?;
    expand_run_args(&mut config.devcontainer.run_args, variables)
}

fn reject_runtime_user_home_in_build_value<'a>(
    values: impl IntoIterator<Item = &'a String>,
    field: &str,
) -> Result<()> {
    for value in values {
        if references_remote_user_home_variable(value)? {
            bail!(
                "{field} must not reference ${{remoteUserHome}} because it is resolved from the runtime container passwd database after the image is built"
            );
        }
    }

    Ok(())
}

fn reject_remote_user_in_build_value<'a>(
    values: impl IntoIterator<Item = &'a String>,
    field: &str,
) -> Result<()> {
    for value in values {
        if references_remote_user_variable(value)? {
            bail!(
                "{field} must not reference ${{remoteUser}} unless remoteUser or containerUser is configured before the Dockerfile build"
            );
        }
    }

    Ok(())
}

fn expand_runtime_independent_run_args(
    run_args: &mut [LayerRunArg],
    variables: &VariableContext,
) -> Result<()> {
    for run_arg in run_args {
        match run_arg {
            LayerRunArg::AddHost(value)
            | LayerRunArg::Dns(value)
            | LayerRunArg::DnsSearch(value)
            | LayerRunArg::Passthrough { value, .. } => {
                if !references_remote_user_variable(value)? {
                    *value = expand_variables(value, variables)
                        .context("Failed to expand runArgs value")?;
                }
            }
        }
    }

    Ok(())
}

fn expand_run_args(run_args: &mut [LayerRunArg], variables: &VariableContext) -> Result<()> {
    for run_arg in run_args {
        match run_arg {
            LayerRunArg::AddHost(value)
            | LayerRunArg::Dns(value)
            | LayerRunArg::DnsSearch(value)
            | LayerRunArg::Passthrough { value, .. } => {
                *value =
                    expand_variables(value, variables).context("Failed to expand runArgs value")?;
            }
        }
    }

    Ok(())
}

fn expand_runtime_independent_string_values(
    values: &mut [String],
    variables: &VariableContext,
    field: &str,
) -> Result<()> {
    for value in values {
        if !references_remote_user_variable(value)? {
            *value = expand_variables(value, variables)
                .with_context(|| format!("Failed to expand {field}"))?;
        }
    }

    Ok(())
}

fn expand_string_values(
    values: &mut [String],
    variables: &VariableContext,
    field: &str,
) -> Result<()> {
    for value in values {
        *value = expand_variables(value, variables)
            .with_context(|| format!("Failed to expand {field}"))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::{
        config::{ConfigLayer, layer::LayerRunArg, types::MountType},
        up::{
            plan::build_up_plan,
            test_support::{test_workspace, write_devcontainer},
        },
        workspace::Workspace,
    };

    struct EnvVarGuard {
        name: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn capture(name: &'static str) -> Self {
            Self {
                name,
                previous: std::env::var_os(name),
            }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => unsafe {
                    std::env::set_var(self.name, value);
                },
                None => unsafe {
                    std::env::remove_var(self.name);
                },
            }
        }
    }

    #[test]
    fn build_up_plan_expands_build_args_and_hashes_local_env_values() {
        let env_name = "DECUNE_TEST_PLAN_BUILD_ARG_SCOPE";
        let _guard = EnvVarGuard::capture(env_name);
        unsafe {
            std::env::remove_var(env_name);
        }

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Build Arg Variables");
        fs::create_dir_all(&root).unwrap();
        let workspace = Workspace::resolve(&root).unwrap();
        let devcontainer_dir = root.join(".devcontainer");
        fs::create_dir_all(&devcontainer_dir).unwrap();
        fs::write(
            devcontainer_dir.join("Dockerfile"),
            "FROM alpine\nARG VARIANT\n",
        )
        .unwrap();
        write_devcontainer(
            &workspace,
            &format!(
                r#"
                {{
                  "build": {{
                    "dockerfile": "Dockerfile",
                    "args": {{
                      "VARIANT": "${{localEnv:{env_name}:bookworm}}"
                    }},
                    "target": "stage-${{localWorkspaceFolderBasename}}",
                    "cacheFrom": "type=registry,ref=example.test/${{localWorkspaceFolderBasename}}:cache"
                  }}
                }}
                "#,
            ),
        );

        let defaulted = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
        assert_eq!(
            defaulted
                .build_options
                .build_args
                .get("VARIANT")
                .map(String::as_str),
            Some("bookworm")
        );
        assert!(!defaulted.sensitive_build_args.contains_key("VARIANT"));
        assert_eq!(
            defaulted.build_options.target.as_deref(),
            Some("stage-Build Arg Variables")
        );
        assert_eq!(
            defaulted.build_options.cache_from,
            vec!["type=registry,ref=example.test/Build Arg Variables:cache"]
        );

        unsafe {
            std::env::set_var(env_name, "secret-bookworm");
        }
        let from_env = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
        assert_eq!(
            from_env
                .build_options
                .build_args
                .get("VARIANT")
                .map(String::as_str),
            Some("secret-bookworm")
        );
        assert!(from_env.sensitive_build_args.contains_key("VARIANT"));
        assert!(
            from_env
                .build_options
                .build_arg_redactions
                .iter()
                .any(|value| value == "secret-bookworm")
        );
        assert_ne!(
            defaulted.resources.config_hash,
            from_env.resources.config_hash
        );

        unsafe {
            std::env::set_var(env_name, "secret-trixie");
        }
        let changed_env = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
        assert_ne!(
            from_env.resources.config_hash,
            changed_env.resources.config_hash
        );
    }

    #[test]
    fn build_up_plan_expands_workspace_folder_variables_before_validation() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Workspace Variables");
        fs::create_dir_all(&root).unwrap();
        let workspace = Workspace::resolve(&root).unwrap();

        write_devcontainer(
            &workspace,
            r#"
            {
              "image": "alpine:3.20",
              "workspaceFolder": "/workspaces/${localWorkspaceFolderBasename}"
            }
            "#,
        );
        let basename = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
        assert_eq!(basename.workspace_folder, "/workspaces/Workspace Variables");

        write_devcontainer(
            &workspace,
            r#"
            {
              "image": "alpine:3.20",
              "workspaceFolder": "${containerWorkspaceFolder}/subdir"
            }
            "#,
        );
        let container_workspace = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
        assert_eq!(
            container_workspace.workspace_folder,
            "/workspaces/Workspace Variables/subdir"
        );
    }

    #[test]
    fn build_up_plan_expands_user_fields_and_run_args_values() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("User Run Args");
        fs::create_dir_all(&root).unwrap();
        let workspace = Workspace::resolve(&root).unwrap();
        write_devcontainer(
            &workspace,
            r#"
            {
              "image": "alpine:3.20",
              "remoteUser": "${localWorkspaceFolderBasename}-remote",
              "containerUser": "${localWorkspaceFolderBasename}-container",
              "runArgs": [
                "--cap-add=SYS_${localWorkspaceFolderBasename}",
                "--security-opt", "label=${localWorkspaceFolderBasename}",
                "--add-host", "api.${localWorkspaceFolderBasename}:127.0.0.1",
                "--dns", "dns-${localWorkspaceFolderBasename}",
                "--dns-search=${localWorkspaceFolderBasename}.test",
                "--hostname", "host-${localWorkspaceFolderBasename}"
              ]
            }
            "#,
        );

        let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_eq!(
            plan.config.devcontainer.remote_user.as_deref(),
            Some("User Run Args-remote")
        );
        assert_eq!(
            plan.config.devcontainer.container_user.as_deref(),
            Some("User Run Args-container")
        );
        assert_eq!(plan.config.devcontainer.cap_add, vec!["SYS_User Run Args"]);
        assert_eq!(
            plan.config.devcontainer.security_opt,
            vec!["label=User Run Args"]
        );
        assert_eq!(
            plan.config.devcontainer.run_args,
            vec![
                LayerRunArg::AddHost("api.User Run Args:127.0.0.1".to_owned()),
                LayerRunArg::Dns("dns-User Run Args".to_owned()),
                LayerRunArg::DnsSearch("User Run Args.test".to_owned()),
                LayerRunArg::Passthrough {
                    option: "--hostname".to_owned(),
                    value: "host-User Run Args".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn build_up_plan_keeps_runtime_user_dependent_fields_for_runtime_expansion() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Runtime User Fields");
        fs::create_dir_all(&root).unwrap();
        let workspace = Workspace::resolve(&root).unwrap();
        write_devcontainer(
            &workspace,
            r#"
            {
              "image": "alpine:3.20",
              "workspaceFolder": "${remoteUserHome}/src",
              "runArgs": [
                "--cap-add=SYS_${remoteUser}",
                "--security-opt", "label=${remoteUser}",
                "--add-host", "api.${remoteUser}:127.0.0.1",
                "--dns", "${remoteUser}",
                "--dns-search=${remoteUser}.test",
                "--hostname", "host-${remoteUser}"
              ]
            }
            "#,
        );

        let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_eq!(plan.workspace_folder, "/root/src");
        assert_eq!(
            plan.config.devcontainer.workspace_folder.as_deref(),
            Some("${remoteUserHome}/src")
        );
        assert_eq!(plan.config.devcontainer.cap_add, vec!["SYS_${remoteUser}"]);
        assert_eq!(
            plan.config.devcontainer.security_opt,
            vec!["label=${remoteUser}"]
        );
        assert_eq!(
            plan.config.devcontainer.run_args,
            vec![
                LayerRunArg::AddHost("api.${remoteUser}:127.0.0.1".to_owned()),
                LayerRunArg::Dns("${remoteUser}".to_owned()),
                LayerRunArg::DnsSearch("${remoteUser}.test".to_owned()),
                LayerRunArg::Passthrough {
                    option: "--hostname".to_owned(),
                    value: "host-${remoteUser}".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn build_up_plan_rejects_remote_user_home_in_build_fields() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Build Remote User Home");
        fs::create_dir_all(&root).unwrap();
        let workspace = Workspace::resolve(&root).unwrap();
        write_devcontainer(
            &workspace,
            r#"
            {
              "build": {
                "dockerfile": "Dockerfile",
                "args": {
                  "REMOTE_HOME": "${remoteUserHome}"
                }
              }
            }
            "#,
        );

        let error = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("build.args must not reference ${remoteUserHome}")
        );
    }

    #[test]
    fn build_up_plan_rejects_remote_user_in_build_fields_when_not_static() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Build Remote User");
        fs::create_dir_all(&root).unwrap();
        let workspace = Workspace::resolve(&root).unwrap();
        write_devcontainer(
            &workspace,
            r#"
            {
              "build": {
                "dockerfile": "Dockerfile",
                "args": {
                  "REMOTE_USER": "${remoteUser}"
                }
              }
            }
            "#,
        );

        let error = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("build.args must not reference ${remoteUser}")
        );
    }

    #[test]
    fn build_up_plan_expands_remote_user_in_build_fields_from_container_user() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Build Container User");
        fs::create_dir_all(&root).unwrap();
        let workspace = Workspace::resolve(&root).unwrap();
        write_devcontainer(
            &workspace,
            r#"
            {
              "build": {
                "dockerfile": "Dockerfile",
                "args": {
                  "REMOTE_USER": "${remoteUser}"
                }
              },
              "containerUser": "node"
            }
            "#,
        );
        fs::write(
            workspace.root().join(".devcontainer").join("Dockerfile"),
            "FROM alpine:3.20\n",
        )
        .unwrap();

        let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_eq!(
            plan.build_options
                .build_args
                .get("REMOTE_USER")
                .map(String::as_str),
            Some("node")
        );
    }

    #[test]
    fn build_up_plan_uses_container_workspace_folder_basename_variable() {
        let workspace = test_workspace("container-basename-plan");
        write_devcontainer(
            &workspace,
            r#"
        {
          "image": "alpine:3.20",
          "workspaceFolder": "/src"
        }
        "#,
        );
        fs::create_dir_all(workspace.root().join(".decune")).unwrap();
        fs::write(
            workspace.root().join(".decune/config.toml"),
            r#"
version = 1

[[mounts]]
source = "project-cache"
target = "/opt/${containerWorkspaceFolderBasename}"
type = "volume"
"#,
        )
        .unwrap();

        let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_eq!(plan.workspace_folder, "/src");
        assert_eq!(
            plan.mounts[0].target,
            super::super::super::mounts::default_workspace_folder(&workspace)
        );
        assert_eq!(plan.mounts[1].target, "/opt/src");
    }

    #[cfg(unix)]
    #[test]
    fn build_up_plan_uses_current_uid_and_gid_variables() {
        let workspace = test_workspace("uid-gid-plan");
        write_devcontainer(
            &workspace,
            r#"
        {
          "image": "alpine:3.20"
        }
        "#,
        );
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        let cache = workspace.root().join(format!("{uid}-{gid}"));
        fs::create_dir_all(&cache).unwrap();
        fs::create_dir_all(workspace.root().join(".decune")).unwrap();
        fs::write(
            workspace.root().join(".decune/config.toml"),
            r#"
version = 1

[[mounts]]
source = "${uid}-${gid}"
target = "/cache"
type = "bind"
"#,
        )
        .unwrap();

        let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_eq!(
            plan.mounts[1].source.as_deref(),
            Some(cache.canonicalize().unwrap().to_str().unwrap())
        );
    }

    #[test]
    fn build_up_plan_uses_explicit_workspace_folder_for_workspace_mount_variables() {
        let workspace = test_workspace("workspace-mount-variable-plan");
        write_devcontainer(
            &workspace,
            r#"
        {
          "image": "alpine:3.20",
          "workspaceMount": "source=${localWorkspaceFolder},target=${containerWorkspaceFolder},type=bind",
          "workspaceFolder": "/workspace"
        }
        "#,
        );

        let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_eq!(plan.workspace_folder, "/workspace");
        assert_eq!(plan.mounts.len(), 1);
        assert_eq!(
            plan.mounts[0].source.as_deref(),
            Some(workspace.root().to_str().unwrap())
        );
        assert_eq!(plan.mounts[0].target, plan.workspace_folder);
        assert_eq!(plan.mounts[0].mount_type, MountType::Bind);
    }
}
