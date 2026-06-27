use std::time::Duration;

use anyhow::Result;

use crate::{
    devcontainer::lifecycle::{
        LifecycleRunContext, PreparedLifecycleRunContext, prepare_container_lifecycle,
        run_attach_lifecycle, run_container_start_lifecycle,
    },
    docker::user::resolve_remote_user,
    ui,
    up::{exec_target::resolve_up_exec_target, start::StartedUpContainer},
};

mod host_daemon;

pub(in crate::up) use host_daemon::start_host_daemon_for_up;

pub(in crate::up) fn report_up_success(started: &StartedUpContainer, elapsed: Duration) {
    let name = &started.outcome.container_name;
    let message = match started.lifecycle_path {
        crate::devcontainer::lifecycle::LifecycleRunPath::New => {
            format!("Started dev container: {name}")
        }
        crate::devcontainer::lifecycle::LifecycleRunPath::Started => {
            format!("Started existing dev container: {name}")
        }
        crate::devcontainer::lifecycle::LifecycleRunPath::Running => {
            format!("Reusing running dev container: {name}")
        }
    };

    ui::finished(&message, elapsed);
}

pub(in crate::up) async fn prepare_up_lifecycle(
    started: &StartedUpContainer,
) -> Result<PreparedLifecycleRunContext<'_>> {
    let target = resolve_up_exec_target(&started.plan, &started.outcome.container_name).await?;
    let remote_user = resolve_remote_user(
        &started.client,
        &target.id,
        &started.plan.effective_users,
        &started.plan.uid_gid_sync_plan,
    )
    .await?;

    prepare_container_lifecycle(LifecycleRunContext {
        client: &started.client,
        container: target.id,
        config: &started.plan.config,
        workspace_root: started.workspace.root(),
        workspace_basename: started.workspace.basename(),
        workspace_id: started.workspace.id(),
        workspace_folder: &started.plan.workspace_folder,
        runtime_dir: started.workspace.paths().runtime_dir(),
        remote_user,
    })
    .await
}

pub(in crate::up) async fn run_container_start_lifecycle_for_up(
    started: &StartedUpContainer,
    lifecycle: &PreparedLifecycleRunContext<'_>,
) -> Result<()> {
    let mut lifecycle_state = started.state.borrow().lifecycle;
    run_container_start_lifecycle(
        started.lifecycle_path,
        lifecycle,
        &mut lifecycle_state,
        |updated_lifecycle| {
            let mut started_state = started.state.borrow_mut();
            started_state.lifecycle = *updated_lifecycle;
            crate::state::write_state_file(started.workspace.paths().state_dir(), &started_state)
        },
    )
    .await
}

pub(in crate::up) async fn run_attach_lifecycle_for_up(
    lifecycle: &PreparedLifecycleRunContext<'_>,
) -> Result<()> {
    run_attach_lifecycle(lifecycle).await
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs};

    use crate::{
        config::ConfigLayer,
        docker::{
            client::DockerClient,
            container::remove_container,
            exec::{ExecCommandSpec, exec_capture},
            image::remove_image,
        },
        up::{
            UpOptions,
            plan::build_up_plan,
            run_detached_up,
            test_support::{test_workspace, write_devcontainer},
        },
    };

    #[test]
    fn up_detach_stops_lifecycle_after_failure() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-lifecycle-failure");
            write_devcontainer(
                &workspace,
                r#"
                {
                  "image": "alpine:3.20",
                  "onCreateCommand": "printf on-create >/tmp/decune-lifecycle; exit 7",
                  "updateContentCommand": "printf update-content >>/tmp/decune-lifecycle"
                }
                "#,
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;

                let error = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await
                .unwrap_err();
                let message = format!("{error:#}");
                assert!(message.contains("Lifecycle stage onCreateCommand failed"));
                assert!(message.contains("exit code 7"));

                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "cat /tmp/decune-lifecycle".to_owned(),
                        ],
                        user: None,
                        working_dir: None,
                        env: BTreeMap::new(),
                        redactions: Vec::new(),
                        tty: false,
                    },
                )
                .await?;

                assert_eq!(String::from_utf8(output.stdout).unwrap(), "on-create");

                Ok(())
            }
            .await;

            let cleanup = remove_container(&client, &container_name, true, true).await;
            result.and(cleanup).unwrap();
        });
    }
    #[test]
    fn up_detach_waits_for_parallel_post_start_siblings() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-parallel-lifecycle-failure");
            write_devcontainer(
                &workspace,
                r#"
                {
                  "image": "alpine:3.20",
                  "postStartCommand": {
                    "a_slow": "sleep 1; printf done >/tmp/decune-parallel-lifecycle",
                    "z_fail": "exit 7"
                  }
                }
                "#,
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;

                let error = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await
                .unwrap_err();
                let message = format!("{error:#}");
                assert!(message.contains("Lifecycle stage postStartCommand.z_fail failed"));
                assert!(message.contains("exit code 7"));

                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "cat /tmp/decune-parallel-lifecycle".to_owned(),
                        ],
                        user: None,
                        working_dir: None,
                        env: BTreeMap::new(),
                        redactions: Vec::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(String::from_utf8(output.stdout).unwrap(), "done");

                Ok(())
            }
            .await;

            let cleanup = remove_container(&client, &container_name, true, true).await;
            result.and(cleanup).unwrap();
        });
    }
    #[test]
    fn up_detach_applies_remote_env_to_lifecycle() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-lifecycle-remote-env");
            write_devcontainer(
                &workspace,
                r#"
                {
                  "image": "alpine:3.20",
                  "remoteEnv": {
                    "DECUNE_REMOTE_ENV_SENTINEL": "from-remote-env"
                  },
                  "postStartCommand": "test \"$DECUNE_REMOTE_ENV_SENTINEL\" = from-remote-env && printf '%s' \"$DECUNE_REMOTE_ENV_SENTINEL\" >/tmp/decune-remote-env"
                }
                "#,
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;

                run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;

                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "cat /tmp/decune-remote-env".to_owned(),
                        ],
                        user: None,
                        working_dir: None,
                        env: BTreeMap::new(),
            redactions: Vec::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(String::from_utf8(output.stdout).unwrap(), "from-remote-env");

                Ok(())
            }
            .await;

            let cleanup = remove_container(&client, &container_name, true, true).await;
            result.and(cleanup).unwrap();
        });
    }
    #[test]
    fn up_detach_applies_user_env_probe_to_lifecycle() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-lifecycle-user-env-probe");
            fs::create_dir_all(workspace.root().join(".devcontainer")).unwrap();
            fs::write(
                workspace.root().join(".devcontainer/Dockerfile"),
                r#"
                FROM alpine:3.20
                RUN printf '%s\n' \
                  'export DECUNE_PROBED_ENV=from-profile' \
                  'export DECUNE_ENV_PRIORITY=from-profile' \
                  >/etc/profile.d/decune-probe.sh
                "#,
            )
            .unwrap();
            write_devcontainer(
                &workspace,
                r#"
                {
                  "build": {
                    "dockerfile": "Dockerfile"
                  },
                  "userEnvProbe": "loginShell",
                  "remoteEnv": {
                    "DECUNE_ENV_PRIORITY": "from-remote-env"
                  },
                  "postStartCommand": [
                    "/bin/sh",
                    "-c",
                    "test \"$DECUNE_PROBED_ENV\" = from-profile && test \"$DECUNE_ENV_PRIORITY\" = from-remote-env && printf '%s:%s' \"$DECUNE_PROBED_ENV\" \"$DECUNE_ENV_PRIORITY\" >/tmp/decune-user-env-probe"
                  ]
                }
                "#,
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let image = plan.image.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;

                run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;

                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "cat /tmp/decune-user-env-probe".to_owned(),
                        ],
                        user: None,
                        working_dir: None,
                        env: BTreeMap::new(),
            redactions: Vec::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(
                    String::from_utf8(output.stdout).unwrap(),
                    "from-profile:from-remote-env"
                );

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }
    #[test]
    fn up_detach_omits_remote_probe_env_for_root_post_start_hook() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-root-hook-user-env-probe");
            fs::create_dir_all(workspace.root().join(".devcontainer")).unwrap();
            fs::write(
                workspace.root().join(".devcontainer/Dockerfile"),
                r#"
                FROM alpine:3.20
                RUN printf '%s\n' \
                  '#!/bin/sh' \
                  'export DECUNE_REMOTE_ONLY=from-decune' \
                  'exec /bin/sh "$@"' \
                  >/usr/local/bin/decune-probe-shell \
                  && chmod +x /usr/local/bin/decune-probe-shell \
                  && adduser -D -s /usr/local/bin/decune-probe-shell decune
                "#,
            )
            .unwrap();
            write_devcontainer(
                &workspace,
                r#"
                {
                  "build": {
                    "dockerfile": "Dockerfile"
                  },
                  "remoteUser": "decune",
                  "userEnvProbe": "loginShell",
                  "remoteEnv": {
                    "DECUNE_REMOTE_ENV": "from-remote-env"
                  }
                }
                "#,
            );
            fs::create_dir_all(workspace.root().join(".decune")).unwrap();
            fs::write(
                workspace.root().join(".decune/config.toml"),
                r#"
    version = 1

    [[hooks.before_post_start]]
    command = "test -z \"${DECUNE_REMOTE_ONLY+x}\" && test \"$DECUNE_REMOTE_ENV\" = from-remote-env && printf '%s' root-hook-clean >/tmp/decune-root-hook-env"
    user = "root"
    "#,
            )
            .unwrap();
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let image = plan.image.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;

                run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;

                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "cat /tmp/decune-root-hook-env".to_owned(),
                        ],
                        user: None,
                        working_dir: None,
                        env: BTreeMap::new(),
            redactions: Vec::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(String::from_utf8(output.stdout).unwrap(), "root-hook-clean");

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }
    #[test]
    fn up_detach_probes_env_with_remote_user_shell() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-lifecycle-user-env-probe-login-shell");
            fs::create_dir_all(workspace.root().join(".devcontainer")).unwrap();
            fs::write(
                workspace.root().join(".devcontainer/Dockerfile"),
                r#"
                FROM alpine:3.20
                RUN printf '%s\n' \
                  '#!/bin/sh' \
                  'export DECUNE_LOGIN_SHELL_ENV=from-login-shell' \
                  'exec /bin/sh "$@"' \
                  >/usr/local/bin/decune-probe-shell \
                  && chmod +x /usr/local/bin/decune-probe-shell \
                  && adduser -D -s /usr/local/bin/decune-probe-shell decune
                "#,
            )
            .unwrap();
            write_devcontainer(
                &workspace,
                r#"
                {
                  "build": {
                    "dockerfile": "Dockerfile"
                  },
                  "remoteUser": "decune",
                  "userEnvProbe": "loginShell",
                  "postStartCommand": [
                    "/bin/sh",
                    "-c",
                    "test \"$DECUNE_LOGIN_SHELL_ENV\" = from-login-shell && printf '%s' \"$DECUNE_LOGIN_SHELL_ENV\" >/tmp/decune-login-shell-env-probe"
                  ]
                }
                "#,
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let image = plan.image.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;

                run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;

                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "cat /tmp/decune-login-shell-env-probe".to_owned(),
                        ],
                        user: None,
                        working_dir: None,
                        env: BTreeMap::new(),
            redactions: Vec::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(String::from_utf8(output.stdout).unwrap(), "from-login-shell");

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }
}
