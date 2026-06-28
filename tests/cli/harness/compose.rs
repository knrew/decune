use super::TestUnwrap as _;

pub(crate) fn fake_compose_capabilities_script_path() -> std::path::PathBuf {
    crate::support::fixture_path("cli/harness/compose-capabilities.sh").must()
}
