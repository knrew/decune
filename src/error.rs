use std::{fmt::Display, path::Path};

use anyhow::{Context, Result};

pub(crate) trait ResultExt<T> {
    fn with_path_context<P>(self, action: &'static str, path: P) -> Result<T>
    where
        P: AsRef<Path>;

    fn with_resource_context<R>(self, action: &'static str, resource: R) -> Result<T>
    where
        R: Display;
}

impl<T, E> ResultExt<T> for std::result::Result<T, E>
where
    E: std::error::Error + Send + Sync + 'static,
{
    fn with_path_context<P>(self, action: &'static str, path: P) -> Result<T>
    where
        P: AsRef<Path>,
    {
        self.with_context(|| format!("Failed to {action}: {}", path.as_ref().display()))
    }

    fn with_resource_context<R>(self, action: &'static str, resource: R) -> Result<T>
    where
        R: Display,
    {
        self.with_context(|| format!("Failed to {action}: {resource}"))
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, io};

    use super::ResultExt;

    #[test]
    fn path_context_includes_action_and_path() {
        let path = "/tmp/decune-missing-file";
        let error = fs::read_to_string(path)
            .with_path_context("read config file", path)
            .unwrap_err();

        let message = format!("{error:#}");
        assert!(message.contains("Failed to read config file: /tmp/decune-missing-file"));
        assert!(message.contains("No such file or directory"));
    }

    #[test]
    fn resource_context_includes_action_and_resource() {
        let result: io::Result<()> = Err(io::Error::other("connection refused"));
        let error = result
            .with_resource_context("connect to Docker daemon", "unix:///var/run/docker.sock")
            .unwrap_err();

        let message = format!("{error:#}");
        assert!(
            message.contains("Failed to connect to Docker daemon: unix:///var/run/docker.sock")
        );
        assert!(message.contains("connection refused"));
    }
}
