use std::{
    fs, io,
    path::{Component, Path, PathBuf},
};

use tempfile::TempDir;

#[derive(Debug)]
pub struct TempWorkspace {
    directory: TempDir,
}

impl TempWorkspace {
    pub fn new() -> io::Result<Self> {
        tempfile::Builder::new()
            .prefix("decune-test-")
            .tempdir()
            .map(|directory| Self { directory })
    }

    pub fn path(&self) -> &Path {
        self.directory.path()
    }

    pub fn create_dir(&self, relative_path: impl AsRef<Path>) -> io::Result<PathBuf> {
        let path = self.resolve(relative_path)?;
        fs::create_dir_all(&path)?;
        Ok(path)
    }

    pub fn write_file(
        &self,
        relative_path: impl AsRef<Path>,
        contents: impl AsRef<[u8]>,
    ) -> io::Result<PathBuf> {
        let path = self.resolve(relative_path)?;

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        fs::write(&path, contents)?;
        Ok(path)
    }

    #[allow(dead_code)]
    pub fn copy_dir_from(&self, source: impl AsRef<Path>) -> io::Result<()> {
        copy_dir_contents(source.as_ref(), self.path())
    }

    fn resolve(&self, relative_path: impl AsRef<Path>) -> io::Result<PathBuf> {
        let relative_path = relative_path.as_ref();

        if relative_path.is_absolute()
            || relative_path
                .components()
                .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "fixture paths must stay inside the temporary workspace",
            ));
        }

        Ok(self.path().join(relative_path))
    }
}

#[allow(dead_code)]
fn copy_dir_contents(source: &Path, destination: &Path) -> io::Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let file_type = entry.file_type()?;

        if file_type.is_dir() {
            fs::create_dir_all(&destination_path)?;
            copy_dir_contents(&source_path, &destination_path)?;
        } else if file_type.is_file() {
            if let Some(parent) = destination_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&source_path, &destination_path)?;
        } else if file_type.is_symlink() {
            let target = fs::read_link(&source_path)?;
            std::os::unix::fs::symlink(target, &destination_path)?;
        }
    }

    Ok(())
}
