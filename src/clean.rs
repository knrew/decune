use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io::{self, IsTerminal, Write},
    os::{
        fd::AsRawFd,
        unix::{fs::FileTypeExt, net::UnixStream},
    },
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::{
    devcontainer::features::FeatureCacheLock,
    docker::{
        client::DockerClient,
        resource::{managed_workspace_id_from_container, managed_workspace_id_from_labels},
    },
    host::forward::forward_status_dir,
    ui,
    workspace::{
        cache_dir_for_workspace_id, decune_cache_root, decune_runtime_root, decune_state_root,
        feature_archive_cache_dir, is_valid_workspace_id, runtime_dir_for_workspace_id,
        state_dir_for_workspace_id,
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CleanOptions {
    pub(crate) dry_run: bool,
    pub(crate) no_confirm: bool,
    pub(crate) json: bool,
    pub(crate) include_feature_cache: bool,
}

#[derive(Debug, Clone, Serialize)]
struct CleanReport {
    dry_run: bool,
    include_feature_cache: bool,
    summary: CleanSummary,
    targets: Vec<CleanTargetReport>,
}

#[derive(Debug, Clone, Default, Serialize)]
struct CleanSummary {
    remove_candidates: usize,
    removed: usize,
    skipped: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum CleanTargetReport {
    Workspace(WorkspaceCleanTarget),
    FeatureCache(FeatureCacheCleanTarget),
}

#[derive(Debug, Clone, Serialize)]
struct WorkspaceCleanTarget {
    workspace_id: String,
    action: CleanAction,
    reason: CleanReason,
    removed: bool,
    paths: WorkspaceCleanPaths,
    #[serde(skip)]
    removal_paths: WorkspaceCleanPathBufs,
    existing_paths: Vec<WorkspacePathKind>,
}

#[derive(Debug, Clone, Serialize)]
struct WorkspaceCleanPaths {
    cache: String,
    state: String,
    runtime: String,
    port_status: String,
}

#[derive(Debug, Clone)]
struct WorkspaceCleanPathBufs {
    cache: PathBuf,
    state: PathBuf,
    runtime: PathBuf,
    port_status: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
enum WorkspacePathKind {
    Cache,
    State,
    Runtime,
    PortStatus,
}

#[derive(Debug, Clone, Serialize)]
struct FeatureCacheCleanTarget {
    action: CleanAction,
    reason: CleanReason,
    removed: bool,
    path: String,
    #[serde(skip)]
    removal_path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CleanAction {
    Remove,
    Skip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CleanReason {
    StaleWorkspaceData,
    FeatureCacheIncluded,
    ManagedResource,
    ActiveRuntime,
    UnsafePath,
    DockerUnavailable,
    Missing,
}

impl CleanReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::StaleWorkspaceData => "stale_workspace_data",
            Self::FeatureCacheIncluded => "feature_cache_included",
            Self::ManagedResource => "managed_resource",
            Self::ActiveRuntime => "active_runtime",
            Self::UnsafePath => "unsafe_path",
            Self::DockerUnavailable => "docker_unavailable",
            Self::Missing => "missing",
        }
    }
}

pub(crate) async fn run_clean(options: CleanOptions) -> Result<()> {
    let mut report = build_clean_report(options.dry_run, options.include_feature_cache).await?;
    let has_remove_candidates = report.summary.remove_candidates > 0;

    if !options.json {
        print_clean_summary(&report, options.dry_run);
    }

    if !options.dry_run {
        ensure_clean_confirmed(CleanConfirmation {
            no_confirm: options.no_confirm,
            stdin_is_terminal: io::stdin().is_terminal(),
            has_targets: has_remove_candidates,
        })?;
    }

    if !options.dry_run {
        apply_clean_report(&mut report).await?;
    }

    if options.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).context("Failed to serialize clean report")?
        );
    } else if report.summary.remove_candidates == 0 {
        ui::done("No stale decune generated data found");
    } else if options.dry_run {
        ui::done("Dry run completed without removing generated data");
    } else {
        ui::done("Removed stale decune generated data");
    }

    Ok(())
}

async fn build_clean_report(dry_run: bool, include_feature_cache: bool) -> Result<CleanReport> {
    let (docker_workspace_ids, docker_unavailable) = match discover_managed_workspace_ids().await {
        Ok(workspace_ids) => (workspace_ids, false),
        Err(_error) if dry_run => (BTreeSet::new(), true),
        Err(error) => {
            return Err(error)
                .context("Failed to determine reusable decune-managed Docker resources");
        }
    };

    let mut targets = discover_workspace_clean_targets(docker_unavailable, &docker_workspace_ids)?;
    if include_feature_cache {
        targets.push(CleanTargetReport::FeatureCache(
            feature_cache_clean_target()?
        ));
    }
    targets.sort_by_key(clean_target_sort_key);

    Ok(CleanReport {
        dry_run,
        include_feature_cache,
        summary: summarize_targets(&targets),
        targets,
    })
}

async fn discover_managed_workspace_ids() -> Result<BTreeSet<String>> {
    let client = DockerClient::connect_from_env();
    let containers = client
        .cli()
        .list_all_managed_container_inspects()
        .await
        .context("Failed to list decune-managed Docker containers")?;
    let volumes = client
        .cli()
        .list_all_managed_volume_inspects()
        .await
        .context("Failed to list decune-managed Docker volumes")?;
    let mut workspace_ids = BTreeSet::new();

    for container in containers {
        if let Some((workspace_id, _labels)) = managed_workspace_id_from_container(&container) {
            workspace_ids.insert(workspace_id);
        }
    }
    for volume in volumes {
        let Some(labels) = volume.labels.as_ref() else {
            continue;
        };
        if let Some(workspace_id) = managed_workspace_id_from_labels(labels) {
            workspace_ids.insert(workspace_id);
        }
    }

    Ok(workspace_ids)
}

fn discover_workspace_clean_targets(
    docker_unavailable: bool,
    managed_workspace_ids: &BTreeSet<String>,
) -> Result<Vec<CleanTargetReport>> {
    let mut workspace_ids = BTreeSet::new();
    collect_workspace_ids_from_root(&mut workspace_ids, &decune_cache_root()?, Some("features"))?;
    collect_workspace_ids_from_root(&mut workspace_ids, &decune_state_root()?, None)?;
    collect_workspace_ids_from_root(&mut workspace_ids, &decune_runtime_root(), None)?;
    collect_workspace_ids_from_port_status_dirs(&mut workspace_ids, &decune_runtime_root())?;
    workspace_ids.extend(managed_workspace_ids.iter().cloned());

    let mut targets = Vec::new();
    for workspace_id in workspace_ids {
        let target =
            workspace_clean_target(&workspace_id, docker_unavailable, managed_workspace_ids)?;
        if !target.existing_paths.is_empty() {
            targets.push(CleanTargetReport::Workspace(target));
        }
    }
    Ok(targets)
}

fn collect_workspace_ids_from_root(
    workspace_ids: &mut BTreeSet<String>,
    root: &Path,
    excluded_name: Option<&str>,
) -> Result<()> {
    let Some(entries) = read_non_symlink_root(root, "decune data root")? else {
        return Ok(());
    };

    for entry in entries {
        let entry = entry.with_context(|| {
            format!("Failed to read decune data root entry: {}", root.display())
        })?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if excluded_name.is_some_and(|excluded| excluded == name) {
            continue;
        }
        if is_valid_workspace_id(&name) {
            workspace_ids.insert(name);
        }
    }

    Ok(())
}

fn collect_workspace_ids_from_port_status_dirs(
    workspace_ids: &mut BTreeSet<String>,
    root: &Path,
) -> Result<()> {
    let Some(entries) = read_non_symlink_root(root, "decune runtime root")? else {
        return Ok(());
    };

    for entry in entries {
        let entry = entry.with_context(|| {
            format!(
                "Failed to read decune runtime root entry: {}",
                root.display()
            )
        })?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(workspace_id) = name.strip_suffix("-ports") else {
            continue;
        };
        if is_valid_workspace_id(workspace_id) {
            workspace_ids.insert(workspace_id.to_owned());
        }
    }

    Ok(())
}

fn read_non_symlink_root(root: &Path, description: &str) -> Result<Option<fs::ReadDir>> {
    let metadata = match root.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to inspect {description}: {}", root.display()));
        }
    };
    if metadata.file_type().is_symlink() {
        return Ok(None);
    }

    match fs::read_dir(root) {
        Ok(entries) => Ok(Some(entries)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("Failed to read {description}: {}", root.display()))
        }
    }
}

fn workspace_clean_target(
    workspace_id: &str,
    docker_unavailable: bool,
    managed_workspace_ids: &BTreeSet<String>,
) -> Result<WorkspaceCleanTarget> {
    let cache = cache_dir_for_workspace_id(workspace_id)?;
    let state = state_dir_for_workspace_id(workspace_id)?;
    let runtime = runtime_dir_for_workspace_id(workspace_id);
    let port_status = forward_status_dir(&runtime);
    let paths = WorkspaceCleanPaths {
        cache: display_path(&cache),
        state: display_path(&state),
        runtime: display_path(&runtime),
        port_status: display_path(&port_status),
    };
    let removal_paths = WorkspaceCleanPathBufs {
        cache: cache.clone(),
        state: state.clone(),
        runtime: runtime.clone(),
        port_status: port_status.clone(),
    };
    let existing_paths = existing_workspace_paths(&cache, &state, &runtime, &port_status);
    let reason = if workspace_paths_are_unsafe(&cache, &state, &runtime, &port_status)? {
        CleanReason::UnsafePath
    } else if docker_unavailable {
        CleanReason::DockerUnavailable
    } else if managed_workspace_ids.contains(workspace_id) {
        CleanReason::ManagedResource
    } else if runtime_is_active(&runtime)? || runtime_is_active(&port_status)? {
        CleanReason::ActiveRuntime
    } else if existing_paths.is_empty() {
        CleanReason::Missing
    } else {
        CleanReason::StaleWorkspaceData
    };
    let action = match reason {
        CleanReason::StaleWorkspaceData => CleanAction::Remove,
        _ => CleanAction::Skip,
    };

    Ok(WorkspaceCleanTarget {
        workspace_id: workspace_id.to_owned(),
        action,
        reason,
        removed: false,
        paths,
        removal_paths,
        existing_paths,
    })
}

fn existing_workspace_paths(
    cache: &Path,
    state: &Path,
    runtime: &Path,
    port_status: &Path,
) -> Vec<WorkspacePathKind> {
    let mut paths = Vec::new();
    push_existing_path(&mut paths, WorkspacePathKind::Cache, cache);
    push_existing_path(&mut paths, WorkspacePathKind::State, state);
    push_existing_path(&mut paths, WorkspacePathKind::Runtime, runtime);
    push_existing_path(&mut paths, WorkspacePathKind::PortStatus, port_status);
    paths
}

fn push_existing_path(paths: &mut Vec<WorkspacePathKind>, kind: WorkspacePathKind, path: &Path) {
    if path.symlink_metadata().is_ok() {
        paths.push(kind);
    }
}

fn workspace_paths_are_unsafe(
    cache: &Path,
    state: &Path,
    runtime: &Path,
    port_status: &Path,
) -> Result<bool> {
    Ok(path_is_unsafe_generated_dir(&decune_cache_root()?, cache)?
        || path_is_unsafe_generated_dir(&decune_state_root()?, state)?
        || path_is_unsafe_generated_dir(&decune_runtime_root(), runtime)?
        || path_is_unsafe_generated_dir(&decune_runtime_root(), port_status)?)
}

fn path_is_unsafe_generated_dir(root: &Path, path: &Path) -> Result<bool> {
    if !path.starts_with(root) {
        return Ok(true);
    }
    if root
        .symlink_metadata()
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Ok(true);
    }
    let metadata = match path.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "Failed to inspect decune generated data: {}",
                    path.display()
                )
            });
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Ok(true);
    }
    directory_contains_symlink(path)
}

fn directory_contains_symlink(path: &Path) -> Result<bool> {
    for entry in fs::read_dir(path)
        .with_context(|| format!("Failed to read decune generated data: {}", path.display()))?
    {
        let entry = entry.with_context(|| {
            format!(
                "Failed to read decune generated data entry: {}",
                path.display()
            )
        })?;
        let entry_path = entry.path();
        let metadata = entry_path.symlink_metadata().with_context(|| {
            format!(
                "Failed to inspect decune generated data entry: {}",
                entry_path.display()
            )
        })?;
        if metadata.file_type().is_symlink() {
            return Ok(true);
        }
        if metadata.is_dir() && directory_contains_symlink(&entry_path)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn runtime_is_active(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    for entry in fs::read_dir(path).with_context(|| {
        format!(
            "Failed to read decune runtime directory: {}",
            path.display()
        )
    })? {
        let entry = entry
            .with_context(|| format!("Failed to read decune runtime entry: {}", path.display()))?;
        let entry_path = entry.path();
        let metadata = entry_path.symlink_metadata().with_context(|| {
            format!(
                "Failed to inspect decune runtime entry: {}",
                entry_path.display()
            )
        })?;
        if metadata.file_type().is_socket() && socket_is_connectable(&entry_path) {
            return Ok(true);
        }
        if metadata.is_file()
            && looks_like_lock_file(&entry_path)
            && lock_file_is_active(&entry_path)?
        {
            return Ok(true);
        }
        if metadata.is_dir() && runtime_is_active(&entry_path)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn socket_is_connectable(path: &Path) -> bool {
    match UnixStream::connect(path) {
        Ok(_stream) => true,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            false
        }
        Err(_) => true,
    }
}

fn looks_like_lock_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "lock" || name.ends_with(".lock"))
}

fn lock_file_is_active(path: &Path) -> Result<bool> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| {
            format!(
                "Failed to open decune runtime lock file: {}",
                path.display()
            )
        })?;
    match flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) {
        Ok(()) => {
            _ = flock(file.as_raw_fd(), libc::LOCK_UN);
            Ok(false)
        }
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(true),
        Err(error) => Err(error)
            .with_context(|| format!("Failed to inspect decune runtime lock: {}", path.display())),
    }
}

fn feature_cache_clean_target() -> Result<FeatureCacheCleanTarget> {
    let path = feature_archive_cache_dir()?;
    let reason = if path_is_unsafe_generated_dir(&decune_cache_root()?, &path)? {
        CleanReason::UnsafePath
    } else if path.exists() {
        CleanReason::FeatureCacheIncluded
    } else {
        CleanReason::Missing
    };
    let action = match reason {
        CleanReason::FeatureCacheIncluded => CleanAction::Remove,
        _ => CleanAction::Skip,
    };
    Ok(FeatureCacheCleanTarget {
        action,
        reason,
        removed: false,
        path: display_path(&path),
        removal_path: path,
    })
}

#[derive(Debug, Clone, Copy)]
struct CleanConfirmation {
    no_confirm: bool,
    stdin_is_terminal: bool,
    has_targets: bool,
}

fn ensure_clean_confirmed(confirmation: CleanConfirmation) -> Result<()> {
    if !confirmation.has_targets {
        return Ok(());
    }
    if !confirmation.no_confirm && !confirmation.stdin_is_terminal {
        bail!(
            "Cannot confirm clean in a non-interactive terminal; rerun with --no-confirm to remove generated data"
        );
    }
    if !confirmation.no_confirm && confirmation.stdin_is_terminal && !confirm_clean()? {
        bail!("Clean cancelled");
    }
    Ok(())
}

fn confirm_clean() -> Result<bool> {
    let mut stderr = io::stderr();
    stderr
        .write_all(b"Remove stale decune generated data? [y/N] ")
        .context("Failed to write clean confirmation prompt")?;
    stderr
        .flush()
        .context("Failed to flush clean confirmation prompt")?;

    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("Failed to read clean confirmation response")?;

    Ok(matches!(input.trim(), "y" | "Y" | "yes" | "YES" | "Yes"))
}

fn print_clean_summary(report: &CleanReport, dry_run: bool) {
    if report.targets.is_empty() {
        return;
    }
    ui::notice(&format!(
        "{} {} decune generated data target(s)",
        if dry_run {
            "Would inspect"
        } else {
            "Inspecting"
        },
        report.targets.len()
    ));
    for target in &report.targets {
        match target {
            CleanTargetReport::Workspace(target) => {
                let action = if target.action == CleanAction::Remove {
                    if dry_run { "would_remove" } else { "remove" }
                } else {
                    "skip"
                };
                ui::info(&format!(
                    "Workspace {} action={} reason={} paths={}",
                    target.workspace_id,
                    action,
                    target.reason.as_str(),
                    workspace_path_kinds(&target.existing_paths)
                ));
            }
            CleanTargetReport::FeatureCache(target) => {
                let action = if target.action == CleanAction::Remove {
                    if dry_run { "would_remove" } else { "remove" }
                } else {
                    "skip"
                };
                ui::info(&format!(
                    "Feature cache action={} reason={}",
                    action,
                    target.reason.as_str()
                ));
            }
        }
    }
}

fn workspace_path_kinds(paths: &[WorkspacePathKind]) -> String {
    if paths.is_empty() {
        return "-".to_owned();
    }
    paths
        .iter()
        .map(|kind| match kind {
            WorkspacePathKind::Cache => "cache",
            WorkspacePathKind::State => "state",
            WorkspacePathKind::Runtime => "runtime",
            WorkspacePathKind::PortStatus => "port_status",
        })
        .collect::<Vec<_>>()
        .join(",")
}

async fn apply_clean_report(report: &mut CleanReport) -> Result<()> {
    for target in &mut report.targets {
        match target {
            CleanTargetReport::Workspace(target) if target.action == CleanAction::Remove => {
                let workspace_id = target.workspace_id.clone();
                let refreshed = revalidate_workspace_clean_target(&workspace_id).await?;
                apply_workspace_clean_target(target, refreshed)?;
            }
            CleanTargetReport::FeatureCache(target) if target.action == CleanAction::Remove => {
                let _lock = FeatureCacheLock::acquire_exclusive(&target.removal_path)?;
                remove_dir_if_exists(&target.removal_path)?;
                target.removed = true;
            }
            _ => {}
        }
    }
    report.summary = summarize_targets(&report.targets);
    Ok(())
}

async fn revalidate_workspace_clean_target(workspace_id: &str) -> Result<WorkspaceCleanTarget> {
    let managed_workspace_ids = discover_managed_workspace_ids()
        .await
        .context("Failed to determine reusable decune-managed Docker resources before removal")?;
    workspace_clean_target(workspace_id, false, &managed_workspace_ids)
}

fn apply_workspace_clean_target(
    target: &mut WorkspaceCleanTarget,
    refreshed: WorkspaceCleanTarget,
) -> Result<()> {
    *target = refreshed;
    if target.action != CleanAction::Remove {
        return Ok(());
    }

    remove_dir_if_exists(&target.removal_paths.cache)?;
    remove_dir_if_exists(&target.removal_paths.state)?;
    remove_dir_if_exists(&target.removal_paths.runtime)?;
    remove_dir_if_exists(&target.removal_paths.port_status)?;
    target.removed = true;
    Ok(())
}

fn remove_dir_if_exists(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("Failed to remove decune generated data: {}", path.display())),
    }
}

fn summarize_targets(targets: &[CleanTargetReport]) -> CleanSummary {
    let mut summary = CleanSummary::default();
    for target in targets {
        match target {
            CleanTargetReport::Workspace(target) => {
                if target.action == CleanAction::Remove {
                    summary.remove_candidates += 1;
                } else {
                    summary.skipped += 1;
                }
                if target.removed {
                    summary.removed += 1;
                }
            }
            CleanTargetReport::FeatureCache(target) => {
                if target.action == CleanAction::Remove {
                    summary.remove_candidates += 1;
                } else {
                    summary.skipped += 1;
                }
                if target.removed {
                    summary.removed += 1;
                }
            }
        }
    }
    summary
}

fn clean_target_sort_key(target: &CleanTargetReport) -> (u8, String) {
    match target {
        CleanTargetReport::Workspace(target) => (0, target.workspace_id.clone()),
        CleanTargetReport::FeatureCache(_) => (1, "features".to_owned()),
    }
}

fn display_path(path: &Path) -> String {
    path.display().to_string()
}

fn flock(fd: i32, operation: i32) -> io::Result<()> {
    loop {
        let status = unsafe { libc::flock(fd, operation) };
        if status == 0 {
            return Ok(());
        }

        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(error);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsString,
        fs::{self, OpenOptions},
        os::{
            fd::AsRawFd,
            unix::{ffi::OsStringExt, fs as unix_fs, net::UnixListener},
        },
        path::{Path, PathBuf},
        sync::{Mutex, MutexGuard, OnceLock},
    };

    use super::*;

    #[test]
    fn workspace_target_removes_stale_workspace_paths_as_a_unit() {
        let roots = TestRoots::new();
        let workspace_id = "123456abcdef";
        let cache = roots.cache_home.path().join("decune").join(workspace_id);
        let state = roots.state_home.path().join("decune").join(workspace_id);
        let runtime = roots.runtime_home.path().join("decune").join(workspace_id);
        fs::create_dir_all(&cache).unwrap();
        fs::create_dir_all(&state).unwrap();
        fs::create_dir_all(&runtime).unwrap();
        fs::write(cache.join("marker"), "cache\n").unwrap();
        fs::write(state.join("marker"), "state\n").unwrap();
        fs::write(runtime.join("marker"), "runtime\n").unwrap();
        let _env = roots.apply();

        let mut report = CleanReport {
            dry_run: false,
            include_feature_cache: false,
            summary: CleanSummary::default(),
            targets: vec![CleanTargetReport::Workspace(
                workspace_clean_target(workspace_id, false, &BTreeSet::new()).unwrap(),
            )],
        };
        report.summary = summarize_targets(&report.targets);

        apply_clean_report_for_test(&mut report, &BTreeSet::new()).unwrap();

        assert!(!cache.exists());
        assert!(!state.exists());
        assert!(!runtime.exists());
        assert_eq!(report.summary.removed, 1);
    }

    #[test]
    fn workspace_target_removes_non_utf8_workspace_paths_as_a_unit() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_id = "123456abcdef";
        let cache_home = temp
            .path()
            .join(PathBuf::from(OsString::from_vec(b"cache-\xff".to_vec())));
        let state_home = temp.path().join("state-home");
        let runtime_home = temp.path().join("runtime-home");
        let cache = cache_home.join("decune").join(workspace_id);
        let state = state_home.join("decune").join(workspace_id);
        let runtime = runtime_home.join("decune").join(workspace_id);
        fs::create_dir_all(&cache).unwrap();
        fs::create_dir_all(&state).unwrap();
        fs::create_dir_all(&runtime).unwrap();
        fs::write(cache.join("marker"), "cache\n").unwrap();
        fs::write(state.join("marker"), "state\n").unwrap();
        fs::write(runtime.join("marker"), "runtime\n").unwrap();
        let _env = apply_env(&state_home, &cache_home, &runtime_home);

        let mut report = CleanReport {
            dry_run: false,
            include_feature_cache: false,
            summary: CleanSummary::default(),
            targets: vec![CleanTargetReport::Workspace(
                workspace_clean_target(workspace_id, false, &BTreeSet::new()).unwrap(),
            )],
        };
        report.summary = summarize_targets(&report.targets);

        apply_clean_report_for_test(&mut report, &BTreeSet::new()).unwrap();

        assert!(!cache.exists());
        assert!(!state.exists());
        assert!(!runtime.exists());
        assert_eq!(report.summary.removed, 1);
    }

    #[test]
    fn workspace_target_skips_managed_resource() {
        let roots = TestRoots::new();
        let workspace_id = "123456abcdef";
        let cache = roots.cache_home.path().join("decune").join(workspace_id);
        fs::create_dir_all(&cache).unwrap();
        let _env = roots.apply();
        let managed = BTreeSet::from([workspace_id.to_owned()]);

        let target = workspace_clean_target(workspace_id, false, &managed).unwrap();

        assert_eq!(target.action, CleanAction::Skip);
        assert_eq!(target.reason, CleanReason::ManagedResource);
    }

    #[test]
    fn workspace_target_skips_active_runtime_socket() {
        let roots = TestRoots::new();
        let workspace_id = "123456abcdef";
        let runtime = roots.runtime_home.path().join("decune").join(workspace_id);
        fs::create_dir_all(&runtime).unwrap();
        let _listener = UnixListener::bind(runtime.join("host-daemon.sock")).unwrap();
        let _env = roots.apply();

        let target = workspace_clean_target(workspace_id, false, &BTreeSet::new()).unwrap();

        assert_eq!(target.action, CleanAction::Skip);
        assert_eq!(target.reason, CleanReason::ActiveRuntime);
    }

    #[test]
    fn workspace_target_skips_active_runtime_lock() {
        let roots = TestRoots::new();
        let workspace_id = "123456abcdef";
        let runtime = roots.runtime_home.path().join("decune").join(workspace_id);
        fs::create_dir_all(&runtime).unwrap();
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(runtime.join("active.lock"))
            .unwrap();
        flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB).unwrap();
        let _env = roots.apply();

        let target = workspace_clean_target(workspace_id, false, &BTreeSet::new()).unwrap();

        assert_eq!(target.action, CleanAction::Skip);
        assert_eq!(target.reason, CleanReason::ActiveRuntime);
        flock(lock.as_raw_fd(), libc::LOCK_UN).unwrap();
    }

    #[test]
    fn workspace_target_is_revalidated_before_removal() {
        let roots = TestRoots::new();
        let workspace_id = "123456abcdef";
        let cache = roots.cache_home.path().join("decune").join(workspace_id);
        let state = roots.state_home.path().join("decune").join(workspace_id);
        let runtime = roots.runtime_home.path().join("decune").join(workspace_id);
        fs::create_dir_all(&cache).unwrap();
        fs::create_dir_all(&state).unwrap();
        fs::create_dir_all(&runtime).unwrap();
        fs::write(cache.join("marker"), "cache\n").unwrap();
        fs::write(state.join("marker"), "state\n").unwrap();
        fs::write(runtime.join("marker"), "runtime\n").unwrap();
        let _env = roots.apply();

        let mut report = CleanReport {
            dry_run: false,
            include_feature_cache: false,
            summary: CleanSummary::default(),
            targets: vec![CleanTargetReport::Workspace(
                workspace_clean_target(workspace_id, false, &BTreeSet::new()).unwrap(),
            )],
        };
        report.summary = summarize_targets(&report.targets);

        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(runtime.join("active.lock"))
            .unwrap();
        flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB).unwrap();

        apply_clean_report_for_test(&mut report, &BTreeSet::new()).unwrap();

        assert!(cache.exists());
        assert!(state.exists());
        assert!(runtime.exists());
        assert_eq!(report.summary.remove_candidates, 0);
        assert_eq!(report.summary.skipped, 1);
        assert_eq!(report.summary.removed, 0);
        let CleanTargetReport::Workspace(target) = &report.targets[0] else {
            panic!("expected workspace target");
        };
        assert_eq!(target.action, CleanAction::Skip);
        assert_eq!(target.reason, CleanReason::ActiveRuntime);
        flock(lock.as_raw_fd(), libc::LOCK_UN).unwrap();
    }

    #[test]
    fn workspace_target_rejects_symlink_descendant() {
        let roots = TestRoots::new();
        let workspace_id = "123456abcdef";
        let cache = roots.cache_home.path().join("decune").join(workspace_id);
        let outside = roots.cache_home.path().join("outside");
        fs::create_dir_all(&cache).unwrap();
        fs::create_dir_all(&outside).unwrap();
        unix_fs::symlink(&outside, cache.join("link")).unwrap();
        let _env = roots.apply();

        let target = workspace_clean_target(workspace_id, false, &BTreeSet::new()).unwrap();

        assert_eq!(target.action, CleanAction::Skip);
        assert_eq!(target.reason, CleanReason::UnsafePath);
    }

    #[test]
    fn workspace_discovery_skips_symlink_data_root() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_id = "123456abcdef";
        let root = temp.path().join("decune");
        let linked_root = temp.path().join("linked-root");
        fs::create_dir_all(root.join(workspace_id)).unwrap();
        unix_fs::symlink(&root, &linked_root).unwrap();
        let mut workspace_ids = BTreeSet::new();

        collect_workspace_ids_from_root(&mut workspace_ids, &linked_root, None).unwrap();

        assert!(workspace_ids.is_empty());
    }

    #[test]
    fn port_status_discovery_skips_symlink_runtime_root() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_id = "123456abcdef";
        let root = temp.path().join("decune");
        let linked_root = temp.path().join("linked-root");
        fs::create_dir_all(root.join(format!("{workspace_id}-ports"))).unwrap();
        unix_fs::symlink(&root, &linked_root).unwrap();
        let mut workspace_ids = BTreeSet::new();

        collect_workspace_ids_from_port_status_dirs(&mut workspace_ids, &linked_root).unwrap();

        assert!(workspace_ids.is_empty());
    }

    #[test]
    fn feature_cache_is_additive_and_uses_its_own_target() {
        let roots = TestRoots::new();
        let cache = roots.cache_home.path().join("decune/features");
        fs::create_dir_all(&cache).unwrap();
        fs::write(cache.join("archive.tgz"), "archive\n").unwrap();
        let _env = roots.apply();

        let mut report = CleanReport {
            dry_run: false,
            include_feature_cache: true,
            summary: CleanSummary::default(),
            targets: vec![CleanTargetReport::FeatureCache(
                feature_cache_clean_target().unwrap(),
            )],
        };
        report.summary = summarize_targets(&report.targets);

        apply_clean_report_for_test(&mut report, &BTreeSet::new()).unwrap();

        assert!(!cache.exists());
        assert_eq!(report.summary.removed, 1);
    }

    #[test]
    fn feature_cache_removes_non_utf8_cache_path() {
        let temp = tempfile::tempdir().unwrap();
        let cache_home = temp
            .path()
            .join(PathBuf::from(OsString::from_vec(b"cache-\xff".to_vec())));
        let state_home = temp.path().join("state-home");
        let runtime_home = temp.path().join("runtime-home");
        let cache = cache_home.join("decune/features");
        fs::create_dir_all(&cache).unwrap();
        fs::write(cache.join("archive.tgz"), "archive\n").unwrap();
        let _env = apply_env(&state_home, &cache_home, &runtime_home);

        let mut report = CleanReport {
            dry_run: false,
            include_feature_cache: true,
            summary: CleanSummary::default(),
            targets: vec![CleanTargetReport::FeatureCache(
                feature_cache_clean_target().unwrap(),
            )],
        };
        report.summary = summarize_targets(&report.targets);

        apply_clean_report_for_test(&mut report, &BTreeSet::new()).unwrap();

        assert!(!cache.exists());
        assert_eq!(report.summary.removed, 1);
    }

    #[test]
    fn confirmation_rejects_non_interactive_without_no_confirm() {
        let error = ensure_clean_confirmed(CleanConfirmation {
            no_confirm: false,
            stdin_is_terminal: false,
            has_targets: true,
        })
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Cannot confirm clean in a non-interactive terminal")
        );
        assert!(
            ensure_clean_confirmed(CleanConfirmation {
                no_confirm: true,
                stdin_is_terminal: false,
                has_targets: true,
            })
            .is_ok()
        );
        assert!(
            ensure_clean_confirmed(CleanConfirmation {
                no_confirm: false,
                stdin_is_terminal: false,
                has_targets: false,
            })
            .is_ok()
        );
    }

    struct TestRoots {
        state_home: tempfile::TempDir,
        cache_home: tempfile::TempDir,
        runtime_home: tempfile::TempDir,
    }

    impl TestRoots {
        fn new() -> Self {
            Self {
                state_home: tempfile::tempdir().unwrap(),
                cache_home: tempfile::tempdir().unwrap(),
                runtime_home: tempfile::tempdir().unwrap(),
            }
        }

        fn apply(&self) -> EnvGuard {
            apply_env(
                self.state_home.path(),
                self.cache_home.path(),
                self.runtime_home.path(),
            )
        }
    }

    struct EnvGuard {
        _guard: MutexGuard<'static, ()>,
        original: [(&'static str, Option<OsString>); 3],
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, value) in &self.original {
                unsafe {
                    match value {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
    }

    fn env_mutex() -> &'static Mutex<()> {
        static ENV_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
        ENV_MUTEX.get_or_init(|| Mutex::new(()))
    }

    fn apply_env(state_home: &Path, cache_home: &Path, runtime_home: &Path) -> EnvGuard {
        let guard = env_mutex().lock().unwrap();
        let original = [
            ("XDG_STATE_HOME", std::env::var_os("XDG_STATE_HOME")),
            ("XDG_CACHE_HOME", std::env::var_os("XDG_CACHE_HOME")),
            ("XDG_RUNTIME_DIR", std::env::var_os("XDG_RUNTIME_DIR")),
        ];
        set_env("XDG_STATE_HOME", state_home);
        set_env("XDG_CACHE_HOME", cache_home);
        set_env("XDG_RUNTIME_DIR", runtime_home);
        EnvGuard {
            _guard: guard,
            original,
        }
    }

    fn set_env(key: &str, value: &Path) {
        unsafe {
            std::env::set_var(key, value);
        }
    }

    fn apply_clean_report_for_test(
        report: &mut CleanReport,
        managed_workspace_ids: &BTreeSet<String>,
    ) -> Result<()> {
        for target in &mut report.targets {
            match target {
                CleanTargetReport::Workspace(target) if target.action == CleanAction::Remove => {
                    let refreshed =
                        workspace_clean_target(&target.workspace_id, false, managed_workspace_ids)?;
                    apply_workspace_clean_target(target, refreshed)?;
                }
                CleanTargetReport::FeatureCache(target) if target.action == CleanAction::Remove => {
                    let _lock = FeatureCacheLock::acquire_exclusive(&target.removal_path)?;
                    remove_dir_if_exists(&target.removal_path)?;
                    target.removed = true;
                }
                _ => {}
            }
        }
        report.summary = summarize_targets(&report.targets);
        Ok(())
    }
}
