mod archive;
mod auth;
mod cache;
mod install;
mod local;
mod lock;
mod metadata;
mod options;
mod reference;
mod registry;
pub(crate) use cache::pull_oci_feature_with_client;
#[cfg(test)]
pub(crate) use install::FeatureInstallInput;
pub(crate) use install::{PreparedFeatureInstallPlan, prepare_feature_install_plan};
pub(crate) use lock::{
    FEATURE_LOCK_VERSION, FeatureLockEntry, FeatureLockFile, read_feature_lock_file,
    remove_feature_lock_file, resolve_locked_feature_ref, write_feature_lock_file,
};
pub(crate) use metadata::{FeatureMetadata, FeatureOptionSchema, read_feature_metadata_document};
pub(crate) use options::feature_option_env;
pub(crate) use reference::{
    FeatureRef, LocalFeatureRef, OciFeatureRef, parse_feature_ref,
    parse_feature_ref_from_devcontainer_dir,
};
pub(crate) use registry::HttpOciRegistryClient;

fn hex_lower(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}
