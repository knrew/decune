use std::{
    fs, io,
    path::{Path, PathBuf},
};

use anyhow::{Result, anyhow};
use serde_json::Value;

use crate::error::ResultExt;

const PRIMARY_METADATA_PATH: &str = ".devcontainer/devcontainer.json";
const ROOT_METADATA_PATH: &str = ".devcontainer.json";
const DEVCONTAINER_DIRECTORY: &str = ".devcontainer";
const METADATA_FILE_NAME: &str = "devcontainer.json";

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DevcontainerJson {
    path: PathBuf,
    value: Value,
}

#[allow(dead_code)]
impl DevcontainerJson {
    pub(crate) fn load(workspace_root: &Path, explicit_config_path: Option<&Path>) -> Result<Self> {
        let path = discover(workspace_root, explicit_config_path)?;
        let value = parse_file(&path)?;

        Ok(Self { path, value })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn value(&self) -> &Value {
        &self.value
    }
}

#[allow(dead_code)]
pub(crate) fn discover(
    workspace_root: &Path,
    explicit_config_path: Option<&Path>,
) -> Result<PathBuf> {
    if let Some(explicit_config_path) = explicit_config_path {
        return resolve_explicit_path(workspace_root, explicit_config_path);
    }

    if let Some(path) = metadata_file(workspace_root.join(PRIMARY_METADATA_PATH))? {
        return Ok(path);
    }

    if let Some(path) = metadata_file(workspace_root.join(ROOT_METADATA_PATH))? {
        return Ok(path);
    }

    let nested_candidates = nested_metadata_files(workspace_root)?;
    match nested_candidates.as_slice() {
        [path] => Ok(path.clone()),
        [] => Err(anyhow!(
            "Devcontainer metadata file was not found under workspace: {}",
            workspace_root.display()
        )),
        paths => Err(anyhow!(
            "Multiple devcontainer metadata files found; pass --config to choose one: {}",
            paths
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

#[allow(dead_code)]
pub(crate) fn parse_file(path: &Path) -> Result<Value> {
    let contents =
        fs::read_to_string(path).with_path_context("read devcontainer metadata file", path)?;
    parse_str(&contents).map_err(|error| {
        anyhow!(
            "Failed to parse devcontainer metadata file: {}: {error}",
            path.display()
        )
    })
}

#[allow(dead_code)]
pub(crate) fn parse_str(contents: &str) -> Result<Value> {
    let normalized = normalize_jsonc(contents)?;
    serde_json::from_str(&normalized).map_err(|error| anyhow!("{error}"))
}

fn normalize_jsonc(contents: &str) -> Result<String> {
    remove_trailing_commas(&strip_jsonc_comments(contents)?)
}

fn strip_jsonc_comments(contents: &str) -> Result<String> {
    let bytes = contents.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        match bytes[index] {
            b'"' => copy_string(bytes, &mut output, &mut index),
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                output.extend_from_slice(b"  ");
                index += 2;
                while index < bytes.len() {
                    let byte = bytes[index];
                    if is_line_ending(byte) {
                        output.push(byte);
                    } else {
                        output.push(b' ');
                    }
                    index += 1;
                    if is_line_ending(byte) {
                        break;
                    }
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                let comment_start = index;
                output.extend_from_slice(b"  ");
                index += 2;
                let mut closed = false;

                while index < bytes.len() {
                    if bytes[index] == b'*' && bytes.get(index + 1) == Some(&b'/') {
                        output.extend_from_slice(b"  ");
                        index += 2;
                        closed = true;
                        break;
                    }

                    let byte = bytes[index];
                    if is_line_ending(byte) {
                        output.push(byte);
                    } else {
                        output.push(b' ');
                    }
                    index += 1;
                }

                if !closed {
                    let (line, column) = line_column(contents, comment_start);
                    return Err(anyhow!(
                        "Unterminated block comment in devcontainer metadata at line {line} column {column}"
                    ));
                }
            }
            byte => {
                output.push(byte);
                index += 1;
            }
        }
    }

    String::from_utf8(output).map_err(|error| anyhow!("{error}"))
}

fn remove_trailing_commas(contents: &str) -> Result<String> {
    let bytes = contents.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        match bytes[index] {
            b'"' => copy_string(bytes, &mut output, &mut index),
            b',' if next_non_whitespace(bytes, index + 1)
                .is_some_and(|byte| matches!(byte, b']' | b'}')) =>
            {
                output.push(b' ');
                index += 1;
            }
            byte => {
                output.push(byte);
                index += 1;
            }
        }
    }

    String::from_utf8(output).map_err(|error| anyhow!("{error}"))
}

fn copy_string(bytes: &[u8], output: &mut Vec<u8>, index: &mut usize) {
    output.push(bytes[*index]);
    *index += 1;

    while *index < bytes.len() {
        let byte = bytes[*index];
        output.push(byte);
        *index += 1;

        if byte == b'\\' && *index < bytes.len() {
            output.push(bytes[*index]);
            *index += 1;
            continue;
        }

        if byte == b'"' {
            break;
        }
    }
}

fn next_non_whitespace(bytes: &[u8], start: usize) -> Option<u8> {
    bytes[start..]
        .iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())
}

fn is_line_ending(byte: u8) -> bool {
    matches!(byte, b'\n' | b'\r')
}

fn line_column(contents: &str, byte_index: usize) -> (usize, usize) {
    let mut line = 1;
    let mut column = 1;

    for (index, ch) in contents.char_indices() {
        if index >= byte_index {
            break;
        }

        if ch == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }

    (line, column)
}

fn resolve_explicit_path(workspace_root: &Path, explicit_config_path: &Path) -> Result<PathBuf> {
    let path = if explicit_config_path.is_absolute() {
        explicit_config_path.to_path_buf()
    } else {
        workspace_root.join(explicit_config_path)
    };

    ensure_metadata_file(&path)?;
    Ok(path)
}

fn nested_metadata_files(workspace_root: &Path) -> Result<Vec<PathBuf>> {
    let devcontainer_dir = workspace_root.join(DEVCONTAINER_DIRECTORY);
    let entries = match fs::read_dir(&devcontainer_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_path_context("read devcontainer directory", &devcontainer_dir);
        }
    };

    let mut candidates = Vec::new();
    for entry in entries {
        let entry =
            entry.with_path_context("read devcontainer directory entry", &devcontainer_dir)?;
        let file_type = entry
            .file_type()
            .with_path_context("read devcontainer directory entry type", entry.path())?;

        if !file_type.is_dir() {
            continue;
        }

        let path = entry.path().join(METADATA_FILE_NAME);
        if let Some(path) = metadata_file(path)? {
            candidates.push(path);
        }
    }

    candidates.sort();
    Ok(candidates)
}

fn metadata_file(path: PathBuf) -> Result<Option<PathBuf>> {
    match fs::metadata(&path) {
        Ok(metadata) if metadata.is_file() => Ok(Some(path)),
        Ok(metadata) if metadata.is_dir() => Err(anyhow!(
            "Devcontainer metadata path is a directory: {}",
            path.display()
        )),
        Ok(_) => Err(anyhow!(
            "Devcontainer metadata path is not a file: {}",
            path.display()
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_path_context("read devcontainer metadata file metadata", &path)
        }
    }
}

fn ensure_metadata_file(path: &Path) -> Result<()> {
    match metadata_file(path.to_path_buf())? {
        Some(_) => Ok(()),
        None => Err(anyhow!(
            "Configured devcontainer metadata file does not exist: {}",
            path.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;
    use tempfile::TempDir;

    use super::{DevcontainerJson, discover, normalize_jsonc, parse_file, parse_str};

    #[test]
    fn primary_metadata_path_has_highest_priority() {
        let workspace = temp_workspace();
        write_file(
            workspace.path(),
            ".devcontainer/devcontainer.json",
            r#"{"image":"primary"}"#,
        );
        write_file(
            workspace.path(),
            ".devcontainer.json",
            r#"{"image":"root"}"#,
        );

        let path = discover(workspace.path(), None).unwrap();

        assert_eq!(
            path,
            workspace.path().join(".devcontainer/devcontainer.json")
        );
    }

    #[test]
    fn root_metadata_path_is_used_as_fallback() {
        let workspace = temp_workspace();
        write_file(
            workspace.path(),
            ".devcontainer.json",
            r#"{"image":"root"}"#,
        );

        let path = discover(workspace.path(), None).unwrap();

        assert_eq!(path, workspace.path().join(".devcontainer.json"));
    }

    #[test]
    fn single_nested_metadata_path_is_discovered() {
        let workspace = temp_workspace();
        write_file(
            workspace.path(),
            ".devcontainer/rust/devcontainer.json",
            r#"{"image":"rust"}"#,
        );

        let path = discover(workspace.path(), None).unwrap();

        assert_eq!(
            path,
            workspace
                .path()
                .join(".devcontainer/rust/devcontainer.json")
        );
    }

    #[test]
    fn multiple_nested_metadata_paths_require_explicit_config() {
        let workspace = temp_workspace();
        write_file(
            workspace.path(),
            ".devcontainer/app/devcontainer.json",
            r#"{"image":"app"}"#,
        );
        write_file(
            workspace.path(),
            ".devcontainer/tool/devcontainer.json",
            r#"{"image":"tool"}"#,
        );

        let error = discover(workspace.path(), None).unwrap_err();
        let message = error.to_string();

        assert!(message.contains("Multiple devcontainer metadata files found"));
        assert!(message.contains("--config"));
        assert!(message.contains(".devcontainer/app/devcontainer.json"));
        assert!(message.contains(".devcontainer/tool/devcontainer.json"));
    }

    #[test]
    fn missing_metadata_path_is_an_error() {
        let workspace = temp_workspace();

        let error = discover(workspace.path(), None).unwrap_err();

        assert!(error.to_string().contains("was not found under workspace"));
    }

    #[test]
    fn explicit_relative_config_path_skips_auto_detection() {
        let workspace = temp_workspace();
        write_file(
            workspace.path(),
            ".devcontainer/devcontainer.json",
            r#"{"image":"primary"}"#,
        );
        write_file(
            workspace.path(),
            "custom/devcontainer.json",
            r#"{"image":"custom"}"#,
        );

        let path = discover(
            workspace.path(),
            Some(std::path::Path::new("custom/devcontainer.json")),
        )
        .unwrap();

        assert_eq!(path, workspace.path().join("custom/devcontainer.json"));
    }

    #[test]
    fn explicit_absolute_config_path_is_accepted() {
        let workspace = temp_workspace();
        let explicit = write_file(
            workspace.path(),
            "custom/devcontainer.json",
            r#"{"image":"custom"}"#,
        );

        let path = discover(workspace.path(), Some(&explicit)).unwrap();

        assert_eq!(path, explicit);
    }

    #[test]
    fn explicit_missing_config_path_is_an_error() {
        let workspace = temp_workspace();
        let missing = std::path::Path::new("missing/devcontainer.json");

        let error = discover(workspace.path(), Some(missing)).unwrap_err();

        assert!(error.to_string().contains("does not exist"));
        assert!(error.to_string().contains("missing/devcontainer.json"));
    }

    #[test]
    fn explicit_directory_config_path_is_an_error() {
        let workspace = temp_workspace();
        fs::create_dir_all(workspace.path().join("custom/devcontainer.json")).unwrap();

        let error = discover(
            workspace.path(),
            Some(std::path::Path::new("custom/devcontainer.json")),
        )
        .unwrap_err();

        assert!(error.to_string().contains("is a directory"));
    }

    #[test]
    fn parses_jsonc_with_comments() {
        let value = parse_str(
            r#"
            {
              // devcontainer comment
              "image": "ubuntu:24.04",
              "features": {
                "ghcr.io/devcontainers/features/github-cli:1": {}
              }
            }
            "#,
        )
        .unwrap();

        assert_eq!(value["image"], json!("ubuntu:24.04"));
        assert!(value["features"].is_object());
    }

    #[test]
    fn parses_jsonc_with_trailing_commas() {
        let value = parse_str(
            r#"
            {
              "image": "ubuntu:24.04",
            }
            "#,
        )
        .unwrap();

        assert_eq!(value["image"], json!("ubuntu:24.04"));
    }

    #[test]
    fn rejects_json5_style_unquoted_property_names() {
        let error = parse_str("{ image: \"ubuntu:24.04\" }").unwrap_err();

        assert!(!error.to_string().is_empty());
    }

    #[test]
    fn preserves_comment_like_tokens_inside_strings() {
        let value = parse_str(
            r#"
            {
              "url": "https://example.test/path",
              "glob": "/* keep */",
              "comma": "value,]"
            }
            "#,
        )
        .unwrap();

        assert_eq!(value["url"], json!("https://example.test/path"));
        assert_eq!(value["glob"], json!("/* keep */"));
        assert_eq!(value["comma"], json!("value,]"));
    }

    #[test]
    fn removes_trailing_commas_after_comments() {
        let value = parse_str(
            r#"
            {
              "features": [
                "ghcr.io/devcontainers/features/github-cli:1", // keep diff-friendly comma
              ],
              "containerEnv": {
                "RUST_LOG": "debug", /* keep diff-friendly comma */
              },
            }
            "#,
        )
        .unwrap();

        assert_eq!(
            value["features"][0],
            json!("ghcr.io/devcontainers/features/github-cli:1")
        );
        assert_eq!(value["containerEnv"]["RUST_LOG"], json!("debug"));
    }

    #[test]
    fn unterminated_block_comment_is_rejected_with_location() {
        let error = parse_str("{\n  /* missing end\n  \"image\": \"ubuntu\"\n}").unwrap_err();
        let message = error.to_string();

        assert!(message.contains("Unterminated block comment"));
        assert!(message.contains("line 2"));
        assert!(message.contains("column 3"));
    }

    #[test]
    fn hash_comments_are_rejected() {
        let error = parse_str("{\n  # not jsonc\n  \"image\": \"ubuntu\"\n}").unwrap_err();

        assert!(!error.to_string().is_empty());
    }

    #[test]
    fn double_commas_are_rejected() {
        let error = parse_str(r#"{"ports": [3000,,]}"#).unwrap_err();

        assert!(!error.to_string().is_empty());
    }

    #[test]
    fn normalization_keeps_line_numbers_for_json_errors() {
        let normalized = normalize_jsonc("{\n  // comment\n  \"image\": \n}").unwrap();

        assert_eq!(normalized.lines().count(), 4);
        assert!(
            serde_json::from_str::<serde_json::Value>(&normalized)
                .unwrap_err()
                .to_string()
                .contains("line")
        );
    }

    #[test]
    fn loads_discovered_jsonc_file() {
        let workspace = temp_workspace();
        write_file(
            workspace.path(),
            ".devcontainer/devcontainer.json",
            r#"
            {
              // comment
              "image": "ubuntu:24.04"
            }
            "#,
        );

        let config = DevcontainerJson::load(workspace.path(), None).unwrap();

        assert_eq!(
            config.path(),
            workspace.path().join(".devcontainer/devcontainer.json")
        );
        assert_eq!(config.value()["image"], json!("ubuntu:24.04"));
    }

    #[test]
    fn parse_error_includes_path_and_location() {
        let workspace = temp_workspace();
        let path = write_file(
            workspace.path(),
            ".devcontainer/devcontainer.json",
            "{\n  \"image\": \n}",
        );

        let error = parse_file(&path).unwrap_err();
        let message = format!("{error:#}");

        assert!(message.contains(path.to_str().unwrap()));
        assert!(message.contains("line") || message.contains("Line") || message.contains("2"));
        assert!(message.contains("column") || message.contains("Column") || message.contains("12"));
    }

    fn temp_workspace() -> TempDir {
        tempfile::Builder::new()
            .prefix("decune-devcontainer-json-test-")
            .tempdir()
            .unwrap()
    }

    fn write_file(
        workspace: &std::path::Path,
        relative_path: &str,
        contents: &str,
    ) -> std::path::PathBuf {
        let path = workspace.join(relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, contents).unwrap();
        path
    }
}
