// Provides fixture setup shared by UID/GID Docker-backed scenario tests.

use std::collections::BTreeMap;

use crate::{
    config::ConfigLayer,
    docker::{
        client::DockerClient,
        container::remove_container,
        exec::{ExecCommandSpec, exec_capture},
        image::remove_image,
        user::{HostPlatform, HostUserIds, current_host_user_ids},
    },
    up::{
        UpOptions,
        plan::build_up_plan,
        run_detached_up,
        test_support::{TestWorkspace, test_workspace, write_devcontainer},
    },
};

pub(super) struct UidGidScenario {
    pub(super) workspace: TestWorkspace,
    pub(super) client: DockerClient,
    pub(super) container_name: String,
    image: Option<String>,
}

impl UidGidScenario {
    pub(super) fn with_image(
        name: &str,
        devcontainer_contents: impl FnOnce(&str) -> String,
    ) -> Self {
        Self::with_image_workspace(name, |workspace, image| {
            write_devcontainer(workspace, &devcontainer_contents(image));
        })
    }

    pub(super) fn with_image_workspace(
        name: &str,
        configure: impl FnOnce(&TestWorkspace, &str),
    ) -> Self {
        let workspace = test_workspace(name);
        let image = format!("decune-test/{name}-{}:latest", workspace.id());
        configure(&workspace, &image);
        Self::from_workspace(workspace, Some(image))
    }

    pub(super) fn with_devcontainer(name: &str, devcontainer_contents: &str) -> Self {
        let workspace = test_workspace(name);
        write_devcontainer(&workspace, devcontainer_contents);
        Self::from_workspace(workspace, None)
    }

    fn from_workspace(workspace: TestWorkspace, image: Option<String>) -> Self {
        let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
        let container_name = plan.resources.container_name.clone();
        let client = DockerClient::connect_from_env().unwrap();

        Self {
            workspace,
            client,
            container_name,
            image,
        }
    }

    pub(super) fn image(&self) -> &str {
        self.image.as_deref().unwrap()
    }

    pub(super) async fn clean_before(&self) -> anyhow::Result<()> {
        remove_container(&self.client, &self.container_name, true, true).await?;
        if let Some(image) = &self.image {
            remove_image(&self.client, image, true).await?;
        }
        Ok(())
    }

    pub(super) async fn run_detached(&self) -> anyhow::Result<crate::up::UpOutcome> {
        run_detached_up(UpOptions {
            workspace: self.workspace.root().to_path_buf(),
            config_path: None,
            skip_global_config: false,
            cli_layer: ConfigLayer::default(),
            pull: false,
            rebuild: false,
            no_cache: false,
            update_features: false,
        })
        .await
    }

    pub(super) async fn exec_sh_capture(
        &self,
        script: &str,
        user: Option<&str>,
    ) -> anyhow::Result<String> {
        let output = exec_capture(
            &self.client,
            &self.container_name,
            &ExecCommandSpec {
                command: vec!["/bin/sh".to_owned(), "-c".to_owned(), script.to_owned()],
                user: user.map(ToOwned::to_owned),
                working_dir: None,
                env: BTreeMap::new(),
                redactions: Vec::new(),
                tty: false,
            },
        )
        .await?;

        Ok(String::from_utf8(output.stdout)?)
    }

    pub(super) async fn finish(&self, result: anyhow::Result<()>) {
        let container_cleanup =
            remove_container(&self.client, &self.container_name, true, true).await;
        let image_cleanup = match &self.image {
            Some(image) => remove_image(&self.client, image, true).await,
            None => Ok(()),
        };
        result.and(container_cleanup).and(image_cleanup).unwrap();
    }
}

#[cfg(unix)]
pub(super) fn linux_host_ids_without(
    excluded_uids: &[u32],
    excluded_gids: &[u32],
) -> Option<HostUserIds> {
    if HostPlatform::current() != HostPlatform::Linux {
        return None;
    }

    let host = current_host_user_ids();
    if host.uid == 0
        || host.gid == 0
        || excluded_uids.contains(&host.uid)
        || excluded_gids.contains(&host.gid)
    {
        return None;
    }

    Some(host)
}

#[cfg(unix)]
pub(super) fn linux_host_ids() -> Option<HostUserIds> {
    linux_host_ids_without(&[], &[])
}
