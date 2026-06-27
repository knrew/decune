// Verifies UID/GID sync Docker-backed failure paths and cleanup behavior.

use super::test_support::{UidGidScenario, linux_host_ids, linux_host_ids_without};
use crate::{
    docker::image::{PullPolicy, ensure_image},
    up::{
        start::list_workspace_containers,
        test_support::{
            build_duplicate_matching_host_ids_image, build_duplicate_old_gid_image,
            build_missing_target_group_conflict_image, build_uid_gid_conflict_user_image,
        },
    },
};

#[test]
fn up_detach_reports_missing_explicit_uid_gid_sync_target_user() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let scenario = UidGidScenario::with_devcontainer(
            "docker-up-uid-gid-sync-missing-target-user",
            r#"
            {
              "image": "alpine:3.20",
              "remoteUser": "missing-sync-user"
            }
            "#,
        );

        let result: anyhow::Result<()> = async {
            ensure_image(&scenario.client, "alpine:3.20", PullPolicy::Missing).await?;
            scenario.clean_before().await?;

            let error = scenario.run_detached().await.unwrap_err();
            let message = format!("{error:#}");
            assert!(message.contains("Remote user does not exist in container"));
            assert!(message.contains("missing-sync-user"));

            let containers =
                list_workspace_containers(&scenario.client, scenario.workspace.id()).await?;
            assert!(
                !containers
                    .iter()
                    .any(|container| container.name == scenario.container_name)
            );

            Ok(())
        }
        .await;

        scenario.finish(result).await;
    });
}

#[cfg(unix)]
#[test]
fn up_detach_fails_uid_gid_sync_when_host_ids_conflict() {
    let Some(host) = linux_host_ids() else {
        return;
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let scenario = UidGidScenario::with_image("docker-up-uid-gid-sync-conflict", |image| {
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
            build_uid_gid_conflict_user_image(
                &scenario.client,
                scenario.image(),
                host.uid,
                host.gid,
            )
            .await?;

            let error = scenario.run_detached().await.unwrap_err();
            let message = format!("{error:#}");
            assert!(
                message.contains("Failed to build Docker image")
                    && message.contains("sync-uid-gid.sh"),
                "{message}"
            );

            Ok(())
        }
        .await;

        scenario.finish(result).await;
    });
}

#[cfg(unix)]
#[test]
fn up_detach_fails_uid_gid_sync_when_host_ids_already_match_but_duplicates_exist() {
    let Some(host) = linux_host_ids() else {
        return;
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let scenario =
            UidGidScenario::with_image("docker-up-uid-gid-sync-duplicate-matching-ids", |image| {
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
            build_duplicate_matching_host_ids_image(
                &scenario.client,
                scenario.image(),
                host.uid,
                host.gid,
            )
            .await?;

            let error = scenario.run_detached().await.unwrap_err();
            let message = format!("{error:#}");
            assert!(
                message.contains("Failed to build Docker image")
                    && message.contains("sync-uid-gid.sh"),
                "{message}"
            );

            Ok(())
        }
        .await;

        scenario.finish(result).await;
    });
}

#[cfg(unix)]
#[test]
fn up_detach_fails_uid_gid_sync_gid_conflict_without_target_group_entry() {
    let Some(host) = linux_host_ids() else {
        return;
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let scenario =
            UidGidScenario::with_image("docker-up-uid-gid-sync-missing-target-group", |image| {
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
            build_missing_target_group_conflict_image(&scenario.client, scenario.image(), host.gid)
                .await?;

            let error = scenario.run_detached().await.unwrap_err();
            let message = format!("{error:#}");
            assert!(
                message.contains("Failed to build Docker image")
                    && message.contains("sync-uid-gid.sh"),
                "{message}"
            );

            Ok(())
        }
        .await;

        scenario.finish(result).await;
    });
}

#[cfg(unix)]
#[test]
fn up_detach_fails_uid_gid_sync_when_old_gid_matches_multiple_groups() {
    let Some(_host) = linux_host_ids_without(&[], &[2001]) else {
        return;
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let scenario =
            UidGidScenario::with_image("docker-up-uid-gid-sync-duplicate-old-gid", |image| {
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
            build_duplicate_old_gid_image(&scenario.client, scenario.image()).await?;

            let error = scenario.run_detached().await.unwrap_err();
            let message = format!("{error:#}");
            assert!(
                message.contains("Failed to build Docker image")
                    && message.contains("sync-uid-gid.sh"),
                "{message}"
            );

            Ok(())
        }
        .await;

        scenario.finish(result).await;
    });
}
