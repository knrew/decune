use std::fmt::Write as _;

use super::types::{PortInventoryEntry, PortUsageType};

pub(crate) fn sort_ports(ports: &mut [PortInventoryEntry]) {
    ports.sort_by(|left, right| {
        (
            left.workspace.as_deref().unwrap_or("\u{10ffff}"),
            left.workspace_id.as_deref().unwrap_or(""),
            &left.host_ip,
            left.host_port,
            left.kind,
            left.service.as_deref(),
            left.container_port,
            &left.protocol,
            &left.source,
            left.label.as_deref(),
        )
            .cmp(&(
                right.workspace.as_deref().unwrap_or("\u{10ffff}"),
                right.workspace_id.as_deref().unwrap_or(""),
                &right.host_ip,
                right.host_port,
                right.kind,
                right.service.as_deref(),
                right.container_port,
                &right.protocol,
                &right.source,
                right.label.as_deref(),
            ))
    });
}

pub(crate) fn render_ports_table(ports: &[PortInventoryEntry], include_workspace: bool) -> String {
    if ports.is_empty() {
        return if include_workspace {
            "No active ports\n".to_owned()
        } else {
            "No active ports for this workspace\n".to_owned()
        };
    }

    let headers = if include_workspace {
        vec![
            "WORKSPACE",
            "ID",
            "LOCAL",
            "TYPE",
            "TARGET",
            "SOURCE",
            "REQUESTED",
            "STATE",
            "LABEL",
        ]
    } else {
        vec![
            "LOCAL",
            "TYPE",
            "TARGET",
            "SOURCE",
            "REQUESTED",
            "STATE",
            "LABEL",
        ]
    };
    let rows = ports.iter().map(port_row).collect::<Vec<_>>();
    let mut widths = headers
        .iter()
        .map(|header| header.len())
        .collect::<Vec<_>>();
    for row in &rows {
        let columns = row.columns(include_workspace);
        for (index, column) in columns.iter().enumerate() {
            widths[index] = widths[index].max(column.len());
        }
    }

    let mut output = String::new();
    write_row(&mut output, &headers, &widths);
    for row in &rows {
        write_row(&mut output, &row.columns(include_workspace), &widths);
    }
    output
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PortRow {
    workspace: String,
    workspace_id: String,
    local: String,
    kind: String,
    target: String,
    source: String,
    requested: String,
    state: String,
    label: String,
}

impl PortRow {
    fn columns(&self, include_workspace: bool) -> Vec<&str> {
        if include_workspace {
            vec![
                self.workspace.as_str(),
                self.workspace_id.as_str(),
                self.local.as_str(),
                self.kind.as_str(),
                self.target.as_str(),
                self.source.as_str(),
                self.requested.as_str(),
                self.state.as_str(),
                self.label.as_str(),
            ]
        } else {
            vec![
                self.local.as_str(),
                self.kind.as_str(),
                self.target.as_str(),
                self.source.as_str(),
                self.requested.as_str(),
                self.state.as_str(),
                self.label.as_str(),
            ]
        }
    }
}

fn port_row(port: &PortInventoryEntry) -> PortRow {
    PortRow {
        workspace: port.workspace.as_deref().unwrap_or("<unknown>").to_owned(),
        workspace_id: port.workspace_id.as_deref().unwrap_or("-").to_owned(),
        local: format_endpoint(&port.host_ip, port.host_port),
        kind: port.kind.as_str().to_owned(),
        target: format_target(port),
        source: port.source.clone(),
        requested: format_requested(port),
        state: format_port_state(port),
        label: port
            .label
            .as_deref()
            .filter(|label| !label.is_empty())
            .unwrap_or("-")
            .to_owned(),
    }
}

fn write_row(output: &mut String, columns: &[&str], widths: &[usize]) {
    for (index, column) in columns.iter().enumerate() {
        if index > 0 {
            output.push_str("  ");
        }
        let _ = write!(output, "{:<width$}", column, width = widths[index]);
    }
    output.push('\n');
}

pub(super) fn format_requested(port: &PortInventoryEntry) -> String {
    if port.kind == PortUsageType::Published && port.relocated != Some(true) {
        return "-".to_owned();
    }
    if port.requested_host_ip_kind.as_deref() == Some("omitted")
        && let Some(host_port) = port.requested_host_port
    {
        return format!("*:{host_port}");
    }
    match (&port.requested_host_ip, port.requested_host_port) {
        (Some(host_ip), Some(host_port)) => format_endpoint(host_ip, host_port),
        _ => "-".to_owned(),
    }
}

pub(super) fn format_port_state(port: &PortInventoryEntry) -> String {
    if port.kind == PortUsageType::Published && port.relocated == Some(true) {
        "relocated".to_owned()
    } else {
        "-".to_owned()
    }
}

pub(super) fn format_target(port: &PortInventoryEntry) -> String {
    let target = port.service.as_deref().unwrap_or("container");
    format!("{target}:{}/{}", port.container_port, port.protocol)
}

fn format_endpoint(host_ip: &str, port: u16) -> String {
    if host_ip.contains(':') && !host_ip.starts_with('[') {
        format!("[{host_ip}]:{port}")
    } else {
        format!("{host_ip}:{port}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::forward::ForwardStatusSource;

    fn port(host_port: u16, requested_host_port: u16) -> PortInventoryEntry {
        let requested = (host_port != requested_host_port)
            .then_some(("127.0.0.1".to_owned(), requested_host_port));
        PortInventoryEntry {
            workspace: None,
            workspace_id: None,
            host_ip: "127.0.0.1".to_owned(),
            host_port,
            kind: PortUsageType::Forwarded,
            service: None,
            container_port: 3000,
            protocol: "tcp".to_owned(),
            source: ForwardStatusSource::Configured.as_str().to_owned(),
            port_entry_index: None,
            target: None,
            requested: None,
            planned: None,
            actual_bindings: None,
            requested_host_ip_kind: None,
            requested_host_ip: requested.as_ref().map(|(host_ip, _)| host_ip.clone()),
            requested_host_port: requested.map(|(_, host_port)| host_port),
            planned_host_ip_kind: None,
            planned_host_ip: None,
            planned_host_port: None,
            relocated: None,
            label: Some("web".to_owned()),
        }
    }

    #[test]
    fn renders_no_active_ports() {
        assert_eq!(
            render_ports_table(&[], false),
            "No active ports for this workspace\n"
        );
        assert_eq!(render_ports_table(&[], true), "No active ports\n");
    }

    #[test]
    fn renders_active_ports_table() {
        let mut ports = vec![port(3001, 3000)];
        ports[0].service = Some("app".to_owned());
        ports[0].source = ForwardStatusSource::Auto.as_str().to_owned();

        let table = render_ports_table(&ports, false);

        assert!(table.contains("LOCAL"));
        assert!(table.contains("TYPE"));
        assert!(table.contains("127.0.0.1:3001"));
        assert!(table.contains("forwarded"));
        assert!(table.contains("app:3000/tcp"));
        assert!(table.contains("auto"));
        assert!(table.contains("127.0.0.1:3000"));
        assert!(table.contains("web"));
    }

    #[test]
    fn renders_all_ports_table_with_workspace_identity() {
        let mut ports = vec![port(3000, 3000)];
        ports[0].workspace = Some("/workspace".to_owned());
        ports[0].workspace_id = Some("123456abcdef".to_owned());

        let table = render_ports_table(&ports, true);

        assert!(table.contains("WORKSPACE"));
        assert!(table.contains("ID"));
        assert!(table.contains("/workspace"));
        assert!(table.contains("123456abcdef"));
    }

    #[test]
    fn formats_ipv6_endpoints_with_brackets() {
        assert_eq!(format_endpoint("::1", 3000), "[::1]:3000");
        assert_eq!(format_endpoint("[::1]", 3000), "[::1]:3000");
    }
}
