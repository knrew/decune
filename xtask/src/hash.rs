use std::{fs, path::Path};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

pub(crate) fn sha256_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path)
        .with_context(|| format!("Failed to read file for sha256: {}", path.display()))?;
    Ok(hex_lower(&Sha256::digest(&bytes)))
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        push_hex_byte(&mut hex, byte);
    }
    hex
}

fn push_hex_byte(output: &mut String, byte: u8) {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    output.push(HEX[(byte >> 4) as usize] as char);
    output.push(HEX[(byte & 0x0f) as usize] as char);
}
