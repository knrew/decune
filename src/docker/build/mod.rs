use std::collections::BTreeMap;

use anyhow::{Result, bail};

mod context;
mod feature_layer;
mod tar;
mod uid_gid_layer;

pub(crate) use context::{ResolvedBuildContext, build_hash_input, resolve_build_context};
pub(crate) use feature_layer::{
    FEATURE_ENTRYPOINT_SENTINEL, FEATURE_ENTRYPOINT_WRAPPER, FeatureLayerBuildFeature,
    FeatureLayerBuildInput, prepare_feature_layer_build_context,
};
pub(crate) use uid_gid_layer::{
    UidGidSyncLayerBuildInput, prepare_uid_gid_sync_layer_build_context,
};

use crate::{
    docker::{client::DockerClient, lock::DockerResourceLock},
    runtime::docker_cli::DockerBuildCliInput,
    ui,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DockerBuildInput {
    pub(crate) image_tag: String,
    pub(crate) labels: std::collections::HashMap<String, String>,
    pub(crate) context: ResolvedBuildContext,
    pub(crate) options: DockerBuildOptions,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct DockerBuildOptions {
    pub(crate) build_args: BTreeMap<String, String>,
    pub(crate) build_arg_redactions: Vec<String>,
    pub(crate) options: Vec<String>,
    pub(crate) target: Option<String>,
    pub(crate) cache_from: Vec<String>,
    pub(crate) no_cache: bool,
    pub(crate) pull: bool,
}

pub(crate) async fn build_image(client: &DockerClient, input: DockerBuildInput) -> Result<()> {
    ui::info(&format!("Building Docker image: {}", input.image_tag));

    let tar = tar::create_build_context_tar(&input.context)?;
    let labels = input.labels.clone().into_iter().collect::<BTreeMap<_, _>>();
    let command_input = DockerBuildCliInput {
        image_tag: &input.image_tag,
        dockerfile: &input.context.dockerfile_in_context,
        context_tar: &tar,
        labels: &labels,
        build_args: &input.options.build_args,
        build_arg_redactions: &input.options.build_arg_redactions,
        options: &input.options.options,
        target: input.options.target.as_deref(),
        cache_from: &input.options.cache_from,
        no_cache: input.options.no_cache,
        pull: input.options.pull,
    };
    let _lock = DockerResourceLock::acquire_shared_from_env()?;
    let output = client.cli().build(command_input).await?;

    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let line = line.trim();
        if !line.is_empty() {
            ui::info(line);
        }
    }

    ui::done(&format!("Built Docker image: {}", input.image_tag));
    Ok(())
}

fn dockerfile_user(user: &str) -> Result<&str> {
    let trimmed = user.trim();
    if trimmed.is_empty() || trimmed != user {
        bail!("Docker image user must not be empty or contain surrounding whitespace");
    }
    if user
        .chars()
        .any(|character| character.is_ascii_control() || character.is_whitespace())
    {
        bail!("Docker image user contains unsupported whitespace or control characters: {user}");
    }

    Ok(user)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, HashMap},
        path::PathBuf,
    };

    use crate::runtime::docker_cli::{DockerBuildCliInput, docker_build_command};

    use super::{DockerBuildInput, DockerBuildOptions, ResolvedBuildContext};

    #[test]
    fn build_image_options_include_devcontainer_build_options() {
        let input = docker_build_input(DockerBuildOptions {
            build_args: [("VARIANT".to_owned(), "bookworm".to_owned())].into(),
            build_arg_redactions: Vec::new(),
            options: vec![
                "--platform=linux/amd64".to_owned(),
                "--network".to_owned(),
                "host".to_owned(),
            ],
            target: Some("dev".to_owned()),
            cache_from: vec!["type=registry,ref=example.test/cache:latest".to_owned()],
            no_cache: true,
            pull: true,
        });

        let labels = input.labels.clone().into_iter().collect::<BTreeMap<_, _>>();
        let command = docker_build_command(&DockerBuildCliInput {
            image_tag: &input.image_tag,
            dockerfile: &input.context.dockerfile_path,
            context_tar: b"",
            labels: &labels,
            build_args: &input.options.build_args,
            build_arg_redactions: &input.options.build_arg_redactions,
            options: &input.options.options,
            target: input.options.target.as_deref(),
            cache_from: &input.options.cache_from,
            no_cache: input.options.no_cache,
            pull: input.options.pull,
        });

        assert!(command.args_vec().contains(&"--build-arg".to_owned()));
        assert!(command.args_vec().contains(&"VARIANT".to_owned()));
        assert!(!command.args_vec().contains(&"VARIANT=bookworm".to_owned()));
        assert_eq!(
            command.env_value("VARIANT").map(String::as_str),
            Some("bookworm")
        );
        assert!(command.args_vec().contains(&"--target".to_owned()));
        assert!(command.args_vec().contains(&"dev".to_owned()));
        assert!(command.args_vec().contains(&"--cache-from".to_owned()));
        assert!(command.args_vec().contains(&"--no-cache".to_owned()));
        assert!(command.args_vec().contains(&"--pull".to_owned()));
        assert!(
            command
                .args_vec()
                .windows(2)
                .any(|args| args[0] == "--network" && args[1] == "host")
        );
        assert!(
            command
                .args_vec()
                .contains(&"--platform=linux/amd64".to_owned())
        );
    }

    #[test]
    fn build_image_options_omit_empty_optional_build_options() {
        let input = docker_build_input(DockerBuildOptions::default());

        let labels = input.labels.clone().into_iter().collect::<BTreeMap<_, _>>();
        let command = docker_build_command(&DockerBuildCliInput {
            image_tag: &input.image_tag,
            dockerfile: &input.context.dockerfile_path,
            context_tar: b"",
            labels: &labels,
            build_args: &input.options.build_args,
            build_arg_redactions: &input.options.build_arg_redactions,
            options: &input.options.options,
            target: input.options.target.as_deref(),
            cache_from: &input.options.cache_from,
            no_cache: input.options.no_cache,
            pull: input.options.pull,
        });

        assert!(!command.args_vec().contains(&"--build-arg".to_owned()));
        assert!(!command.args_vec().contains(&"--target".to_owned()));
        assert!(!command.args_vec().contains(&"--cache-from".to_owned()));
        assert!(!command.args_vec().contains(&"--no-cache".to_owned()));
        assert!(!command.args_vec().contains(&"--pull".to_owned()));
    }

    fn docker_build_input(options: DockerBuildOptions) -> DockerBuildInput {
        DockerBuildInput {
            image_tag: "decune/test:options".to_owned(),
            labels: HashMap::new(),
            context: ResolvedBuildContext {
                context_dir: PathBuf::new(),
                dockerfile_path: PathBuf::new(),
                dockerfile_in_context: PathBuf::from("Dockerfile"),
                dockerignore_path: None,
            },
            options,
        }
    }
}
