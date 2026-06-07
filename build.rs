use std::{
    collections::BTreeSet,
    env, fs,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const REQUIRED_PROTOCOL_VERSION: u32 = 1;
const REQUIRED_SCHEMA_VERSION: u32 = 1;
const TOOLS: [&str; 2] = ["git-credential-decune", "decune-forward-agent"];
const PLATFORMS: [&str; 2] = ["linux-amd64", "linux-arm64"];

fn main() -> Result<()> {
    println!("cargo:rerun-if-env-changed=DECUNE_CONTAINER_TOOLS_BUNDLE");
    println!("cargo:rerun-if-env-changed=DECUNE_CONTAINER_TOOLS_BUNDLE_DIR");
    println!("cargo:rerun-if-changed=assets/container-tools/manifest.json");
    for platform in PLATFORMS {
        for tool in TOOLS {
            println!("cargo:rerun-if-changed=assets/container-tools/{platform}/{tool}");
        }
    }

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").context("OUT_DIR is not set")?);
    let generated_path = out_dir.join("container_tools_bundle.rs");
    let mode = env::var("DECUNE_CONTAINER_TOOLS_BUNDLE").unwrap_or_else(|_| "auto".to_owned());
    let bundle_dir = resolve_bundle_dir()?;

    match mode.as_str() {
        "off" => write_empty_bundle(&generated_path),
        "auto" if !bundle_dir.join("manifest.json").is_file() => {
            write_empty_bundle(&generated_path)
        }
        "auto" | "required" => {
            let entries = validate_bundle(&bundle_dir).with_context(|| {
                format!(
                    "Invalid decune container tools bundle: {}",
                    bundle_dir.display()
                )
            })?;
            write_bundle(&generated_path, &entries)
        }
        other => bail!(
            "Unsupported DECUNE_CONTAINER_TOOLS_BUNDLE value: {other}. Expected auto, required, or off."
        ),
    }
}

fn resolve_bundle_dir() -> Result<PathBuf> {
    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").context("CARGO_MANIFEST_DIR is not set")?);
    let bundle_dir = env::var_os("DECUNE_CONTAINER_TOOLS_BUNDLE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest_dir.join("assets/container-tools"));
    if bundle_dir.is_absolute() {
        Ok(bundle_dir)
    } else {
        Ok(manifest_dir.join(bundle_dir))
    }
}

fn write_empty_bundle(path: &Path) -> Result<()> {
    fs::write(
        path,
        "pub(crate) static EMBEDDED_CONTAINER_TOOLS: &[EmbeddedContainerToolArtifact] = &[];\n",
    )
    .with_context(|| {
        format!(
            "Failed to write generated container tools bundle: {}",
            path.display()
        )
    })
}

fn write_bundle(path: &Path, entries: &[ValidatedEntry]) -> Result<()> {
    let mut code = String::from(
        "pub(crate) static EMBEDDED_CONTAINER_TOOLS: &[EmbeddedContainerToolArtifact] = &[\n",
    );
    for entry in entries {
        code.push_str("    EmbeddedContainerToolArtifact {\n");
        code.push_str(&format!("        name: {:?},\n", entry.name));
        code.push_str(&format!("        platform: {:?},\n", entry.platform));
        code.push_str(&format!("        sha256: {:?},\n", entry.sha256));
        code.push_str(&format!(
            "        bytes: include_bytes!({:?}),\n",
            entry.absolute_path
        ));
        code.push_str("    },\n");
    }
    code.push_str("];\n");
    fs::write(path, code).with_context(|| {
        format!(
            "Failed to write generated container tools bundle: {}",
            path.display()
        )
    })
}

fn validate_bundle(bundle_dir: &Path) -> Result<Vec<ValidatedEntry>> {
    let bundle_dir = fs::canonicalize(bundle_dir).with_context(|| {
        format!(
            "Failed to canonicalize container tools bundle dir: {}",
            bundle_dir.display()
        )
    })?;
    let manifest_path = bundle_dir.join("manifest.json");
    let manifest_bytes = fs::read(&manifest_path).with_context(|| {
        format!(
            "Failed to read container tools manifest: {}",
            manifest_path.display()
        )
    })?;
    let manifest: Manifest = serde_json::from_slice(&manifest_bytes).with_context(|| {
        format!(
            "Failed to parse container tools manifest: {}",
            manifest_path.display()
        )
    })?;
    if manifest.schema_version != REQUIRED_SCHEMA_VERSION {
        bail!(
            "Unsupported container tools manifest schemaVersion: {}",
            manifest.schema_version
        );
    }
    if manifest.protocol_version != REQUIRED_PROTOCOL_VERSION {
        bail!(
            "Unsupported container tools protocolVersion: {}",
            manifest.protocol_version
        );
    }

    let expected = expected_container_tool_set();
    let mut seen = BTreeSet::new();
    let mut entries = Vec::new();
    for entry in manifest.tools {
        let key = (entry.name.clone(), entry.platform.clone());
        if !expected.contains(&key) {
            bail!(
                "Unexpected container tool artifact in manifest: {} for {}",
                entry.name,
                entry.platform
            );
        }
        if !seen.insert(key) {
            bail!(
                "Duplicate container tool artifact in manifest: {} for {}",
                entry.name,
                entry.platform
            );
        }
        validate_manifest_path(&entry.path)?;
        validate_sha256_string(&entry.sha256)?;
        let absolute_path = bundle_dir.join(&entry.path);
        if !absolute_path.is_file() {
            bail!(
                "Container tools manifest entry does not exist: {}",
                absolute_path.display()
            );
        }
        validate_executable(&absolute_path)?;
        let bytes = fs::read(&absolute_path).with_context(|| {
            format!(
                "Failed to read container tool artifact for checksum: {}",
                absolute_path.display()
            )
        })?;
        let actual = hex_lower(&Sha256::digest(&bytes));
        if actual != entry.sha256 {
            bail!(
                "Container tool artifact checksum mismatch: {}",
                absolute_path.display()
            );
        }
        entries.push(ValidatedEntry {
            name: entry.name,
            platform: entry.platform,
            sha256: entry.sha256,
            absolute_path,
        });
    }

    if seen != expected {
        let missing = expected.difference(&seen).cloned().collect::<Vec<_>>();
        bail!("Missing required container tool artifacts: {missing:?}");
    }

    entries.sort_by(|left, right| {
        left.platform
            .cmp(&right.platform)
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(entries)
}

fn expected_container_tool_set() -> BTreeSet<(String, String)> {
    PLATFORMS
        .iter()
        .flat_map(|platform| {
            TOOLS
                .iter()
                .map(move |tool| ((*tool).to_owned(), (*platform).to_owned()))
        })
        .collect()
}

fn validate_manifest_path(path: &Path) -> Result<()> {
    if path.is_absolute() {
        bail!(
            "Container tools manifest path must be relative: {}",
            path.display()
        );
    }
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            _ => bail!(
                "Container tools manifest path must not escape the bundle: {}",
                path.display()
            ),
        }
    }
    Ok(())
}

fn validate_sha256_string(value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .chars()
            .all(|ch| ch.is_ascii_hexdigit() && !ch.is_ascii_uppercase())
    {
        bail!("Invalid sha256 value in container tools manifest: {value}");
    }
    Ok(())
}

#[cfg(unix)]
fn validate_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mode = fs::metadata(path)
        .with_context(|| format!("Failed to stat container tool artifact: {}", path.display()))?
        .permissions()
        .mode()
        & 0o777;
    if mode & 0o111 == 0 {
        bail!(
            "Container tool artifact is not executable: {}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_executable(_path: &Path) -> Result<()> {
    Ok(())
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    schema_version: u32,
    protocol_version: u32,
    tools: Vec<ManifestEntry>,
}

#[derive(Debug, Deserialize)]
struct ManifestEntry {
    name: String,
    platform: String,
    path: PathBuf,
    sha256: String,
}

#[derive(Debug)]
struct ValidatedEntry {
    name: String,
    platform: String,
    sha256: String,
    absolute_path: PathBuf,
}
