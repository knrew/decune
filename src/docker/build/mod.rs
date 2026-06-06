use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result, bail};
use bollard::{
    body_full,
    models::BuildInfo,
    query_parameters::{BuildImageOptions, BuildImageOptionsBuilder},
};
use futures_util::StreamExt;

mod context;
mod feature_layer;
mod tar;
mod uid_gid_layer;

pub(crate) use context::{ResolvedBuildContext, build_hash_input, resolve_build_context};
pub(crate) use feature_layer::{
    FEATURE_ENTRYPOINT_SENTINEL, FEATURE_ENTRYPOINT_WRAPPER, FeatureLayerBuildFeature,
    FeatureLayerBuildInput, prepare_feature_layer_build_context,
};
pub(crate) use tar::create_build_context_tar;
pub(crate) use uid_gid_layer::{
    UidGidSyncLayerBuildInput, prepare_uid_gid_sync_layer_build_context,
};

use crate::{
    docker::{client::DockerClient, lock::DockerResourceLock},
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
    pub(crate) target: Option<String>,
    pub(crate) cache_from: Vec<String>,
    pub(crate) no_cache: bool,
    pub(crate) pull: bool,
}

pub(crate) async fn build_image(client: &DockerClient, input: DockerBuildInput) -> Result<()> {
    ui::info(&format!("Building Docker image: {}", input.image_tag));

    let tar = create_build_context_tar(&input.context)?;
    let options = build_image_options(&input);
    let _lock = DockerResourceLock::acquire_shared_from_env()?;
    let mut stream = client
        .raw()
        .build_image(options, None, Some(body_full(tar.into())));

    while let Some(item) = stream.next().await {
        let info =
            item.with_context(|| format!("Failed to build Docker image: {}", input.image_tag))?;
        handle_build_info(&input.image_tag, info)?;
    }

    ui::done(&format!("Built Docker image: {}", input.image_tag));
    Ok(())
}

fn build_image_options(input: &DockerBuildInput) -> BuildImageOptions {
    let labels = input.labels.clone();
    let mut builder = BuildImageOptionsBuilder::default()
        .dockerfile(&tar::path_for_docker(&input.context.dockerfile_in_context))
        .t(&input.image_tag)
        .labels(&labels)
        .rm(true)
        .forcerm(true)
        .nocache(input.options.no_cache);

    if !input.options.build_args.is_empty() {
        let build_args = input
            .options
            .build_args
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<HashMap<_, _>>();
        builder = builder.buildargs(&build_args);
    }

    if let Some(target) = &input.options.target {
        builder = builder.target(target);
    }

    if !input.options.cache_from.is_empty() {
        builder = builder.cachefrom(&input.options.cache_from);
    }

    if input.options.pull {
        builder = builder.pull("true");
    }

    builder.build()
}

fn handle_build_info(image_tag: &str, info: BuildInfo) -> Result<()> {
    if let Some(error) = info.error_detail.and_then(|detail| detail.message) {
        bail!("Failed to build Docker image: {image_tag}: {error}");
    }

    if let Some(stream) = info.stream {
        let line = stream.trim();
        if !line.is_empty() {
            ui::info(line);
        }
    }

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
    use std::{collections::HashMap, path::PathBuf};

    use super::{DockerBuildInput, DockerBuildOptions, ResolvedBuildContext, build_image_options};

    #[test]
    fn build_image_options_include_devcontainer_build_options() {
        let input = docker_build_input(DockerBuildOptions {
            build_args: [("VARIANT".to_owned(), "bookworm".to_owned())].into(),
            target: Some("dev".to_owned()),
            cache_from: vec!["type=registry,ref=example.test/cache:latest".to_owned()],
            no_cache: true,
            pull: true,
        });

        let options = build_image_options(&input);

        assert_eq!(
            options.buildargs,
            Some(HashMap::from([(
                "VARIANT".to_owned(),
                "bookworm".to_owned()
            )]))
        );
        assert_eq!(options.target, "dev");
        assert_eq!(
            options.cachefrom,
            Some(vec![
                "type=registry,ref=example.test/cache:latest".to_owned()
            ])
        );
        assert!(options.nocache);
        assert_eq!(options.pull.as_deref(), Some("true"));
    }

    #[test]
    fn build_image_options_omit_empty_optional_build_options() {
        let input = docker_build_input(DockerBuildOptions::default());

        let options = build_image_options(&input);

        assert_eq!(options.buildargs, None);
        assert_eq!(options.target, "");
        assert_eq!(options.cachefrom, None);
        assert!(!options.nocache);
        assert_eq!(options.pull, None);
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
