use std::{
    fmt::Write as _,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::ports::{PortInventory, PortInventoryEntry, PortUsageType, render_ports_table};

use super::types::{
    EnvironmentStatus, LifecycleStatus, StatusInventory, WorkspaceMode, WorkspaceStatus,
};

pub(super) fn render_status_summary(
    inventory: &StatusInventory,
    port_inventory: &PortInventory,
) -> String {
    if inventory.workspaces.is_empty() {
        return "No decune-managed workspace environments found\n".to_owned();
    }

    let mut workspaces = inventory.workspaces.iter().collect::<Vec<_>>();
    sort_workspaces_for_display(&mut workspaces);

    let mut output = String::new();
    let running = workspaces
        .iter()
        .filter(|workspace| workspace.environment_status == EnvironmentStatus::Running)
        .count();
    let stopped = workspaces
        .iter()
        .filter(|workspace| workspace.environment_status == EnvironmentStatus::Stopped)
        .count();
    let with_issues = workspaces
        .iter()
        .filter(|workspace| !workspace.issues.is_empty())
        .count();
    let _ = writeln!(
        output,
        "Found {} decune-managed workspace environments ({} running, {} stopped, {} with issues)",
        workspaces.len(),
        running,
        stopped,
        with_issues
    );

    let headers = [
        "ID",
        "WORKSPACE",
        "RUNTIME",
        "CONFIG",
        "HEALTH",
        "FWD/PUB",
        "ISSUES",
        "LAST_USED",
    ];
    let rows = workspaces
        .iter()
        .map(|workspace| summary_row(workspace, port_inventory))
        .collect::<Vec<_>>();
    let mut widths = headers
        .iter()
        .map(|header| header.len())
        .collect::<Vec<_>>();
    for row in &rows {
        for (index, column) in row.iter().enumerate() {
            widths[index] = widths[index].max(column.len());
        }
    }
    write_columns(&mut output, &headers, &widths);
    for row in rows {
        let refs = row.iter().map(String::as_str).collect::<Vec<_>>();
        write_columns(&mut output, &refs, &widths);
    }

    output
}

pub(super) fn render_workspace_detail(
    status: &WorkspaceStatus,
    ports: &[PortInventoryEntry],
) -> String {
    let mut output = String::new();
    let _ = writeln!(
        output,
        "Workspace: {}",
        status.workspace_path.as_deref().unwrap_or("<unknown>")
    );
    let _ = writeln!(output, "ID: {}", status.workspace_id);
    let _ = writeln!(output, "Mode: {}", status.mode.as_str());
    output.push('\n');

    output.push_str("Summary\n");
    let _ = writeln!(output, "  Runtime: {}", status.environment_status.as_str());
    let _ = writeln!(output, "  Config: {}", status.config_status.as_str());
    let _ = writeln!(output, "  Health: {}", status.health_status.as_str());
    let _ = writeln!(output, "  Containers: {}", status.containers.len());
    let _ = writeln!(output, "  Volumes: {}", status.volumes.len());
    let _ = writeln!(
        output,
        "  Last used: {}",
        format_timestamp(status.last_used_at.as_deref())
    );
    output.push('\n');

    output.push_str("Config\n");
    let _ = writeln!(
        output,
        "  File: {}",
        status.config_file.as_deref().unwrap_or("-")
    );
    let _ = writeln!(
        output,
        "  Created: {}",
        status.created_at.as_deref().unwrap_or("-")
    );
    let _ = writeln!(
        output,
        "  Last started: {}",
        status.last_started_at.as_deref().unwrap_or("-")
    );
    output.push('\n');

    if !status.issues.is_empty() {
        output.push_str("Issues\n");
        for issue in &status.issues {
            let _ = writeln!(
                output,
                "  {} [{}]: {}",
                issue.code,
                issue.severity.as_str(),
                issue.message
            );
        }
        output.push('\n');
    }

    if status.mode == WorkspaceMode::Compose {
        output.push_str("Services\n");
        let services = compose_services(status);
        if services.is_empty() {
            output.push_str("  -\n");
        } else {
            for service in services {
                let _ = writeln!(output, "  {service}");
            }
        }
        output.push('\n');
    }

    output.push_str("Runtime\n");
    if status.containers.is_empty() {
        output.push_str("  No containers\n");
    } else {
        for container in &status.containers {
            let name = container
                .name
                .as_deref()
                .or(container.id.as_deref())
                .unwrap_or("<unknown>");
            let service = container.service.as_deref().unwrap_or("-");
            let _ = writeln!(
                output,
                "  {}  service={}  state={}  health={}",
                name.trim_start_matches('/'),
                service,
                container.run_state.as_str(),
                container.health_status.as_str()
            );
        }
    }
    output.push('\n');

    output.push_str("Ports\n");
    for line in render_ports_table(ports, false).lines() {
        let _ = writeln!(output, "  {line}");
    }
    output.push('\n');

    output.push_str("Resources\n");
    let _ = writeln!(output, "  Containers: {}", status.containers.len());
    let _ = writeln!(output, "  Volumes: {}", status.volumes.len());
    output.push('\n');

    if status.lifecycle_status == LifecycleStatus::Incomplete {
        output.push_str("Lifecycle\n");
        if let Some(lifecycle) = status.lifecycle {
            let _ = writeln!(
                output,
                "  onCreateCommand: {}",
                completion(
                    lifecycle.on_create_completed,
                    lifecycle.after_on_create_completed
                )
            );
            let _ = writeln!(
                output,
                "  updateContentCommand: {}",
                completion(
                    lifecycle.update_content_completed,
                    lifecycle.after_update_content_completed
                )
            );
            let _ = writeln!(
                output,
                "  postCreateCommand: {}",
                completion(
                    lifecycle.post_create_completed,
                    lifecycle.after_post_create_completed
                )
            );
        } else {
            output.push_str("  unknown\n");
        }
        output.push('\n');
    }

    let actions = status
        .issues
        .iter()
        .filter_map(|issue| issue.action.as_deref().map(|action| (issue.code, action)))
        .collect::<Vec<_>>();
    if !actions.is_empty() {
        output.push_str("Action\n");
        for (code, action) in actions {
            let _ = writeln!(output, "  {code}: {action}");
        }
    }

    output
}

fn sort_workspaces_for_display(workspaces: &mut [&WorkspaceStatus]) {
    workspaces.sort_by(|left, right| {
        (
            left.workspace_path.as_deref().unwrap_or("\u{10ffff}"),
            left.workspace_id.as_str(),
        )
            .cmp(&(
                right.workspace_path.as_deref().unwrap_or("\u{10ffff}"),
                right.workspace_id.as_str(),
            ))
    });
}

fn summary_row(workspace: &WorkspaceStatus, port_inventory: &PortInventory) -> Vec<String> {
    let (forwarded, published) = port_counts(&workspace.workspace_id, &port_inventory.ports);
    vec![
        workspace.workspace_id.clone(),
        workspace
            .workspace_path
            .as_deref()
            .unwrap_or("<unknown>")
            .to_owned(),
        workspace.environment_status.as_str().to_owned(),
        workspace.config_status.as_str().to_owned(),
        workspace.health_status.as_str().to_owned(),
        format!("{forwarded}/{published}"),
        workspace.issues.len().to_string(),
        format_timestamp(workspace.last_used_at.as_deref()),
    ]
}

fn port_counts(workspace_id: &str, ports: &[PortInventoryEntry]) -> (usize, usize) {
    let mut forwarded = 0;
    let mut published = 0;
    for port in ports
        .iter()
        .filter(|port| port.workspace_id.as_deref() == Some(workspace_id))
    {
        match port.kind {
            PortUsageType::Forwarded => forwarded += 1,
            PortUsageType::Published => published += 1,
        }
    }
    (forwarded, published)
}

fn write_columns(output: &mut String, columns: &[&str], widths: &[usize]) {
    for (index, column) in columns.iter().enumerate() {
        if index > 0 {
            output.push_str("  ");
        }
        let _ = write!(output, "{:<width$}", column, width = widths[index]);
    }
    output.push('\n');
}

fn format_timestamp(value: Option<&str>) -> String {
    let Some(value) = value else {
        return "-".to_owned();
    };
    let Some(seconds) = value
        .strip_prefix("unix:")
        .and_then(|value| value.parse::<u64>().ok())
    else {
        return "-".to_owned();
    };
    let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return "-".to_owned();
    };
    let now = now.as_secs();
    if seconds > now {
        return "-".to_owned();
    }
    let elapsed = now - seconds;
    match elapsed {
        0..=59 => format!("{elapsed}s ago"),
        60..=3_599 => format!("{}m ago", elapsed / 60),
        3_600..=86_399 => format!("{}h ago", elapsed / 3_600),
        _ => format!("{}d ago", elapsed / 86_400),
    }
}

pub(super) fn compose_services(status: &WorkspaceStatus) -> Vec<String> {
    let mut services = status
        .containers
        .iter()
        .filter_map(|container| container.service.clone())
        .collect::<Vec<_>>();
    services.sort();
    services.dedup();
    services
}

const fn completion(command: bool, after_hook: bool) -> &'static str {
    match (command, after_hook) {
        (true, true) => "complete",
        (true, false) => "after-hook-pending",
        (false, _) => "pending",
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        ports::PortInventory,
        state::LifecycleState,
        status::types::{
            ConfigStatus, EnvironmentStatus, HealthStatus, LifecycleStatus, StatusInventory,
            StatusIssue, StatusIssueSeverity, WorkspaceStatus,
        },
    };

    use super::*;

    const WORKSPACE_ID: &str = "123456abcdef";
    #[test]
    fn summary_renderer_reports_empty_inventory() {
        let output = render_status_summary(
            &StatusInventory {
                workspaces: Vec::new(),
                issues: Vec::new(),
            },
            &PortInventory::default(),
        );

        assert_eq!(output, "No decune-managed workspace environments found\n");
    }
    #[test]
    fn summary_renderer_sorts_paths_and_does_not_fallback_last_used() {
        let mut alpha = rendered_status("bbbbbbbbbbbb", Some("/alpha"));
        alpha.created_at = Some("unix:1".to_owned());
        alpha.last_started_at = Some("unix:2".to_owned());
        let beta = rendered_status("aaaaaaaaaaaa", Some("/beta"));
        let unknown = rendered_status("cccccccccccc", None);
        let output = render_status_summary(
            &StatusInventory {
                workspaces: vec![unknown, beta, alpha],
                issues: Vec::new(),
            },
            &PortInventory::default(),
        );

        let alpha_index = output.find("/alpha").unwrap();
        let beta_index = output.find("/beta").unwrap();
        let unknown_index = output.find("<unknown>").unwrap();
        assert!(alpha_index < beta_index);
        assert!(beta_index < unknown_index);
        assert!(output.contains("LAST_USED"));
        assert!(output.lines().any(|line| {
            line.contains("bbbbbbbbbbbb") && line.split_whitespace().last() == Some("-")
        }));
    }
    #[test]
    fn detail_renderer_reports_not_created_and_omits_complete_lifecycle() {
        let mut status = rendered_status(WORKSPACE_ID, Some("/workspace"));
        status.mode = WorkspaceMode::Image;
        status.environment_status = EnvironmentStatus::NotCreated;
        status.lifecycle_status = LifecycleStatus::Complete;
        status.lifecycle = Some(LifecycleState::all_completed());
        status.issues.push(issue(
            "not-created",
            StatusIssueSeverity::Info,
            "No decune-managed environment exists for this workspace yet.",
            Some("Run decune up to create the environment."),
        ));

        let output = render_workspace_detail(&status, &[]);

        assert!(output.contains("Runtime: not-created"));
        assert!(output.contains("No active ports for this workspace"));
        assert!(output.contains("Run decune up to create the environment."));
        assert!(!output.contains("Lifecycle\n"));
    }
    #[test]
    fn detail_renderer_reports_issue_codes_severities_and_all_actions() {
        let mut status = rendered_status(WORKSPACE_ID, Some("/workspace"));
        status.issues.push(issue(
            "config-unreadable",
            StatusIssueSeverity::Warning,
            "The current devcontainer configuration could not be read.",
            Some("Fix the configuration error, then retry."),
        ));
        status.issues.push(issue(
            "not-created",
            StatusIssueSeverity::Info,
            "No decune-managed environment exists for this workspace yet.",
            Some("Run decune up to create the environment."),
        ));

        let output = render_workspace_detail(&status, &[]);

        assert!(output.contains(
            "config-unreadable [warning]: The current devcontainer configuration could not be read."
        ));
        assert!(output.contains(
            "not-created [info]: No decune-managed environment exists for this workspace yet."
        ));
        assert!(output.contains("config-unreadable: Fix the configuration error, then retry."));
        assert!(output.contains("not-created: Run decune up to create the environment."));
    }
    #[test]
    fn renderers_do_not_include_sensitive_raw_values() {
        let mut status = rendered_status(WORKSPACE_ID, Some("/workspace"));
        status.config_file = Some("/workspace/.devcontainer/devcontainer.json".to_owned());
        status.issues.push(StatusIssue {
            code: "config-unreadable",
            severity: StatusIssueSeverity::Warning,
            message: "The current devcontainer configuration could not be read.".to_owned(),
            action: Some("Fix the configuration error, then retry.".to_owned()),
        });
        let output = render_workspace_detail(&status, &[]);

        assert!(!output.contains("secret-config-hash"));
        assert!(!output.contains("decune.config_hash"));
        assert!(!output.contains("TOKEN="));
        assert!(!output.contains("build.args"));
        assert!(!output.contains("raw-compose"));
    }

    fn issue(
        code: &'static str,
        severity: StatusIssueSeverity,
        message: &str,
        action: Option<&str>,
    ) -> StatusIssue {
        StatusIssue {
            code,
            severity,
            message: message.to_owned(),
            action: action.map(str::to_owned),
        }
    }
    fn rendered_status(workspace_id: &str, workspace_path: Option<&str>) -> WorkspaceStatus {
        WorkspaceStatus {
            workspace_id: workspace_id.to_owned(),
            workspace_path: workspace_path.map(str::to_owned),
            mode: WorkspaceMode::Unknown,
            config_file: None,
            created_at: None,
            last_started_at: None,
            last_used_at: None,
            containers: Vec::new(),
            volumes: Vec::new(),
            environment_status: EnvironmentStatus::Missing,
            config_status: ConfigStatus::Unknown,
            health_status: HealthStatus::Unknown,
            lifecycle_status: LifecycleStatus::Unknown,
            lifecycle: None,
            issues: Vec::new(),
        }
    }
}
