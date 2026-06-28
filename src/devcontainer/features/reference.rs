use std::path::{Component, Path, PathBuf};

use anyhow::{Result, anyhow, bail};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FeatureRef {
    Oci(OciFeatureRef),
    Local(LocalFeatureRef),
}

impl FeatureRef {
    pub(crate) fn canonical_id(&self) -> &str {
        match self {
            Self::Oci(reference) => &reference.canonical_id,
            Self::Local(reference) => &reference.canonical_id,
        }
    }

    pub(crate) fn original(&self) -> &str {
        match self {
            Self::Oci(reference) => &reference.original,
            Self::Local(reference) => &reference.original,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OciFeatureRef {
    pub(crate) original: String,
    pub(crate) registry: String,
    pub(crate) repository: String,
    pub(crate) feature_id: String,
    pub(crate) tag: Option<String>,
    pub(crate) digest: Option<String>,
    pub(crate) canonical_id: String,
}

impl OciFeatureRef {
    pub(super) fn canonical_repository(&self) -> String {
        format!("{}/{}", self.registry, self.repository)
    }

    pub(super) fn repository_path(&self) -> String {
        format!("{}/{}", self.repository, self.feature_id)
    }

    pub(super) fn normalized_reference(&self) -> String {
        self.digest.as_ref().map_or_else(
            || {
                format!(
                    "{}:{}",
                    self.canonical_id,
                    self.tag.as_deref().unwrap_or("latest")
                )
            },
            |digest| format!("{}@{digest}", self.canonical_id),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalFeatureRef {
    pub(crate) original: String,
    pub(crate) path: PathBuf,
    pub(crate) canonical_id: String,
}

pub(crate) fn parse_feature_ref(value: &str) -> Result<FeatureRef> {
    parse_oci_feature_ref(value).map(FeatureRef::Oci)
}

pub(crate) fn parse_feature_ref_from_devcontainer_dir(
    value: &str,
    devcontainer_dir: &Path,
) -> Result<FeatureRef> {
    if value.starts_with("./") {
        return Ok(FeatureRef::Local(parse_local_feature_ref(
            value,
            devcontainer_dir,
        )?));
    }

    if Path::new(value).is_absolute() {
        return Err(invalid_feature_ref(
            value,
            "local Feature path must not be absolute; use a ./ path relative to the devcontainer directory",
        ));
    }
    if value.contains("://") {
        return Err(invalid_feature_ref(
            value,
            "URL scheme Feature refs are not supported",
        ));
    }
    if value == ".." || value.starts_with("../") {
        return Err(invalid_feature_ref(
            value,
            "local Feature path must not contain .. traversal; use a ./ path inside the devcontainer directory",
        ));
    }

    parse_feature_ref(value).map_err(|error| {
        if looks_like_relative_local_feature_path(value) {
            invalid_feature_ref(
                value,
                "local Feature paths must start with ./ and stay inside the devcontainer directory",
            )
        } else {
            error
        }
    })
}

pub(super) fn parse_oci_feature_ref(value: &str) -> Result<OciFeatureRef> {
    if value.starts_with("http://") || value.starts_with("https://") {
        return Err(invalid_feature_ref(
            value,
            "direct HTTPS Feature tarballs are not supported",
        ));
    }

    let (without_digest, digest) = split_digest(value)?;
    let (without_tag, tag) = split_tag(without_digest);
    let (registry, path) = without_tag
        .split_once('/')
        .ok_or_else(|| invalid_feature_ref(value, "missing registry or repository"))?;
    let last_slash = path
        .rfind('/')
        .ok_or_else(|| invalid_feature_ref(value, "missing repository or feature id"))?;
    let (repository, feature_id) = path.split_at(last_slash);
    let feature_id = feature_id
        .strip_prefix('/')
        .expect("feature path slash was found");

    if registry.is_empty()
        || repository.is_empty()
        || feature_id.is_empty()
        || tag.is_some_and(str::is_empty)
    {
        return Err(invalid_feature_ref(
            value,
            "expected <registry>/<repository>/<feature-id>:<tag> or @<digest>",
        ));
    }

    validate_oci_feature_registry(value, registry)?;
    validate_oci_feature_path(value, repository, "repository")?;
    validate_oci_feature_path_component(value, feature_id, "feature id")?;
    if let Some(tag) = tag {
        validate_oci_feature_tag(value, tag)?;
    }

    let registry = registry.to_ascii_lowercase();
    let repository = repository.to_ascii_lowercase();
    let feature_id = feature_id.to_ascii_lowercase();
    let canonical_id = format!("{registry}/{repository}/{feature_id}");
    let digest = digest
        .map(|digest| {
            validate_oci_digest(digest)
                .map(|()| digest.to_owned())
                .map_err(|error| invalid_feature_ref(value, &format!("invalid digest: {error}")))
        })
        .transpose()?;

    Ok(OciFeatureRef {
        original: value.to_owned(),
        registry,
        repository,
        feature_id,
        tag: tag
            .or_else(|| digest.is_none().then_some("latest"))
            .map(str::to_owned),
        digest,
        canonical_id,
    })
}

pub(super) fn validate_oci_digest(digest: &str) -> Result<()> {
    let Some(expected) = digest.strip_prefix("sha256:") else {
        bail!("Unsupported OCI digest algorithm: {digest}");
    };
    if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("Invalid OCI sha256 digest: {digest}");
    }
    if !expected
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        bail!("OCI sha256 digest must use lowercase hex: {digest}");
    }

    Ok(())
}

fn parse_local_feature_ref(value: &str, devcontainer_dir: &Path) -> Result<LocalFeatureRef> {
    let relative = value
        .strip_prefix("./")
        .filter(|path| !path.is_empty())
        .ok_or_else(|| invalid_feature_ref(value, "local feature path is empty"))?;
    let relative = normalize_local_feature_relative_path(value, relative)?;

    Ok(LocalFeatureRef {
        original: value.to_owned(),
        path: devcontainer_dir.join(&relative),
        canonical_id: format!("local:{}", relative.display()),
    })
}

fn normalize_local_feature_relative_path(value: &str, relative: &str) -> Result<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in Path::new(relative).components() {
        match component {
            Component::Normal(component) => normalized.push(component),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(invalid_feature_ref(
                    value,
                    "local Feature path must not contain .. traversal",
                ));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(invalid_feature_ref(
                    value,
                    "local Feature path must be relative to the devcontainer directory",
                ));
            }
        }
    }
    if normalized.as_os_str().is_empty() {
        return Err(invalid_feature_ref(value, "local feature path is empty"));
    }

    Ok(normalized)
}

fn looks_like_relative_local_feature_path(value: &str) -> bool {
    value.starts_with('.') || value.split('/').count() == 2
}

fn validate_oci_feature_registry(value: &str, registry: &str) -> Result<()> {
    if registry.contains("://")
        || registry.contains('/')
        || registry.bytes().any(|byte| byte.is_ascii_whitespace())
        || registry.chars().any(char::is_control)
    {
        return Err(invalid_feature_ref(
            value,
            "registry must be a host or host:port without a URL scheme",
        ));
    }

    if registry.starts_with('[') {
        return validate_oci_feature_ipv6_registry(value, registry);
    }

    let (host, port) = match registry.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (registry, None),
    };
    if host.is_empty() {
        return Err(invalid_feature_ref(value, "registry host is empty"));
    }
    if let Some(port) = port
        && (port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return Err(invalid_feature_ref(
            value,
            "registry port must contain only digits",
        ));
    }
    for component in host.split('.') {
        if component.is_empty()
            || component.starts_with('-')
            || component.ends_with('-')
            || !component
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(invalid_feature_ref(
                value,
                "registry host contains an invalid component",
            ));
        }
    }

    Ok(())
}

fn validate_oci_feature_ipv6_registry(value: &str, registry: &str) -> Result<()> {
    let Some(after_open) = registry.strip_prefix('[') else {
        return Err(invalid_feature_ref(
            value,
            "registry IPv6 host must be enclosed in brackets",
        ));
    };
    let Some(bracket_end) = after_open.find(']') else {
        return Err(invalid_feature_ref(
            value,
            "registry IPv6 host must be enclosed in brackets",
        ));
    };
    let (host, after_host) = after_open.split_at(bracket_end);
    if host.is_empty() {
        return Err(invalid_feature_ref(value, "registry host is empty"));
    }
    if host.contains('%') || host.parse::<std::net::Ipv6Addr>().is_err() {
        return Err(invalid_feature_ref(value, "registry IPv6 host is invalid"));
    }

    let rest = after_host
        .strip_prefix(']')
        .expect("registry IPv6 host closing bracket was found");
    if rest.is_empty() {
        return Ok(());
    }

    let Some(port) = rest.strip_prefix(':') else {
        return Err(invalid_feature_ref(
            value,
            "registry must be a host or host:port without a URL scheme",
        ));
    };
    if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid_feature_ref(
            value,
            "registry port must contain only digits",
        ));
    }

    Ok(())
}

fn validate_oci_feature_path(value: &str, path: &str, label: &str) -> Result<()> {
    for component in path.split('/') {
        validate_oci_feature_path_component(value, component, label)?;
    }

    Ok(())
}

fn validate_oci_feature_path_component(value: &str, component: &str, label: &str) -> Result<()> {
    if component.is_empty() {
        return Err(invalid_feature_ref(
            value,
            &format!("{label} contains an empty path component"),
        ));
    }

    let mut chars = component.chars().peekable();
    let Some(first) = chars.next() else {
        return Err(invalid_feature_ref(value, &format!("{label} is empty")));
    };
    if !first.is_ascii_alphanumeric() {
        return Err(invalid_feature_ref(
            value,
            &format!("{label} component must start with an alphanumeric character"),
        ));
    }

    while let Some(ch) = chars.next() {
        if ch.is_ascii_alphanumeric() {
            continue;
        }
        if ch == '-' {
            while chars.peek().is_some_and(|next| *next == '-') {
                chars.next();
            }
        } else if ch == '_' {
            if chars.peek().is_some_and(|next| *next == '_') {
                chars.next();
            }
        } else if ch != '.' {
            return Err(invalid_feature_ref(
                value,
                &format!("{label} contains an invalid character"),
            ));
        }

        if !chars.peek().is_some_and(char::is_ascii_alphanumeric) {
            return Err(invalid_feature_ref(
                value,
                &format!("{label} separator must be followed by an alphanumeric character"),
            ));
        }
    }

    Ok(())
}

fn validate_oci_feature_tag(value: &str, tag: &str) -> Result<()> {
    let mut bytes = tag.bytes();
    let Some(first) = bytes.next() else {
        return Err(invalid_feature_ref(value, "tag is empty"));
    };
    if !first.is_ascii_alphanumeric() && first != b'_' {
        return Err(invalid_feature_ref(
            value,
            "tag must start with an alphanumeric character or underscore",
        ));
    }
    if tag.len() > 128
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
    {
        return Err(invalid_feature_ref(
            value,
            "tag contains invalid characters",
        ));
    }

    Ok(())
}

pub(super) fn split_digest(value: &str) -> Result<(&str, Option<&str>)> {
    match value.split_once('@') {
        Some((base, digest)) if !base.is_empty() && !digest.is_empty() => Ok((base, Some(digest))),
        Some(_) => Err(invalid_feature_ref(value, "invalid digest")),
        None => Ok((value, None)),
    }
}

pub(super) fn split_tag(value: &str) -> (&str, Option<&str>) {
    let last_slash = value.rfind('/');
    let last_colon = value.rfind(':');

    match (last_slash, last_colon) {
        (_, Some(colon)) if last_slash.is_none_or(|slash| colon > slash) => {
            let (base, tag) = value.split_at(colon);
            (base, tag.strip_prefix(':'))
        }
        _ => (value, None),
    }
}

fn invalid_feature_ref(value: &str, reason: &str) -> anyhow::Error {
    anyhow!("Invalid feature ref `{value}`: {reason}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST_DIGEST_HEX: &str =
        "1111111111111111111111111111111111111111111111111111111111111111";

    #[test]
    fn parses_tagged_oci_feature_ref() {
        let reference = parse_feature_ref("ghcr.io/devcontainers/features/go:1").unwrap();

        assert_eq!(
            reference,
            FeatureRef::Oci(OciFeatureRef {
                original: "ghcr.io/devcontainers/features/go:1".to_owned(),
                registry: "ghcr.io".to_owned(),
                repository: "devcontainers/features".to_owned(),
                feature_id: "go".to_owned(),
                tag: Some("1".to_owned()),
                digest: None,
                canonical_id: "ghcr.io/devcontainers/features/go".to_owned(),
            })
        );
    }

    #[test]
    fn parses_oci_feature_ref_case_insensitively() {
        let reference = parse_feature_ref("GHCR.IO/DevContainers/Features/GitHub-CLI:1").unwrap();

        assert_eq!(
            reference,
            FeatureRef::Oci(OciFeatureRef {
                original: "GHCR.IO/DevContainers/Features/GitHub-CLI:1".to_owned(),
                registry: "ghcr.io".to_owned(),
                repository: "devcontainers/features".to_owned(),
                feature_id: "github-cli".to_owned(),
                tag: Some("1".to_owned()),
                digest: None,
                canonical_id: "ghcr.io/devcontainers/features/github-cli".to_owned(),
            })
        );
    }

    #[test]
    fn parses_digest_oci_feature_ref_with_registry_port() {
        let digest = sha256_digest(MANIFEST_DIGEST_HEX);
        let reference = parse_feature_ref(&format!(
            "localhost:5000/devcontainers/features/tool@{digest}"
        ))
        .unwrap();

        assert_eq!(
            reference,
            FeatureRef::Oci(OciFeatureRef {
                original: format!("localhost:5000/devcontainers/features/tool@{digest}"),
                registry: "localhost:5000".to_owned(),
                repository: "devcontainers/features".to_owned(),
                feature_id: "tool".to_owned(),
                tag: None,
                digest: Some(digest),
                canonical_id: "localhost:5000/devcontainers/features/tool".to_owned(),
            })
        );
    }

    #[test]
    fn parses_oci_feature_ref_with_bracketed_ipv6_registry() {
        let reference = parse_feature_ref("[2001:db8::1]:5000/example/features/tool:1").unwrap();

        assert_eq!(
            reference,
            FeatureRef::Oci(OciFeatureRef {
                original: "[2001:db8::1]:5000/example/features/tool:1".to_owned(),
                registry: "[2001:db8::1]:5000".to_owned(),
                repository: "example/features".to_owned(),
                feature_id: "tool".to_owned(),
                tag: Some("1".to_owned()),
                digest: None,
                canonical_id: "[2001:db8::1]:5000/example/features/tool".to_owned(),
            })
        );
    }

    #[test]
    fn bracketed_ipv6_oci_feature_ref_without_tag_defaults_to_latest() {
        let reference = parse_feature_ref("[2001:db8::1]/example/features/tool").unwrap();

        assert_eq!(
            reference,
            FeatureRef::Oci(OciFeatureRef {
                original: "[2001:db8::1]/example/features/tool".to_owned(),
                registry: "[2001:db8::1]".to_owned(),
                repository: "example/features".to_owned(),
                feature_id: "tool".to_owned(),
                tag: Some("latest".to_owned()),
                digest: None,
                canonical_id: "[2001:db8::1]/example/features/tool".to_owned(),
            })
        );
    }

    #[test]
    fn oci_feature_ref_without_tag_defaults_to_latest() {
        let reference = parse_feature_ref("ghcr.io/devcontainers/features/go").unwrap();

        assert_eq!(
            reference,
            FeatureRef::Oci(OciFeatureRef {
                original: "ghcr.io/devcontainers/features/go".to_owned(),
                registry: "ghcr.io".to_owned(),
                repository: "devcontainers/features".to_owned(),
                feature_id: "go".to_owned(),
                tag: Some("latest".to_owned()),
                digest: None,
                canonical_id: "ghcr.io/devcontainers/features/go".to_owned(),
            })
        );
    }

    #[test]
    fn invalid_feature_ref_error_includes_ref() {
        let error = parse_feature_ref("ghcr.io/features").unwrap_err();

        assert!(error.to_string().contains("ghcr.io/features"), "{error:#}");

        let error = parse_feature_ref("ghcr.io/example/features/tool:").unwrap_err();

        assert!(
            error.to_string().contains("ghcr.io/example/features/tool:"),
            "{error:#}"
        );
    }

    #[test]
    fn oci_feature_ref_rejects_unsupported_https_tarballs_and_malformed_names() {
        for reference in [
            "https://github.com/owner/repo/releases/devcontainer-feature-tool.tgz",
            "http://example.com/devcontainer-feature-tool.tgz",
            "ghcr.io//features/tool",
            "ghcr.io/example/features/tool with space:1",
            "ghcr.io/example/features/tool:bad tag",
            "ghcr.io/example/features/tool:bad\tnew",
            "ghcr.io/example/feat\nures/tool:1",
            "2001:db8::1/example/features/tool:1",
            "[2001:db8::1:5000/example/features/tool:1",
            "[2001:db8::1]:bad/example/features/tool:1",
            "[fe80::1%eth0]/example/features/tool:1",
        ] {
            let error = parse_feature_ref(reference).unwrap_err();

            assert!(error.to_string().contains(reference), "{error:#}");
        }
    }

    #[test]
    fn oci_feature_ref_rejects_invalid_digest_values() {
        for reference in [
            "ghcr.io/example/features/tool@../../x",
            "ghcr.io/example/features/tool@sha256:abcd",
            "ghcr.io/example/features/tool@sha512:1111111111111111111111111111111111111111111111111111111111111111",
        ] {
            let error = parse_feature_ref(reference).unwrap_err();

            assert!(error.to_string().contains(reference), "{error:#}");
            assert!(error.to_string().contains("digest"), "{error:#}");
        }
    }

    #[test]
    fn local_feature_path_is_resolved_from_devcontainer_dir() {
        let devcontainer_dir = Path::new("/workspace/.devcontainer");
        let reference =
            parse_feature_ref_from_devcontainer_dir("./features/local", devcontainer_dir).unwrap();

        assert_eq!(
            reference,
            FeatureRef::Local(LocalFeatureRef {
                original: "./features/local".to_owned(),
                path: devcontainer_dir.join("features/local"),
                canonical_id: "local:features/local".to_owned(),
            })
        );
    }

    #[test]
    fn local_feature_path_rejects_absolute_path() {
        let error = parse_feature_ref_from_devcontainer_dir(
            "/workspace/.devcontainer/features/local",
            Path::new("/workspace/.devcontainer"),
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("/workspace/.devcontainer/features/local"),
            "{error:#}"
        );
        assert!(error.to_string().contains("absolute"), "{error:#}");
    }

    #[test]
    fn local_feature_path_rejects_url_scheme() {
        let error = parse_feature_ref_from_devcontainer_dir(
            "file:///workspace/.devcontainer/features/local",
            Path::new("/workspace/.devcontainer"),
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("file:///workspace/.devcontainer/features/local"),
            "{error:#}"
        );
        assert!(error.to_string().contains("URL scheme"), "{error:#}");
    }

    #[test]
    fn local_feature_path_rejects_relative_path_without_dot_slash() {
        let error = parse_feature_ref_from_devcontainer_dir(
            "features/local",
            Path::new("/workspace/.devcontainer"),
        )
        .unwrap_err();

        assert!(error.to_string().contains("features/local"), "{error:#}");
        assert!(error.to_string().contains("./"), "{error:#}");
    }

    #[test]
    fn local_feature_path_rejects_parent_directory_traversal() {
        for reference in ["../outside", "./../outside", "./features/../outside"] {
            let error = parse_feature_ref_from_devcontainer_dir(
                reference,
                Path::new("/workspace/.devcontainer"),
            )
            .unwrap_err();

            assert!(error.to_string().contains(reference), "{error:#}");
            assert!(error.to_string().contains(".."), "{error:#}");
        }
    }

    fn sha256_digest(hex: &str) -> String {
        format!("sha256:{hex}")
    }
}
