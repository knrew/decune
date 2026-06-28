use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use tar::{Archive, EntryType};

pub(crate) fn extract_feature_archive(archive_path: &Path, destination: &Path) -> Result<()> {
    if destination.exists() {
        fs::remove_dir_all(destination).with_context(|| {
            format!(
                "Failed to remove existing feature extraction directory: {}",
                destination.display()
            )
        })?;
    }
    fs::create_dir_all(destination).with_context(|| {
        format!(
            "Failed to create feature extraction directory: {}",
            destination.display()
        )
    })?;

    let mut file = fs::File::open(archive_path)
        .with_context(|| format!("Failed to open feature archive: {}", archive_path.display()))?;
    let mut magic = [0; 2];
    let read = file
        .read(&mut magic)
        .with_context(|| format!("Failed to read feature archive: {}", archive_path.display()))?;
    file.seek(SeekFrom::Start(0)).with_context(|| {
        format!(
            "Failed to rewind feature archive: {}",
            archive_path.display()
        )
    })?;

    if read == magic.len() && magic == [0x1f, 0x8b] {
        extract_tar_archive(archive_path, destination, GzDecoder::new(file))
    } else {
        extract_tar_archive(archive_path, destination, file)
    }
}

pub(super) fn find_required_feature_file(root: &Path, name: &str) -> Result<PathBuf> {
    let path = root.join(name);
    if !path.is_file() {
        bail!("Feature archive must contain {name} at its root");
    }

    Ok(path)
}

fn extract_tar_archive<R: Read>(archive_path: &Path, destination: &Path, reader: R) -> Result<()> {
    let mut archive = Archive::new(reader);

    for entry in archive.entries().with_context(|| {
        format!(
            "Failed to read feature archive entries: {}",
            archive_path.display()
        )
    })? {
        let mut entry = entry.with_context(|| {
            format!(
                "Failed to read feature archive entry: {}",
                archive_path.display()
            )
        })?;
        let path = entry.path().with_context(|| {
            format!(
                "Failed to read feature archive entry path: {}",
                archive_path.display()
            )
        })?;
        let path = path.into_owned();
        validate_archive_entry_path(&path)?;
        validate_archive_entry_type(entry.header().entry_type(), &path)?;
        entry.unpack_in(destination).with_context(|| {
            format!(
                "Failed to extract feature archive entry {} from {}",
                path.display(),
                archive_path.display()
            )
        })?;
    }

    Ok(())
}

fn validate_archive_entry_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        bail!("Unsafe feature archive path: empty path");
    }
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("Unsafe feature archive path: {}", path.display());
            }
        }
    }

    Ok(())
}

fn validate_archive_entry_type(entry_type: EntryType, path: &Path) -> Result<()> {
    if entry_type.is_file() || entry_type.is_dir() {
        return Ok(());
    }

    bail!(
        "Unsupported feature archive entry type for {}",
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Write};

    use flate2::{Compression, write::GzEncoder};

    use super::*;

    #[test]
    fn feature_archive_rejects_path_traversal_entries() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("feature.tgz");
        write_malicious_feature_archive(&archive, "../escape", b"owned");

        let error = extract_feature_archive(&archive, &temp.path().join("out")).unwrap_err();

        assert!(error.to_string().contains("Unsafe feature archive path"));
        assert!(!temp.path().join("escape").exists());
    }

    fn write_malicious_feature_archive(path: &Path, entry_path: &str, content: &[u8]) {
        let file = fs::File::create(path).unwrap();
        let mut encoder = GzEncoder::new(file, Compression::default());
        let mut header = [0u8; 512];
        header[..entry_path.len()].copy_from_slice(entry_path.as_bytes());
        write_octal(&mut header[100..108], 0o755);
        write_octal(&mut header[108..116], 0);
        write_octal(&mut header[116..124], 0);
        write_octal(&mut header[124..136], content.len() as u64);
        write_octal(&mut header[136..148], 0);
        for byte in &mut header[148..156] {
            *byte = b' ';
        }
        header[156] = b'0';
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        let checksum = header.iter().map(|byte| u32::from(*byte)).sum::<u32>() as u64;
        write_checksum(&mut header[148..156], checksum);

        encoder.write_all(&header).unwrap();
        encoder.write_all(content).unwrap();
        let padding = (512 - (content.len() % 512)) % 512;
        encoder.write_all(&vec![0; padding]).unwrap();
        encoder.write_all(&[0; 1024]).unwrap();
        let mut file = encoder.finish().unwrap();
        file.flush().unwrap();
    }

    fn write_octal(field: &mut [u8], value: u64) {
        let value = format!("{value:0width$o}\0", width = field.len() - 1);
        field.copy_from_slice(value.as_bytes());
    }

    fn write_checksum(field: &mut [u8], value: u64) {
        let value = format!("{value:06o}\0 ");
        field.copy_from_slice(value.as_bytes());
    }
}
