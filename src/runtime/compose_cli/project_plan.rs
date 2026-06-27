use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Result, anyhow};

use crate::{
    config::{canonical::sha256_hex, hash::ComposeFileHashInput},
    error::ResultExt,
    workspace::Workspace,
};

use super::command_plan::ComposeCommandPlan;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposeProjectPlan {
    project_name: String,
    pub(crate) project_directory: PathBuf,
    files: Vec<ComposeFilePlan>,
    generated_override_path: PathBuf,
    config_hash_files: Vec<ComposeFileHashInput>,
    generated_override_env: BTreeMap<String, String>,
    generated_override_redactions: Vec<String>,
}

impl ComposeProjectPlan {
    pub(crate) fn resolve(
        workspace: &Workspace,
        devcontainer_dir: &Path,
        compose_files: &[String],
    ) -> Result<Self> {
        if compose_files.is_empty() {
            return Err(anyhow!("dockerComposeFile must not be empty"));
        }

        let files = compose_files
            .iter()
            .map(|file| resolve_compose_file(devcontainer_dir, file))
            .collect::<Result<Vec<_>>>()?;
        let first_file = files
            .first()
            .expect("compose files are checked as non-empty before resolution");
        let project_directory_path = first_file.resolved_path.parent().ok_or_else(|| {
            anyhow!(
                "Failed to resolve Docker Compose project directory from file: {}",
                first_file.resolved_path.display()
            )
        })?;
        let project_directory = project_directory_path.canonicalize().with_path_context(
            "canonicalize Docker Compose project directory",
            project_directory_path,
        )?;
        let config_hash_files = files
            .iter()
            .map(|file| ComposeFileHashInput {
                canonical_path: file.canonical_path.display().to_string(),
                digest: file.digest.clone(),
            })
            .collect();

        Ok(Self {
            project_name: compose_project_name(workspace),
            project_directory,
            files,
            generated_override_path: workspace.paths().state_dir().join("compose.override.yaml"),
            config_hash_files,
            generated_override_env: BTreeMap::new(),
            generated_override_redactions: Vec::new(),
        })
    }

    pub(crate) fn project_name(&self) -> &str {
        &self.project_name
    }

    #[cfg(test)]
    pub(crate) fn project_directory(&self) -> &Path {
        &self.project_directory
    }

    pub(crate) fn generated_override_path(&self) -> PathBuf {
        self.generated_override_path.clone()
    }

    pub(crate) fn config_hash_files(&self) -> &[ComposeFileHashInput] {
        &self.config_hash_files
    }

    pub(crate) fn with_generated_override_env(
        mut self,
        env: BTreeMap<String, String>,
        redactions: Vec<String>,
    ) -> Self {
        self.generated_override_env = env;
        self.generated_override_redactions = redactions;
        self
    }

    pub(crate) fn command_plan_without_generated_override(&self) -> ComposeCommandPlan {
        ComposeCommandPlan {
            project_name: self.project_name.clone(),
            project_directory: self.project_directory.clone(),
            files: self
                .files
                .iter()
                .map(|file| file.canonical_path.clone())
                .collect(),
            env: BTreeMap::new(),
            redactions: Vec::new(),
        }
    }

    pub(crate) fn command_plan_with_generated_override(&self) -> ComposeCommandPlan {
        let mut files = self
            .files
            .iter()
            .map(|file| file.canonical_path.clone())
            .collect::<Vec<_>>();
        files.push(self.generated_override_path.clone());

        ComposeCommandPlan {
            project_name: self.project_name.clone(),
            project_directory: self.project_directory.clone(),
            files,
            env: self.generated_override_env.clone(),
            redactions: self.generated_override_redactions.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ComposeFilePlan {
    resolved_path: PathBuf,
    canonical_path: PathBuf,
    digest: String,
}

fn compose_project_name(workspace: &Workspace) -> String {
    format!("decune-{}-{}", workspace.safe_slug(), workspace.id())
}

fn resolve_compose_file(devcontainer_dir: &Path, value: &str) -> Result<ComposeFilePlan> {
    reject_unsupported_compose_file_reference(value)?;

    let path = Path::new(value);
    let resolved_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        devcontainer_dir.join(path)
    };
    let canonical_path = resolved_path
        .canonicalize()
        .with_path_context("canonicalize Docker Compose file", &resolved_path)?;
    let contents =
        fs::read(&canonical_path).with_path_context("read Docker Compose file", &canonical_path)?;

    Ok(ComposeFilePlan {
        resolved_path,
        canonical_path,
        digest: sha256_hex(&contents),
    })
}

fn reject_unsupported_compose_file_reference(value: &str) -> Result<()> {
    if value == "-" || value.contains("://") {
        return Err(anyhow!(
            "Unsupported dockerComposeFile reference: {value}. Only local file paths are supported"
        ));
    }

    Ok(())
}
