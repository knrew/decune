use std::{os::unix::ffi::OsStrExt, path::PathBuf};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::{
    config::canonical::sha256_hex,
    host::forward::forward_status_dir,
    workspace::{Workspace, is_valid_workspace_id},
};

const QUERY_CONTEXT_FINGERPRINT_DOMAIN: &[u8] = b"decune-cli-query-context-v1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContainerCliQueryContext {
    workspace_id: String,
    state_dir: PathBuf,
    runtime_dir: PathBuf,
    forward_status_dir: PathBuf,
    context_fingerprint: String,
}

impl ContainerCliQueryContext {
    fn for_workspace(workspace: &Workspace) -> Result<Self> {
        Self::from_parts(
            workspace.id(),
            workspace.paths().state_dir().to_path_buf(),
            workspace.paths().runtime_dir().to_path_buf(),
        )
    }

    fn from_parts(
        workspace_id: impl Into<String>,
        state_dir: PathBuf,
        runtime_dir: PathBuf,
    ) -> Result<Self> {
        let workspace_id = workspace_id.into();
        if !is_valid_workspace_id(&workspace_id) {
            bail!("Invalid workspace ID for container CLI query context");
        }
        let forward_status_dir = forward_status_dir(&runtime_dir);
        let mut context = Self {
            workspace_id,
            state_dir,
            runtime_dir,
            forward_status_dir,
            context_fingerprint: String::new(),
        };
        context.context_fingerprint = context_fingerprint(
            &context.workspace_id,
            &context.state_dir,
            &context.runtime_dir,
            &context.forward_status_dir,
        );
        Ok(context)
    }

    pub(crate) fn context_fingerprint(&self) -> &str {
        &self.context_fingerprint
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HostDaemonCliQueryPolicy {
    Disabled,
    Enabled(ContainerCliQueryContext),
}

impl HostDaemonCliQueryPolicy {
    pub(crate) fn for_workspace(enabled: bool, workspace: &Workspace) -> Result<Self> {
        if enabled {
            Ok(Self::Enabled(ContainerCliQueryContext::for_workspace(
                workspace,
            )?))
        } else {
            Ok(Self::Disabled)
        }
    }

    pub(crate) fn identity(&self) -> HostDaemonCliQueryIdentity {
        match self {
            Self::Disabled => HostDaemonCliQueryIdentity::Disabled,
            Self::Enabled(context) => HostDaemonCliQueryIdentity::Enabled {
                context_fingerprint: context.context_fingerprint().to_owned(),
            },
        }
    }

    #[cfg(test)]
    pub(crate) fn enabled_for_test(
        workspace_id: &str,
        state_dir: PathBuf,
        runtime_dir: PathBuf,
    ) -> Self {
        Self::Enabled(
            ContainerCliQueryContext::from_parts(workspace_id, state_dir, runtime_dir).unwrap(),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "policy", rename_all = "kebab-case")]
pub(crate) enum HostDaemonCliQueryIdentity {
    Disabled,
    Enabled { context_fingerprint: String },
}

impl HostDaemonCliQueryIdentity {
    pub(crate) fn can_reuse(&self, requested: &Self) -> bool {
        self == requested
    }
}

fn context_fingerprint(
    workspace_id: &str,
    state_dir: &std::path::Path,
    runtime_dir: &std::path::Path,
    forward_status_dir: &std::path::Path,
) -> String {
    let mut input = Vec::new();
    append_fingerprint_field(&mut input, b"domain", QUERY_CONTEXT_FINGERPRINT_DOMAIN);
    append_fingerprint_field(&mut input, b"workspace_id", workspace_id.as_bytes());
    append_fingerprint_field(&mut input, b"state_dir", state_dir.as_os_str().as_bytes());
    append_fingerprint_field(
        &mut input,
        b"runtime_dir",
        runtime_dir.as_os_str().as_bytes(),
    );
    append_fingerprint_field(
        &mut input,
        b"forward_status_dir",
        forward_status_dir.as_os_str().as_bytes(),
    );
    sha256_hex(&input)
}

fn append_fingerprint_field(input: &mut Vec<u8>, name: &[u8], value: &[u8]) {
    input.extend_from_slice(name.len().to_string().as_bytes());
    input.push(b':');
    input.extend_from_slice(name);
    input.extend_from_slice(value.len().to_string().as_bytes());
    input.push(b':');
    input.extend_from_slice(value);
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{
        ContainerCliQueryContext, HostDaemonCliQueryIdentity, HostDaemonCliQueryPolicy,
        context_fingerprint,
    };

    const WORKSPACE_ID: &str = "012345abcdef";

    fn context(state_dir: &str, runtime_dir: &str) -> ContainerCliQueryContext {
        ContainerCliQueryContext::from_parts(
            WORKSPACE_ID,
            PathBuf::from(state_dir),
            PathBuf::from(runtime_dir),
        )
        .unwrap()
    }

    #[test]
    fn query_context_keeps_only_fixed_workspace_paths() {
        let context = context("/state/decune/012345abcdef", "/run/decune/012345abcdef");

        assert_eq!(context.workspace_id, WORKSPACE_ID);
        assert_eq!(
            context.state_dir,
            std::path::Path::new("/state/decune/012345abcdef")
        );
        assert_eq!(
            context.runtime_dir,
            std::path::Path::new("/run/decune/012345abcdef")
        );
        assert_eq!(
            context.forward_status_dir,
            std::path::Path::new("/run/decune/012345abcdef-ports")
        );
        assert_eq!(context.context_fingerprint().len(), 64);
        assert!(
            context
                .context_fingerprint()
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        );
    }

    #[test]
    fn query_context_rejects_invalid_workspace_id() {
        let error = ContainerCliQueryContext::from_parts(
            "../workspace",
            PathBuf::from("/state"),
            PathBuf::from("/runtime"),
        )
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            "Invalid workspace ID for container CLI query context"
        );
    }

    #[test]
    fn context_fingerprint_is_stable_and_covers_all_fixed_fields() {
        let baseline = context_fingerprint(
            WORKSPACE_ID,
            std::path::Path::new("/state/a"),
            std::path::Path::new("/runtime/a"),
            std::path::Path::new("/runtime/a-ports"),
        );

        assert_eq!(
            baseline,
            context_fingerprint(
                WORKSPACE_ID,
                std::path::Path::new("/state/a"),
                std::path::Path::new("/runtime/a"),
                std::path::Path::new("/runtime/a-ports"),
            )
        );
        assert_ne!(
            baseline,
            context_fingerprint(
                "fedcba543210",
                std::path::Path::new("/state/a"),
                std::path::Path::new("/runtime/a"),
                std::path::Path::new("/runtime/a-ports"),
            )
        );
        assert_ne!(
            baseline,
            context_fingerprint(
                WORKSPACE_ID,
                std::path::Path::new("/state/b"),
                std::path::Path::new("/runtime/a"),
                std::path::Path::new("/runtime/a-ports"),
            )
        );
        assert_ne!(
            baseline,
            context_fingerprint(
                WORKSPACE_ID,
                std::path::Path::new("/state/a"),
                std::path::Path::new("/runtime/b"),
                std::path::Path::new("/runtime/a-ports"),
            )
        );
        assert_ne!(
            baseline,
            context_fingerprint(
                WORKSPACE_ID,
                std::path::Path::new("/state/a"),
                std::path::Path::new("/runtime/a"),
                std::path::Path::new("/runtime/b-ports"),
            )
        );
    }

    #[test]
    fn query_identity_reuse_matrix_requires_equal_policy_and_fingerprint() {
        let enabled_a = HostDaemonCliQueryPolicy::Enabled(context("/state/a", "/runtime/a"));
        let enabled_a_copy = HostDaemonCliQueryPolicy::Enabled(context("/state/a", "/runtime/a"));
        let enabled_b = HostDaemonCliQueryPolicy::Enabled(context("/state/b", "/runtime/a"));
        let disabled = HostDaemonCliQueryPolicy::Disabled;

        let rows = [
            (disabled.identity(), disabled.identity(), true),
            (enabled_a.identity(), enabled_a_copy.identity(), true),
            (disabled.identity(), enabled_a.identity(), false),
            (enabled_a.identity(), disabled.identity(), false),
            (enabled_a.identity(), enabled_b.identity(), false),
        ];

        for (existing, requested, expected) in rows {
            assert_eq!(existing.can_reuse(&requested), expected);
        }
    }

    #[test]
    fn enabled_identity_serializes_only_the_fingerprint() {
        let policy = HostDaemonCliQueryPolicy::Enabled(context(
            "/state/SECRET-workspace",
            "/runtime/SECRET-workspace",
        ));

        let serialized = serde_json::to_string(&policy.identity()).unwrap();

        assert!(serialized.contains("context_fingerprint"));
        assert!(!serialized.contains("/state"));
        assert!(!serialized.contains("/runtime"));
        assert!(!serialized.contains("SECRET"));
    }

    #[test]
    fn disabled_identity_has_no_context_fingerprint() {
        let serialized = serde_json::to_value(HostDaemonCliQueryIdentity::Disabled).unwrap();

        assert_eq!(serialized, serde_json::json!({"policy": "disabled"}));
    }
}
