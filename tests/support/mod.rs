use std::{
    fs, io,
    os::unix::fs::PermissionsExt,
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
        if path.file_name().is_some_and(|name| name == "install.sh") {
            let mut permissions = fs::metadata(&path)?.permissions();
            permissions.set_mode(permissions.mode() | 0o111);
            fs::set_permissions(&path, permissions)?;
        }
        Ok(path)
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
