use std::{
    collections::BTreeMap,
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Child, Command as HostCommand, Stdio},
    thread,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use serde::Deserialize;

const DOCKER_HUB_CANONICAL_HOST: &str = "docker.io";
const DOCKER_HUB_REGISTRY_HOST: &str = "registry-1.docker.io";
const DOCKER_HUB_INDEX_HOST: &str = "index.docker.io";
const DOCKER_HUB_INDEX_AUTH_KEY: &str = "index.docker.io/v1";
const DOCKER_HUB_CREDENTIAL_HELPER_SERVER: &str = "https://index.docker.io/v1/";
const DOCKER_CREDENTIAL_HELPER_SPAWN_RETRIES: usize = 5;
const DOCKER_CREDENTIAL_HELPER_SPAWN_RETRY_DELAY: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RegistryAuth {
    Basic { username: String, password: String },
    Bearer(String),
}

#[derive(Debug, Clone, Default)]
pub(super) struct DockerConfigAuthStore {
    entries: BTreeMap<String, RegistryAuth>,
    cred_helpers: BTreeMap<String, String>,
    creds_store: Option<String>,
    helper_paths: Vec<PathBuf>,
}

impl DockerConfigAuthStore {
    pub(super) fn from_default_config() -> Result<Self> {
        let path = match env::var_os("DOCKER_CONFIG").map(PathBuf::from) {
            Some(config_dir) => config_dir.join("config.json"),
            None => {
                let Some(home) = env::var_os("HOME").map(PathBuf::from) else {
                    return Ok(Self::default());
                };
                home.join(".docker").join("config.json")
            }
        };
        if !path.exists() {
            return Ok(Self::default());
        }

        Self::from_config_file(&path)
    }

    pub(super) fn from_config_file(path: &Path) -> Result<Self> {
        let helper_paths = env::var_os("PATH")
            .map(|paths| env::split_paths(&paths).collect::<Vec<_>>())
            .unwrap_or_default();
        Self::from_config_file_with_helper_paths(path, &helper_paths)
    }

    pub(super) fn from_config_file_with_helper_paths(
        path: &Path,
        helper_paths: &[PathBuf],
    ) -> Result<Self> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read Docker config: {}", path.display()))?;
        let config: DockerConfigFile = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse Docker config: {}", path.display()))?;
        let mut entries = BTreeMap::new();
        for (registry, entry) in &config.auths {
            if let Some(auth) = entry.to_registry_auth().with_context(|| {
                format!(
                    "Failed to parse Docker registry auth for {registry} in {}",
                    path.display()
                )
            })? {
                entries.insert(normalize_registry_auth_key(registry), auth);
            }
        }

        Ok(Self {
            entries,
            cred_helpers: config
                .cred_helpers
                .into_iter()
                .map(|(registry, helper)| (normalize_registry_auth_key(&registry), helper))
                .collect(),
            creds_store: config.creds_store,
            helper_paths: helper_paths.to_vec(),
        })
    }

    pub(super) fn get(&self, registry: &str) -> Result<Option<RegistryAuth>> {
        let registry = normalize_registry_auth_key(registry);
        let lookup_keys = registry_auth_lookup_keys(&registry);
        let helper_server = registry_auth_helper_server(&registry);
        if let Some(helper) = self.credential_helper_for_registry(&lookup_keys) {
            return docker_config_helper_auth(helper, &helper_server, &self.helper_paths)
                .with_context(|| {
                    format!("Failed to read Docker registry credential helper auth for {registry}")
                });
        }
        if let Some(helper) = self.creds_store.as_deref() {
            return docker_config_helper_auth(helper, &helper_server, &self.helper_paths)
                .with_context(|| {
                    format!("Failed to read Docker registry credential helper auth for {registry}")
                });
        }

        for key in lookup_keys {
            if let Some(auth) = self.entries.get(&key) {
                return Ok(Some(auth.clone()));
            }
        }

        Ok(None)
    }

    fn credential_helper_for_registry<'a>(&'a self, lookup_keys: &[String]) -> Option<&'a str> {
        lookup_keys
            .iter()
            .find_map(|registry| self.cred_helpers.get(registry).map(String::as_str))
    }
}

fn normalize_registry_auth_key(registry: &str) -> String {
    registry
        .strip_prefix("https://")
        .or_else(|| registry.strip_prefix("http://"))
        .unwrap_or(registry)
        .trim_end_matches('/')
        .to_owned()
}

fn registry_auth_lookup_keys(registry: &str) -> Vec<String> {
    if !is_docker_hub_auth_key(registry) {
        return vec![registry.to_owned()];
    }

    let mut keys = vec![registry.to_owned()];
    for key in [
        DOCKER_HUB_INDEX_AUTH_KEY,
        DOCKER_HUB_CANONICAL_HOST,
        DOCKER_HUB_REGISTRY_HOST,
        DOCKER_HUB_INDEX_HOST,
    ] {
        if !keys.iter().any(|existing| existing.as_str() == key) {
            keys.push(key.to_owned());
        }
    }

    keys
}

fn registry_auth_helper_server(registry: &str) -> String {
    if is_docker_hub_auth_key(registry) {
        DOCKER_HUB_CREDENTIAL_HELPER_SERVER.to_owned()
    } else {
        registry.to_owned()
    }
}

fn is_docker_hub_auth_key(registry: &str) -> bool {
    matches!(
        registry,
        DOCKER_HUB_CANONICAL_HOST
            | DOCKER_HUB_REGISTRY_HOST
            | DOCKER_HUB_INDEX_HOST
            | DOCKER_HUB_INDEX_AUTH_KEY
    )
}

#[derive(Debug, Deserialize)]
struct DockerConfigFile {
    #[serde(default)]
    auths: BTreeMap<String, DockerAuthEntry>,
    #[serde(default, rename = "credHelpers")]
    cred_helpers: BTreeMap<String, String>,
    #[serde(default, rename = "credsStore")]
    creds_store: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DockerAuthEntry {
    auth: Option<String>,
    username: Option<String>,
    password: Option<String>,
    identitytoken: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DockerCredentialHelperGetResponse {
    username: Option<String>,
    secret: Option<String>,
}

impl DockerAuthEntry {
    fn to_registry_auth(&self) -> Result<Option<RegistryAuth>> {
        if let Some(token) = &self.identitytoken {
            return Ok(Some(RegistryAuth::Bearer(token.clone())));
        }
        if let Some(auth) = &self.auth {
            let decoded = BASE64
                .decode(auth)
                .with_context(|| "Docker registry auth is not valid base64")?;
            let decoded = String::from_utf8(decoded)
                .with_context(|| "Docker registry auth is not valid UTF-8")?;
            let (username, password) = decoded
                .split_once(':')
                .ok_or_else(|| anyhow!("Docker registry auth must be username:password"))?;
            return Ok(Some(RegistryAuth::Basic {
                username: username.to_owned(),
                password: password.to_owned(),
            }));
        }
        if let (Some(username), Some(password)) = (&self.username, &self.password) {
            return Ok(Some(RegistryAuth::Basic {
                username: username.clone(),
                password: password.clone(),
            }));
        }

        Ok(None)
    }
}

fn docker_config_helper_auth(
    helper: &str,
    helper_server: &str,
    helper_paths: &[PathBuf],
) -> Result<Option<RegistryAuth>> {
    let Some(binary) = docker_credential_helper_binary(helper, helper_paths) else {
        bail!("Docker credential helper was not found: docker-credential-{helper}");
    };
    let Some(output) = run_docker_credential_helper_get(&binary, helper_server)? else {
        return Ok(None);
    };
    let Some(secret) = output.secret else {
        return Ok(None);
    };
    let username = output.username.unwrap_or_default();
    if username == "<token>" {
        Ok(Some(RegistryAuth::Bearer(secret)))
    } else {
        Ok(Some(RegistryAuth::Basic {
            username,
            password: secret,
        }))
    }
}

fn docker_credential_helper_binary(helper: &str, helper_paths: &[PathBuf]) -> Option<PathBuf> {
    let binary = format!("docker-credential-{helper}");
    helper_paths
        .iter()
        .map(|path| path.join(&binary))
        .find(|path| path.is_file())
}

fn run_docker_credential_helper_get(
    binary: &Path,
    registry: &str,
) -> Result<Option<DockerCredentialHelperGetResponse>> {
    let mut child = spawn_docker_credential_helper_get(binary)?;
    child
        .stdin
        .as_mut()
        .context("Docker credential helper stdin was not available")?
        .write_all(registry.as_bytes())
        .with_context(|| {
            format!(
                "Failed to write registry to Docker credential helper: {}",
                binary.display()
            )
        })?;
    let output = child.wait_with_output().with_context(|| {
        format!(
            "Failed to wait for Docker credential helper: {}",
            binary.display()
        )
    })?;
    if !output.status.success() {
        let message = docker_credential_helper_error_message(&output);
        if docker_credential_helper_credentials_not_found(&message) {
            return Ok(None);
        }
        bail!(
            "Docker credential helper failed for {registry}: {}",
            message
        );
    }

    serde_json::from_slice(&output.stdout)
        .map(Some)
        .with_context(|| {
            format!(
                "Failed to parse Docker credential helper output: {}",
                binary.display()
            )
        })
}

fn spawn_docker_credential_helper_get(binary: &Path) -> Result<Child> {
    let mut last_error = None;
    for attempt in 0..=DOCKER_CREDENTIAL_HELPER_SPAWN_RETRIES {
        match docker_credential_helper_get_command(binary).spawn() {
            Ok(child) => return Ok(child),
            Err(error)
                if is_text_file_busy(&error)
                    && attempt < DOCKER_CREDENTIAL_HELPER_SPAWN_RETRIES =>
            {
                last_error = Some(error);
                thread::sleep(DOCKER_CREDENTIAL_HELPER_SPAWN_RETRY_DELAY);
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "Failed to spawn Docker credential helper: {}",
                        binary.display()
                    )
                });
            }
        }
    }

    Err(last_error
        .unwrap_or_else(|| io::Error::other("Docker credential helper spawn retry exhausted")))
    .with_context(|| {
        format!(
            "Failed to spawn Docker credential helper: {}",
            binary.display()
        )
    })
}

fn docker_credential_helper_get_command(binary: &Path) -> HostCommand {
    let mut command = HostCommand::new(binary);
    command
        .arg("get")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn is_text_file_busy(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::ETXTBSY)
}

fn docker_credential_helper_error_message(output: &std::process::Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    [stdout.trim(), stderr.trim()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(": ")
}

fn docker_credential_helper_credentials_not_found(message: &str) -> bool {
    message
        .to_ascii_lowercase()
        .contains("credentials not found")
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Write, os::unix::fs::PermissionsExt, path::Path};

    use super::*;

    fn write_credential_helper(path: &Path, contents: &str) {
        let staged = path.with_extension(format!("tmp-{}", std::process::id()));
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&staged)
            .unwrap();
        file.write_all(contents.as_bytes()).unwrap();
        file.flush().unwrap();
        file.sync_all().unwrap();
        drop(file);
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o755)).unwrap();
        fs::rename(&staged, path).unwrap();
    }

    #[test]
    fn docker_config_auth_decodes_registry_credentials() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config.json");
        fs::write(
            &config,
            r#"{"auths":{"ghcr.io":{"auth":"dXNlcjp0b2tlbg=="}}}"#,
        )
        .unwrap();

        let auth = DockerConfigAuthStore::from_config_file(&config)
            .unwrap()
            .get("ghcr.io")
            .unwrap();

        assert_eq!(
            auth,
            Some(RegistryAuth::Basic {
                username: "user".to_owned(),
                password: "token".to_owned(),
            })
        );
    }

    #[test]
    fn docker_config_auth_matches_docker_hub_index_auth_for_aliases() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config.json");
        fs::write(
            &config,
            r#"{"auths":{"https://index.docker.io/v1/":{"auth":"aHViOnRva2Vu"}}}"#,
        )
        .unwrap();
        let store = DockerConfigAuthStore::from_config_file(&config).unwrap();

        for registry in ["docker.io", "registry-1.docker.io", "index.docker.io"] {
            assert_eq!(
                store.get(registry).unwrap(),
                Some(RegistryAuth::Basic {
                    username: "hub".to_owned(),
                    password: "token".to_owned(),
                }),
                "{registry}"
            );
        }
    }

    #[test]
    fn docker_config_auth_uses_registry_credential_helper_before_inline_auth() {
        let temp = tempfile::tempdir().unwrap();
        let helper_dir = temp.path().join("bin");
        fs::create_dir_all(&helper_dir).unwrap();
        let helper = helper_dir.join("docker-credential-fake");
        write_credential_helper(
            &helper,
            r#"#!/bin/sh
set -eu
test "$1" = get
server="$(cat)"
test "$server" = ghcr.io
printf '{"Username":"helper-user","Secret":"helper-token"}'
"#,
        );
        let config = temp.path().join("config.json");
        fs::write(
            &config,
            r#"{
                "credHelpers": {
                    "ghcr.io": "fake"
                },
                "auths": {
                    "ghcr.io": {
                        "auth": "aW5saW5lOnRva2Vu"
                    }
                }
            }"#,
        )
        .unwrap();

        let store =
            DockerConfigAuthStore::from_config_file_with_helper_paths(&config, &[helper_dir])
                .unwrap();

        assert_eq!(
            store.get("ghcr.io").unwrap(),
            Some(RegistryAuth::Basic {
                username: "helper-user".to_owned(),
                password: "helper-token".to_owned(),
            })
        );
    }

    #[test]
    fn docker_config_auth_uses_docker_hub_index_address_for_default_credential_store() {
        let temp = tempfile::tempdir().unwrap();
        let helper_dir = temp.path().join("bin");
        fs::create_dir_all(&helper_dir).unwrap();
        let helper = helper_dir.join("docker-credential-store");
        write_credential_helper(
            &helper,
            r#"#!/bin/sh
set -eu
test "$1" = get
server="$(cat)"
test "$server" = https://index.docker.io/v1/
printf '{"Username":"hub-user","Secret":"hub-token"}'
"#,
        );
        let config = temp.path().join("config.json");
        fs::write(
            &config,
            r#"{
                "credsStore": "store"
            }"#,
        )
        .unwrap();

        let store =
            DockerConfigAuthStore::from_config_file_with_helper_paths(&config, &[helper_dir])
                .unwrap();

        assert_eq!(
            store.get("docker.io").unwrap(),
            Some(RegistryAuth::Basic {
                username: "hub-user".to_owned(),
                password: "hub-token".to_owned(),
            })
        );
    }

    #[test]
    fn docker_config_auth_uses_trailing_slash_docker_hub_helper_server() {
        let temp = tempfile::tempdir().unwrap();
        let helper_dir = temp.path().join("bin");
        fs::create_dir_all(&helper_dir).unwrap();
        let helper = helper_dir.join("docker-credential-hub");
        write_credential_helper(
            &helper,
            r#"#!/bin/sh
set -eu
test "$1" = get
server="$(cat)"
test "$server" = https://index.docker.io/v1/
printf '{"Username":"hub-user","Secret":"hub-token"}'
"#,
        );
        let config = temp.path().join("config.json");
        fs::write(
            &config,
            r#"{
                "credHelpers": {
                    "docker.io": "hub"
                }
            }"#,
        )
        .unwrap();

        let store =
            DockerConfigAuthStore::from_config_file_with_helper_paths(&config, &[helper_dir])
                .unwrap();

        assert_eq!(
            store.get("docker.io").unwrap(),
            Some(RegistryAuth::Basic {
                username: "hub-user".to_owned(),
                password: "hub-token".to_owned(),
            })
        );
    }

    #[test]
    fn docker_config_auth_does_not_fallback_to_inline_auth_when_helper_has_no_credentials() {
        let temp = tempfile::tempdir().unwrap();
        let helper_dir = temp.path().join("bin");
        fs::create_dir_all(&helper_dir).unwrap();
        let helper = helper_dir.join("docker-credential-fake");
        write_credential_helper(
            &helper,
            r#"#!/bin/sh
set -eu
test "$1" = get
server="$(cat)"
test "$server" = ghcr.io
printf 'credentials not found in native keychain'
exit 1
"#,
        );
        let config = temp.path().join("config.json");
        fs::write(
            &config,
            r#"{
                "credHelpers": {
                    "ghcr.io": "fake"
                },
                "auths": {
                    "ghcr.io": {
                        "auth": "aW5saW5lOnRva2Vu"
                    }
                }
            }"#,
        )
        .unwrap();

        let store =
            DockerConfigAuthStore::from_config_file_with_helper_paths(&config, &[helper_dir])
                .unwrap();

        assert_eq!(store.get("ghcr.io").unwrap(), None);
    }

    #[test]
    fn docker_config_auth_uses_registry_helper_instead_of_default_store() {
        let temp = tempfile::tempdir().unwrap();
        let helper_dir = temp.path().join("bin");
        fs::create_dir_all(&helper_dir).unwrap();
        let helper = helper_dir.join("docker-credential-fake");
        write_credential_helper(
            &helper,
            r#"#!/bin/sh
set -eu
test "$1" = get
server="$(cat)"
test "$server" = ghcr.io
printf 'credentials not found in native keychain'
exit 1
"#,
        );
        let store_helper = helper_dir.join("docker-credential-store");
        write_credential_helper(
            &store_helper,
            r#"#!/bin/sh
set -eu
test "$1" = get
server="$(cat)"
test "$server" = ghcr.io
printf '{"Username":"store-user","Secret":"store-token"}'
"#,
        );
        let config = temp.path().join("config.json");
        fs::write(
            &config,
            r#"{
                "credHelpers": {
                    "ghcr.io": "fake"
                },
                "credsStore": "store"
            }"#,
        )
        .unwrap();

        let store =
            DockerConfigAuthStore::from_config_file_with_helper_paths(&config, &[helper_dir])
                .unwrap();

        assert_eq!(store.get("ghcr.io").unwrap(), None);
    }

    #[test]
    fn docker_config_auth_does_not_fallback_to_inline_auth_when_default_store_has_no_credentials() {
        let temp = tempfile::tempdir().unwrap();
        let helper_dir = temp.path().join("bin");
        fs::create_dir_all(&helper_dir).unwrap();
        let helper = helper_dir.join("docker-credential-fake");
        write_credential_helper(
            &helper,
            r#"#!/bin/sh
set -eu
test "$1" = get
server="$(cat)"
test "$server" = ghcr.io
printf 'credentials not found in native keychain'
exit 1
"#,
        );
        let config = temp.path().join("config.json");
        fs::write(
            &config,
            r#"{
                "credsStore": "fake",
                "auths": {
                    "ghcr.io": {
                        "auth": "aW5saW5lOnRva2Vu"
                    }
                }
            }"#,
        )
        .unwrap();

        let store =
            DockerConfigAuthStore::from_config_file_with_helper_paths(&config, &[helper_dir])
                .unwrap();

        assert_eq!(store.get("ghcr.io").unwrap(), None);
    }

    #[test]
    fn docker_config_auth_errors_when_configured_registry_helper_is_missing() {
        let temp = tempfile::tempdir().unwrap();
        let helper_dir = temp.path().join("bin");
        fs::create_dir_all(&helper_dir).unwrap();
        let config = temp.path().join("config.json");
        fs::write(
            &config,
            r#"{
                "credHelpers": {
                    "ghcr.io": "missing"
                },
                "auths": {
                    "ghcr.io": {
                        "auth": "aW5saW5lOnRva2Vu"
                    }
                }
            }"#,
        )
        .unwrap();

        let store =
            DockerConfigAuthStore::from_config_file_with_helper_paths(&config, &[helper_dir])
                .unwrap();
        let error = store.get("ghcr.io").unwrap_err();
        let message = format!("{error:#}");

        assert!(
            message.contains("Docker credential helper was not found"),
            "{error:#}"
        );
    }

    #[test]
    fn docker_config_auth_treats_missing_helper_credentials_as_no_auth() {
        let temp = tempfile::tempdir().unwrap();
        let helper_dir = temp.path().join("bin");
        fs::create_dir_all(&helper_dir).unwrap();
        let helper = helper_dir.join("docker-credential-fake");
        write_credential_helper(
            &helper,
            r#"#!/bin/sh
set -eu
test "$1" = get
server="$(cat)"
test "$server" = ghcr.io
printf 'credentials not found in native keychain'
exit 1
"#,
        );
        let config = temp.path().join("config.json");
        fs::write(
            &config,
            r#"{
                "credsStore": "fake"
            }"#,
        )
        .unwrap();

        let store =
            DockerConfigAuthStore::from_config_file_with_helper_paths(&config, &[helper_dir])
                .unwrap();

        assert_eq!(store.get("ghcr.io").unwrap(), None);
    }

    #[test]
    fn docker_config_auth_uses_default_credential_store_when_registry_helper_is_absent() {
        let temp = tempfile::tempdir().unwrap();
        let helper_dir = temp.path().join("bin");
        fs::create_dir_all(&helper_dir).unwrap();
        let helper = helper_dir.join("docker-credential-store");
        write_credential_helper(
            &helper,
            r#"#!/bin/sh
set -eu
test "$1" = get
server="$(cat)"
test "$server" = ghcr.io
printf '{"Username":"store-user","Secret":"store-token"}'
"#,
        );
        let config = temp.path().join("config.json");
        fs::write(
            &config,
            r#"{
                "credsStore": "store"
            }"#,
        )
        .unwrap();

        let store =
            DockerConfigAuthStore::from_config_file_with_helper_paths(&config, &[helper_dir])
                .unwrap();

        assert_eq!(
            store.get("ghcr.io").unwrap(),
            Some(RegistryAuth::Basic {
                username: "store-user".to_owned(),
                password: "store-token".to_owned(),
            })
        );
    }
}
