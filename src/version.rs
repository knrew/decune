use std::process::Command;

pub(crate) fn display_version() -> String {
    display_version_from(&BuildVersionMetadata::from_env(), &CommandGitProbe)
}

#[derive(Debug)]
struct BuildVersionMetadata {
    package_version: &'static str,
    display_version: &'static str,
    source_root: Option<&'static str>,
    full_commit: Option<&'static str>,
    short_commit: Option<&'static str>,
    release_tag_matches: bool,
}

impl BuildVersionMetadata {
    fn from_env() -> Self {
        Self {
            package_version: env!("CARGO_PKG_VERSION"),
            display_version: env!("DECUNE_DISPLAY_VERSION"),
            source_root: non_empty(option_env!("DECUNE_VERSION_SOURCE_ROOT")),
            full_commit: non_empty(option_env!("DECUNE_VERSION_FULL_COMMIT")),
            short_commit: non_empty(option_env!("DECUNE_VERSION_SHORT_COMMIT")),
            release_tag_matches: option_env!("DECUNE_VERSION_RELEASE_TAG_MATCHES") == Some("true"),
        }
    }
}

fn display_version_from(metadata: &BuildVersionMetadata, probe: &impl GitProbe) -> String {
    let Some(source_root) = metadata.source_root else {
        return metadata.display_version.to_owned();
    };
    let Some(full_commit) = metadata.full_commit else {
        return metadata.display_version.to_owned();
    };
    let Some(short_commit) = metadata.short_commit else {
        return metadata.display_version.to_owned();
    };

    let Some(head) = probe.head_commit(source_root) else {
        return metadata.display_version.to_owned();
    };
    if head != full_commit {
        return metadata.display_version.to_owned();
    }

    let Some(dirty) = probe.dirty(source_root) else {
        return metadata.display_version.to_owned();
    };

    if !dirty && metadata.release_tag_matches {
        return metadata.package_version.to_owned();
    }

    let dirty_suffix = if dirty { ".dirty" } else { "" };
    format!(
        "{}+g{}{}",
        metadata.package_version, short_commit, dirty_suffix
    )
}

trait GitProbe {
    fn head_commit(&self, source_root: &str) -> Option<String>;

    fn dirty(&self, source_root: &str) -> Option<bool>;
}

struct CommandGitProbe;

impl GitProbe for CommandGitProbe {
    fn head_commit(&self, source_root: &str) -> Option<String> {
        git_output(source_root, ["rev-parse", "HEAD"])
    }

    fn dirty(&self, source_root: &str) -> Option<bool> {
        git_output(
            source_root,
            [
                "--no-optional-locks",
                "status",
                "--porcelain",
                "--untracked-files=normal",
            ],
        )
        .map(|status| !status.is_empty())
    }
}

fn git_output<const N: usize>(source_root: &str, args: [&str; N]) -> Option<String> {
    let output = Command::new("git")
        .current_dir(source_root)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn non_empty(value: Option<&'static str>) -> Option<&'static str> {
    value.filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_dirty_on_build_commit_adds_dirty_suffix() {
        let metadata = BuildVersionMetadata {
            package_version: "1.2.3",
            display_version: "1.2.3",
            source_root: Some("/workspace/decune"),
            full_commit: Some("1234567890abcdef"),
            short_commit: Some("1234567890ab"),
            release_tag_matches: true,
        };
        let probe = FakeGitProbe {
            head: Some("1234567890abcdef"),
            dirty: Some(true),
        };

        assert_eq!(
            display_version_from(&metadata, &probe),
            "1.2.3+g1234567890ab.dirty"
        );
    }

    #[test]
    fn runtime_clean_release_tag_keeps_plain_package_version() {
        let metadata = BuildVersionMetadata {
            package_version: "1.2.3",
            display_version: "1.2.3",
            source_root: Some("/workspace/decune"),
            full_commit: Some("1234567890abcdef"),
            short_commit: Some("1234567890ab"),
            release_tag_matches: true,
        };
        let probe = FakeGitProbe {
            head: Some("1234567890abcdef"),
            dirty: Some(false),
        };

        assert_eq!(display_version_from(&metadata, &probe), "1.2.3");
    }

    #[test]
    fn runtime_clean_non_release_tag_keeps_commit_metadata() {
        let metadata = BuildVersionMetadata {
            package_version: "1.2.3",
            display_version: "1.2.3+g1234567890ab",
            source_root: Some("/workspace/decune"),
            full_commit: Some("1234567890abcdef"),
            short_commit: Some("1234567890ab"),
            release_tag_matches: false,
        };
        let probe = FakeGitProbe {
            head: Some("1234567890abcdef"),
            dirty: Some(false),
        };

        assert_eq!(
            display_version_from(&metadata, &probe),
            "1.2.3+g1234567890ab"
        );
    }

    #[test]
    fn runtime_missing_source_root_falls_back_to_build_display_version() {
        let metadata = BuildVersionMetadata {
            package_version: "1.2.3",
            display_version: "1.2.3+g1234567890ab",
            source_root: None,
            full_commit: Some("1234567890abcdef"),
            short_commit: Some("1234567890ab"),
            release_tag_matches: false,
        };
        let probe = FakeGitProbe {
            head: Some("1234567890abcdef"),
            dirty: Some(true),
        };

        assert_eq!(
            display_version_from(&metadata, &probe),
            "1.2.3+g1234567890ab"
        );
    }

    #[test]
    fn runtime_head_mismatch_falls_back_to_build_display_version() {
        let metadata = BuildVersionMetadata {
            package_version: "1.2.3",
            display_version: "1.2.3+g1234567890ab",
            source_root: Some("/workspace/decune"),
            full_commit: Some("1234567890abcdef"),
            short_commit: Some("1234567890ab"),
            release_tag_matches: false,
        };
        let probe = FakeGitProbe {
            head: Some("abcdef1234567890"),
            dirty: Some(true),
        };

        assert_eq!(
            display_version_from(&metadata, &probe),
            "1.2.3+g1234567890ab"
        );
    }

    #[test]
    fn runtime_git_failure_falls_back_to_build_display_version() {
        let metadata = BuildVersionMetadata {
            package_version: "1.2.3",
            display_version: "1.2.3+g1234567890ab",
            source_root: Some("/workspace/decune"),
            full_commit: Some("1234567890abcdef"),
            short_commit: Some("1234567890ab"),
            release_tag_matches: false,
        };
        let probe = FakeGitProbe {
            head: Some("1234567890abcdef"),
            dirty: None,
        };

        assert_eq!(
            display_version_from(&metadata, &probe),
            "1.2.3+g1234567890ab"
        );
    }

    struct FakeGitProbe {
        head: Option<&'static str>,
        dirty: Option<bool>,
    }

    impl GitProbe for FakeGitProbe {
        fn head_commit(&self, _source_root: &str) -> Option<String> {
            self.head.map(str::to_owned)
        }

        fn dirty(&self, _source_root: &str) -> Option<bool> {
            self.dirty
        }
    }
}
