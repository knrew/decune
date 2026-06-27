use std::{fs, path::Path};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

pub(crate) fn sha256_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path)
        .with_context(|| format!("Failed to read file for sha256: {}", path.display()))?;
    Ok(hex_lower(&Sha256::digest(&bytes)))
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
