// Verifies successful and intentionally disabled UID/GID sync Docker scenarios.

use std::fs;

use super::test_support::{UidGidScenario, linux_host_ids, linux_host_ids_without};
use crate::up::test_support::{
    build_distinct_uid_gid_users_image, build_named_uid_numeric_gid_user_image,
    build_numeric_uid_gid_user_image, build_uid_gid_user_image,
};

#[cfg(unix)]
#[test]
fn up_detach_syncs_remote_user_uid_gid_on_linux_host() {
    let Some(host) = linux_host_ids() else {
        return;
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let scenario = UidGidScenario::with_image("docker-up-uid-gid-sync", |image| {
            format!(
                r#"
                {{
                  "image": "{image}",
                  "remoteUser": "syncuser"
                }}
                "#
            )
        });

        let result: anyhow::Result<()> = async {
            scenario.clean_before().await?;
            build_uid_gid_user_image(&scenario.client, scenario.image(), "syncuser", 2001, 2001)
                .await?;

            let outcome = scenario.run_detached().await?;
            assert!(!outcome.reused);

            let output = scenario
                .exec_sh_capture("id -u; id -g", Some("syncuser"))
                .await?;
            assert_eq!(output, format!("{}\n{}\n", host.uid, host.gid));

            Ok(())
        }
        .await;

        scenario.finish(result).await;
    });
}

#[cfg(unix)]
#[test]
fn up_detach_syncs_container_user_uid_gid_when_remote_user_is_not_set() {
    let Some(host) = linux_host_ids_without(&[2001], &[2001]) else {
        return;
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let scenario =
            UidGidScenario::with_image("docker-up-uid-gid-sync-container-user", |image| {
                format!(
                    r#"
                    {{
                      "image": "{image}",
                      "containerUser": "syncuser",
                      "postCreateCommand": "id -u >/tmp/decune-container-user-ids; id -g >>/tmp/decune-container-user-ids"
                    }}
                    "#
                )
            });

        let result: anyhow::Result<()> = async {
            scenario.clean_before().await?;
            build_uid_gid_user_image(&scenario.client, scenario.image(), "syncuser", 2001, 2001)
                .await?;

            let outcome = scenario.run_detached().await?;
            assert!(!outcome.reused);

            let inspect = scenario
                .client
                .cli()
                .inspect_container(&scenario.container_name)
                .await?;
            assert_eq!(
                inspect.config.and_then(|config| config.user),
                Some("syncuser".to_owned())
            );
            let output = scenario
                .exec_sh_capture("cat /tmp/decune-container-user-ids", Some("root"))
                .await?;
            assert_eq!(output, format!("{}\n{}\n", host.uid, host.gid));

            Ok(())
        }
        .await;

        scenario.finish(result).await;
    });
}

#[cfg(unix)]
#[test]
fn up_detach_syncs_remote_user_without_renumbering_distinct_container_user() {
    let Some(host) = linux_host_ids_without(&[2001, 2002], &[2001, 2002]) else {
        return;
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let scenario =
            UidGidScenario::with_image("docker-up-uid-gid-sync-distinct-users", |image| {
                format!(
                    r#"
                    {{
                      "image": "{image}",
                      "containerUser": "containeruser",
                      "remoteUser": "remoteuser",
                      "postCreateCommand": "id -u >/tmp/decune-remote-user-ids; id -g >>/tmp/decune-remote-user-ids"
                    }}
                    "#
                )
            });

        let result: anyhow::Result<()> = async {
            scenario.clean_before().await?;
            build_distinct_uid_gid_users_image(&scenario.client, scenario.image()).await?;

            let outcome = scenario.run_detached().await?;
            assert!(!outcome.reused);

            let remote_output = scenario
                .exec_sh_capture("cat /tmp/decune-remote-user-ids", Some("root"))
                .await?;
            assert_eq!(remote_output, format!("{}\n{}\n", host.uid, host.gid));

            let container_output = scenario
                .exec_sh_capture("id -u containeruser; id -g containeruser", Some("root"))
                .await?;
            assert_eq!(container_output, "2002\n2002\n");

            Ok(())
        }
        .await;

        scenario.finish(result).await;
    });
}

#[cfg(unix)]
#[test]
fn up_detach_does_not_sync_remote_user_when_update_remote_user_uid_is_false() {
    let Some(host) = linux_host_ids() else {
        return;
    };
    if host.uid == 2001 && host.gid == 2001 {
        return;
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let scenario = UidGidScenario::with_image("docker-up-uid-gid-sync-disabled", |image| {
            format!(
                r#"
                {{
                  "image": "{image}",
                  "remoteUser": "syncuser",
                  "updateRemoteUserUID": false,
                  "postCreateCommand": "id -u >/tmp/decune-disabled-user-ids; id -g >>/tmp/decune-disabled-user-ids"
                }}
                "#
            )
        });

        let result: anyhow::Result<()> = async {
            scenario.clean_before().await?;
            build_uid_gid_user_image(&scenario.client, scenario.image(), "syncuser", 2001, 2001)
                .await?;

            let outcome = scenario.run_detached().await?;
            assert!(!outcome.reused);

            let output = scenario
                .exec_sh_capture("cat /tmp/decune-disabled-user-ids", Some("root"))
                .await?;
            assert_eq!(output, "2001\n2001\n");

            Ok(())
        }
        .await;

        scenario.finish(result).await;
    });
}

#[cfg(unix)]
#[test]
fn up_detach_applies_uid_gid_sync_after_feature_layer() {
    let Some(host) = linux_host_ids_without(&[2001], &[2001]) else {
        return;
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let scenario = UidGidScenario::with_image_workspace(
            "docker-up-uid-gid-sync-after-feature",
            |workspace, image| {
                fs::create_dir_all(workspace.root().join(".devcontainer/features/order-tool"))
                    .unwrap();
                crate::up::test_support::write_devcontainer(
                    workspace,
                    &format!(
                        r#"
                        {{
                          "image": "{image}",
                          "features": {{
                            "./features/order-tool": {{}}
                          }},
                          "remoteUser": "syncuser",
                          "postCreateCommand": "test \"$(cat /usr/local/share/decune-feature-syncuser-uid)\" = 2001 && test \"$(id -u)\" = {host_uid} && test \"$(id -g)\" = {host_gid}"
                        }}
                        "#,
                        host_uid = host.uid,
                        host_gid = host.gid,
                    ),
                );
                fs::write(
                    workspace
                        .root()
                        .join(".devcontainer/features/order-tool/devcontainer-feature.json"),
                    r#"{"id":"order-tool","version":"1.0.0","name":"Order Tool"}"#,
                )
                .unwrap();
                fs::write(
                    workspace
                        .root()
                        .join(".devcontainer/features/order-tool/install.sh"),
                    r#"
                    set -eu
                    mkdir -p /usr/local/share
                    id -u syncuser >/usr/local/share/decune-feature-syncuser-uid
                    test "$(cat /usr/local/share/decune-feature-syncuser-uid)" = 2001
                    "#,
                )
                .unwrap();
            },
        );

        let result: anyhow::Result<()> = async {
            scenario.clean_before().await?;
            build_uid_gid_user_image(&scenario.client, scenario.image(), "syncuser", 2001, 2001)
                .await?;

            let outcome = scenario.run_detached().await?;
            assert!(!outcome.reused);

            Ok(())
        }
        .await;

        scenario.finish(result).await;
    });
}

#[cfg(unix)]
#[test]
fn up_detach_rewrites_numeric_image_user_after_uid_gid_sync() {
    let Some(host) = linux_host_ids() else {
        return;
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let scenario = UidGidScenario::with_image("docker-up-uid-gid-sync-numeric-user", |image| {
            format!(
                r#"
                {{
                  "image": "{image}",
                  "remoteUser": "syncuser"
                }}
                "#
            )
        });

        let result: anyhow::Result<()> = async {
            scenario.clean_before().await?;
            build_numeric_uid_gid_user_image(&scenario.client, scenario.image()).await?;

            let outcome = scenario.run_detached().await?;
            assert!(!outcome.reused);

            let inspect = scenario
                .client
                .cli()
                .inspect_container(&scenario.container_name)
                .await?;
            assert_eq!(
                inspect.config.and_then(|config| config.user),
                Some(format!("syncuser:{}", host.gid))
            );

            let output = scenario.exec_sh_capture("id -u; id -g", None).await?;
            assert_eq!(output, format!("{}\n{}\n", host.uid, host.gid));

            Ok(())
        }
        .await;

        scenario.finish(result).await;
    });
}

#[cfg(unix)]
#[test]
fn up_detach_rewrites_named_image_user_numeric_group_after_uid_gid_sync() {
    let Some(host) = linux_host_ids_without(&[], &[2001]) else {
        return;
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let scenario = UidGidScenario::with_image(
            "docker-up-uid-gid-sync-named-user-numeric-group",
            |image| {
                format!(
                    r#"
                    {{
                      "image": "{image}",
                      "remoteUser": "syncuser"
                    }}
                    "#
                )
            },
        );

        let result: anyhow::Result<()> = async {
            scenario.clean_before().await?;
            build_named_uid_numeric_gid_user_image(&scenario.client, scenario.image()).await?;

            let outcome = scenario.run_detached().await?;
            assert!(!outcome.reused);

            let inspect = scenario
                .client
                .cli()
                .inspect_container(&scenario.container_name)
                .await?;
            assert_eq!(
                inspect.config.and_then(|config| config.user),
                Some(format!("syncuser:{}", host.gid))
            );

            let output = scenario.exec_sh_capture("id -u; id -g", None).await?;
            assert_eq!(output, format!("{}\n{}\n", host.uid, host.gid));

            Ok(())
        }
        .await;

        scenario.finish(result).await;
    });
}

#[cfg(unix)]
#[test]
fn up_detach_rewrites_numeric_remote_user_after_uid_gid_sync() {
    let Some(host) = linux_host_ids_without(&[2001], &[2001]) else {
        return;
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let scenario =
            UidGidScenario::with_image("docker-up-uid-gid-sync-numeric-remote-user", |image| {
                format!(
                    r#"
                    {{
                      "image": "{image}",
                      "remoteUser": "2001:2001",
                      "postCreateCommand": "id -u >/tmp/decune-remote-user-ids; id -g >>/tmp/decune-remote-user-ids"
                    }}
                    "#
                )
            });

        let result: anyhow::Result<()> = async {
            scenario.clean_before().await?;
            build_numeric_uid_gid_user_image(&scenario.client, scenario.image()).await?;

            let outcome = scenario.run_detached().await?;
            assert!(!outcome.reused);

            let output = scenario
                .exec_sh_capture("cat /tmp/decune-remote-user-ids", Some("root"))
                .await?;
            assert_eq!(output, format!("{}\n{}\n", host.uid, host.gid));

            Ok(())
        }
        .await;

        scenario.finish(result).await;
    });
}
