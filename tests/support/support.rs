use std::{
    ffi::OsString,
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
        Ok(path)
    }

    pub fn copy_fixture_dir(&self, relative_path: impl AsRef<Path>) -> io::Result<PathBuf> {
        self.copy_fixture_dir_to(relative_path, "")
    }

    pub fn copy_fixture_dir_to(
        &self,
        fixture: impl AsRef<Path>,
        destination: impl AsRef<Path>,
    ) -> io::Result<PathBuf> {
        let fixture = fixture_path(fixture)?;
        let destination = self.resolve(destination)?;
        copy_dir_contents(&fixture, &destination)?;
        Ok(destination)
    }

    pub fn write_fixture_file(
        &self,
        destination: impl AsRef<Path>,
        fixture: impl AsRef<Path>,
    ) -> io::Result<PathBuf> {
        self.write_file(destination, read_fixture(fixture)?)
    }

    pub fn write_fixture_template(
        &self,
        destination: impl AsRef<Path>,
        fixture: impl AsRef<Path>,
        replacements: &[(&str, &str)],
    ) -> io::Result<PathBuf> {
        self.write_file(destination, render_fixture_template(fixture, replacements)?)
    }

    pub fn write_executable(
        &self,
        destination: impl AsRef<Path>,
        contents: impl AsRef<[u8]>,
    ) -> io::Result<PathBuf> {
        let path = self.write_file(destination, contents)?;
        make_executable(&path)?;
        Ok(path)
    }

    pub fn write_executable_fixture(
        &self,
        destination: impl AsRef<Path>,
        fixture: impl AsRef<Path>,
    ) -> io::Result<PathBuf> {
        let path = self.write_fixture_file(destination, fixture)?;
        make_executable(&path)?;
        Ok(path)
    }

    fn resolve(&self, relative_path: impl AsRef<Path>) -> io::Result<PathBuf> {
        let relative_path = relative_path.as_ref();

        validate_relative_path(
            relative_path,
            "fixture paths must stay inside the temporary workspace",
        )?;

        Ok(self.path().join(relative_path))
    }
}

pub fn repo_file(relative_path: impl AsRef<Path>) -> io::Result<PathBuf> {
    let relative_path = relative_path.as_ref();
    validate_relative_path(
        relative_path,
        "repository paths must stay inside the repository",
    )?;
    Ok(Path::new(env!("CARGO_MANIFEST_DIR")).join(relative_path))
}

pub fn fixture_path(relative_path: impl AsRef<Path>) -> io::Result<PathBuf> {
    let relative_path = relative_path.as_ref();
    validate_relative_path(
        relative_path,
        "fixture paths must stay inside tests/fixtures",
    )?;
    repo_file(Path::new("tests/fixtures").join(relative_path))
}

pub fn read_fixture(relative_path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    fs::read(fixture_path(relative_path)?)
}

pub fn read_fixture_string(relative_path: impl AsRef<Path>) -> io::Result<String> {
    fs::read_to_string(fixture_path(relative_path)?)
}

pub fn render_fixture_template(
    relative_path: impl AsRef<Path>,
    replacements: &[(&str, &str)],
) -> io::Result<String> {
    let mut contents = read_fixture_string(relative_path)?;

    for (placeholder, value) in replacements {
        if !contents.contains(placeholder) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("fixture template does not contain placeholder `{placeholder}`"),
            ));
        }
        contents = contents.replace(placeholder, value);
    }

    Ok(contents)
}

pub fn copy_dir_contents(
    source: impl AsRef<Path>,
    destination: impl AsRef<Path>,
) -> io::Result<()> {
    let source = source.as_ref();
    let destination = destination.as_ref();
    fs::create_dir_all(destination)?;

    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());

        if file_type.is_dir() {
            copy_dir_contents(&source_path, &destination_path)?;
        } else if file_type.is_symlink() {
            let target = fs::read_link(&source_path)?;
            std::os::unix::fs::symlink(target, &destination_path)?;
        } else if fs::metadata(&source_path)?.is_file() {
            if let Some(parent) = destination_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&source_path, &destination_path)?;
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unsupported fixture file type: {}", source_path.display()),
            ));
        }
    }

    Ok(())
}

pub fn path_with_prepended(directory: impl AsRef<Path>) -> io::Result<OsString> {
    let paths = std::iter::once(directory.as_ref().to_path_buf()).chain(
        std::env::var_os("PATH")
            .into_iter()
            .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>()),
    );
    std::env::join_paths(paths).map_err(io::Error::other)
}

fn validate_relative_path(path: &Path, message: &'static str) -> io::Result<()> {
    if path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, message));
    }

    Ok(())
}

fn make_executable(path: &Path) -> io::Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)
}
