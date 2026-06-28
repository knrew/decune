use sha2::{Digest, Sha256};
use std::path::Path;

pub(crate) fn workspace_id(root: &Path) -> String {
    let digest = Sha256::digest(root.to_string_lossy().as_bytes());
    let mut id = String::with_capacity(12);

    for byte in digest.iter().take(6) {
        push_hex_byte(&mut id, *byte);
    }

    id
}

pub(crate) fn workspace_image_repository(root: &Path) -> String {
    let basename = root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("workspace");

    format!(
        "decune/{}-{}",
        safe_workspace_slug(basename),
        workspace_id(root)
    )
}

pub(crate) fn safe_workspace_slug(value: &str) -> String {
    let mut output = String::new();
    let mut previous_was_hyphen = false;

    for character in value.chars() {
        let character = character.to_ascii_lowercase();

        if character.is_ascii_alphanumeric() {
            output.push(character);
            previous_was_hyphen = false;
        } else if !output.is_empty() && !previous_was_hyphen {
            output.push('-');
            previous_was_hyphen = true;
        }
    }

    trim_slug_separators(&mut output);
    output.truncate(48);
    trim_slug_separators(&mut output);

    if output.is_empty() {
        "workspace".to_owned()
    } else {
        output
    }
}

fn trim_slug_separators(output: &mut String) {
    while output.ends_with('-') {
        output.pop();
    }

    let trim_start = output.bytes().take_while(|byte| *byte == b'-').count();
    if trim_start > 0 {
        output.drain(..trim_start);
    }
}

fn push_hex_byte(output: &mut String, byte: u8) {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    output.push(HEX[(byte >> 4) as usize] as char);
    output.push(HEX[(byte & 0x0f) as usize] as char);
}

pub(super) fn hex_lower(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        push_hex_byte(&mut output, *byte);
    }
    output
}

pub(crate) fn current_uid() -> u32 {
    // SAFETY: getuid has no preconditions, takes no pointers, and cannot fail.
    unsafe { libc::getuid() }
}

pub(crate) fn current_gid() -> u32 {
    // SAFETY: getgid has no preconditions, takes no pointers, and cannot fail.
    unsafe { libc::getgid() }
}
