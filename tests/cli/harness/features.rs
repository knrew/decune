use flate2::{Compression, write::GzEncoder};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};
use tar::{Builder, Header};

use super::names::hex_lower;

pub(crate) fn write_fake_github_cli_feature_cache(
    workspace_root: &Path,
    cache_home: &Path,
    manifest_digest: &str,
    install_script: &str,
) {
    fs::create_dir_all(workspace_root.join(".decune")).unwrap();
    fs::write(
        workspace_root.join(".decune/features.lock.toml"),
        format!(
            r#"
version = 1

[[features]]
id = "ghcr.io/devcontainers/features/github-cli"
ref = "ghcr.io/devcontainers/features/github-cli:1"
digest = "{manifest_digest}"
"#
        ),
    )
    .unwrap();

    let cache_root = cache_home.join("decune/features");
    fs::create_dir_all(&cache_root).unwrap();
    let archive = cache_root.join(format!("{}.tgz", manifest_digest.replace(':', "_")));
    let metadata = r#"{"id":"github-cli","version":"1.0.0","name":"GitHub CLI"}"#;
    write_feature_archive(
        &archive,
        &[
            ("install.sh", install_script.as_bytes()),
            ("devcontainer-feature.json", metadata.as_bytes()),
        ],
    );
    let blob = fs::read(&archive).unwrap();
    let layer_digest = format!("sha256:{}", hex_lower(&Sha256::digest(&blob)));
    fs::write(
        archive.with_extension("tgz.toml"),
        format!("manifest_digest = \"{manifest_digest}\"\nlayer_digest = \"{layer_digest}\"\n"),
    )
    .unwrap();
}

fn write_feature_archive(path: &PathBuf, entries: &[(&str, &[u8])]) {
    let file = fs::File::create(path).unwrap();
    let encoder = GzEncoder::new(file, Compression::default());
    let mut builder = Builder::new(encoder);
    for (path, content) in entries {
        let mut header = Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, *path, &mut &content[..])
            .unwrap();
    }
    let encoder = builder.into_inner().unwrap();
    let mut file = encoder.finish().unwrap();
    file.flush().unwrap();
}
