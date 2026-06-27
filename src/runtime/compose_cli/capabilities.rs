use anyhow::{Result, bail};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposeCliCapabilities {
    pub(crate) version_short: Option<String>,
    pub(crate) config_format_json: bool,
    pub(crate) ps_format_json: bool,
    pub(crate) build_with_dependencies: bool,
    pub(crate) pull_policy_always: bool,
    pub(crate) pull_ignore_buildable: bool,
    pub(crate) pull_include_deps: bool,
    pub(crate) up_force_recreate: bool,
    pub(crate) up_remove_orphans: bool,
}

impl ComposeCliCapabilities {
    const COMPOSE_OVERRIDE_TAG_MIN_VERSION: (u64, u64, u64) = (2, 24, 4);

    pub(crate) fn from_help_outputs(
        version_short: Option<String>,
        config_help: &str,
        ps_help: &str,
        build_help: &str,
        pull_help: &str,
        up_help: &str,
    ) -> Self {
        Self {
            version_short,
            config_format_json: help_contains_option(config_help, "--format"),
            ps_format_json: help_contains_option(ps_help, "--format"),
            build_with_dependencies: help_contains_option(build_help, "--with-dependencies"),
            pull_policy_always: help_contains_option(pull_help, "--policy"),
            pull_ignore_buildable: help_contains_option(pull_help, "--ignore-buildable"),
            pull_include_deps: help_contains_option(pull_help, "--include-deps"),
            up_force_recreate: help_contains_option(up_help, "--force-recreate"),
            up_remove_orphans: help_contains_option(up_help, "--remove-orphans"),
        }
    }

    pub(crate) fn ensure_required(&self) -> Result<()> {
        let mut missing = Vec::new();
        if !self.config_format_json {
            missing
                .push("docker compose config --format json (config --help does not list --format)");
        }
        if !self.ps_format_json {
            missing.push("docker compose ps --format json (ps --help does not list --format)");
        }
        if !self.build_with_dependencies {
            missing.push(
                "docker compose build --with-dependencies (build --help does not list --with-dependencies)",
            );
        }
        if !self.pull_policy_always {
            missing
                .push("docker compose pull --policy always (pull --help does not list --policy)");
        }
        if !self.pull_ignore_buildable {
            missing.push(
                "docker compose pull --ignore-buildable (pull --help does not list --ignore-buildable)",
            );
        }
        if !self.pull_include_deps {
            missing.push(
                "docker compose pull --include-deps (pull --help does not list --include-deps)",
            );
        }
        if !self.up_force_recreate {
            missing.push(
                "docker compose up --force-recreate (up --help does not list --force-recreate)",
            );
        }
        if !self.up_remove_orphans {
            missing.push(
                "docker compose up --remove-orphans (up --help does not list --remove-orphans)",
            );
        }
        if missing.is_empty() {
            return Ok(());
        }

        bail!(
            "Docker Compose v2 plugin is missing required capabilities: {}. Update Docker Compose v2 plugin to a newer release.",
            missing.join("; ")
        )
    }

    pub(crate) fn ensure_compose_override_tag(&self) -> Result<()> {
        let Some(version) = self
            .version_short
            .as_deref()
            .and_then(parse_compose_version)
        else {
            bail!(
                "Compose published port relocation requires Docker Compose v2.24.4 or newer; failed to determine Docker Compose version"
            );
        };

        if version < Self::COMPOSE_OVERRIDE_TAG_MIN_VERSION {
            bail!(
                "Compose published port relocation requires Docker Compose v2.24.4 or newer; detected Docker Compose v{}.{}.{}",
                version.0,
                version.1,
                version.2
            );
        }

        Ok(())
    }
}

fn help_contains_option(help: &str, option: &str) -> bool {
    help.split(|ch: char| {
        ch.is_ascii_whitespace() || matches!(ch, ',' | ';' | '[' | ']' | '(' | ')' | '{' | '}')
    })
    .any(|token| token == option || token.starts_with(&format!("{option}=")))
}

fn parse_compose_version(version: &str) -> Option<(u64, u64, u64)> {
    let trimmed = version.trim().trim_start_matches('v');
    let mut parts = trimmed.split(['.', '-']);
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::super::test_support::valid_compose_capabilities;
    use super::*;

    #[test]
    fn compose_capability_valid_help_output_detects_required_options() {
        let capabilities = valid_compose_capabilities();

        assert_eq!(capabilities.version_short.as_deref(), Some("2.40.0"));
        assert!(capabilities.config_format_json);
        assert!(capabilities.ps_format_json);
        assert!(capabilities.build_with_dependencies);
        assert!(capabilities.pull_policy_always);
        assert!(capabilities.pull_ignore_buildable);
        assert!(capabilities.pull_include_deps);
        assert!(capabilities.up_force_recreate);
        assert!(capabilities.up_remove_orphans);
        capabilities.ensure_required().unwrap();
        capabilities.ensure_compose_override_tag().unwrap();
    }

    #[test]
    fn compose_capability_accepts_override_tag_minimum_version() {
        let capabilities = ComposeCliCapabilities {
            version_short: Some("v2.24.4".to_owned()),
            ..valid_compose_capabilities()
        };

        capabilities.ensure_compose_override_tag().unwrap();
    }

    #[test]
    fn compose_capability_rejects_old_override_tag_version() {
        let capabilities = ComposeCliCapabilities {
            version_short: Some("2.24.3".to_owned()),
            ..valid_compose_capabilities()
        };

        let error = capabilities
            .ensure_compose_override_tag()
            .unwrap_err()
            .to_string();

        assert!(error.contains(
            "Compose published port relocation requires Docker Compose v2.24.4 or newer"
        ));
        assert!(error.contains("detected Docker Compose v2.24.3"));
    }

    #[test]
    fn compose_capability_rejects_unknown_override_tag_version() {
        let capabilities = ComposeCliCapabilities {
            version_short: None,
            ..valid_compose_capabilities()
        };

        let error = capabilities
            .ensure_compose_override_tag()
            .unwrap_err()
            .to_string();

        assert!(error.contains(
            "Compose published port relocation requires Docker Compose v2.24.4 or newer"
        ));
        assert!(error.contains("failed to determine Docker Compose version"));
    }

    #[test]
    fn compose_capability_missing_build_with_dependencies_errors_clearly() {
        let capabilities = ComposeCliCapabilities::from_help_outputs(
            Some("2.3.0".to_owned()),
            "--format string",
            "--format string",
            "--no-cache --pull",
            "--policy string --ignore-buildable --include-deps",
            "--force-recreate --remove-orphans",
        );

        let error = capabilities.ensure_required().unwrap_err().to_string();

        assert!(error.contains("docker compose build --with-dependencies"));
        assert!(error.contains("build --help does not list --with-dependencies"));
        assert!(error.contains("Update Docker Compose v2 plugin"));
    }

    #[test]
    fn compose_capability_missing_pull_include_deps_errors_clearly() {
        let capabilities = ComposeCliCapabilities::from_help_outputs(
            Some("2.3.0".to_owned()),
            "--format string",
            "--format string",
            "--with-dependencies",
            "--policy string --ignore-buildable",
            "--force-recreate --remove-orphans",
        );

        let error = capabilities.ensure_required().unwrap_err().to_string();

        assert!(error.contains("docker compose pull --include-deps"));
        assert!(error.contains("pull --help does not list --include-deps"));
        assert!(error.contains("Update Docker Compose v2 plugin"));
    }

    #[test]
    fn compose_capability_missing_config_format_mentions_config_format_json() {
        let capabilities = ComposeCliCapabilities::from_help_outputs(
            Some("2.3.0".to_owned()),
            "--services",
            "--format string",
            "--with-dependencies",
            "--policy string --ignore-buildable --include-deps",
            "--force-recreate --remove-orphans",
        );

        let error = capabilities.ensure_required().unwrap_err().to_string();

        assert!(error.contains("docker compose config --format json"));
        assert!(error.contains("config --help does not list --format"));
    }

    #[test]
    fn compose_capability_missing_up_options_prompts_compose_plugin_update() {
        let capabilities = ComposeCliCapabilities::from_help_outputs(
            Some("2.3.0".to_owned()),
            "--format string",
            "--format string",
            "--with-dependencies",
            "--policy string --ignore-buildable --include-deps",
            "--detach",
        );

        let error = capabilities.ensure_required().unwrap_err().to_string();

        assert!(error.contains("docker compose up --force-recreate"));
        assert!(error.contains("docker compose up --remove-orphans"));
        assert!(error.contains("Update Docker Compose v2 plugin"));
    }
}
