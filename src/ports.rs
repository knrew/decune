use std::{fmt::Write as _, path::PathBuf};

use anyhow::{Context, Result};

use crate::{
    host::forward::{ActiveForwardPort, forward_status_dir, list_active_forward_status_ports},
    ui,
    workspace::Workspace,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PortsOptions {
    pub(crate) workspace: PathBuf,
    pub(crate) json: bool,
}

pub(crate) async fn run_ports(options: PortsOptions) -> Result<()> {
    let workspace = Workspace::resolve(&options.workspace)?;
    let status_dir = forward_status_dir(workspace.paths().runtime_dir());
    let mut status = list_active_forward_status_ports(status_dir).await?;
    for warning in status.warnings {
        ui::warn(&warning);
    }
    sort_ports(&mut status.ports);

    if options.json {
        let output = serde_json::to_string_pretty(&status.ports)
            .context("Failed to serialize active forwarded ports")?;
        println!("{output}");
    } else {
        print!("{}", render_ports_table(&status.ports));
    }

    Ok(())
}

fn sort_ports(ports: &mut [ActiveForwardPort]) {
    ports.sort_by(|left, right| {
        (
            &left.host_ip,
            left.host_port,
            left.service.as_deref(),
            left.container_port,
            &left.protocol,
            left.source,
            left.label.as_deref(),
        )
            .cmp(&(
                &right.host_ip,
                right.host_port,
                right.service.as_deref(),
                right.container_port,
                &right.protocol,
                right.source,
                right.label.as_deref(),
            ))
    });
}

fn render_ports_table(ports: &[ActiveForwardPort]) -> String {
    if ports.is_empty() {
        return "No active forwarded ports\n".to_owned();
    }

    let headers = ["LOCAL", "TARGET", "SOURCE", "REQUESTED", "LABEL"];
    let rows = ports.iter().map(port_row).collect::<Vec<_>>();
    let mut widths = headers.map(str::len);
    for row in &rows {
        widths[0] = widths[0].max(row.local.len());
        widths[1] = widths[1].max(row.target.len());
        widths[2] = widths[2].max(row.source.len());
        widths[3] = widths[3].max(row.requested.len());
        widths[4] = widths[4].max(row.label.len());
    }

    let mut output = String::new();
    write_row(&mut output, headers, widths);
    for row in &rows {
        write_row(
            &mut output,
            [
                row.local.as_str(),
                row.target.as_str(),
                row.source.as_str(),
                row.requested.as_str(),
                row.label.as_str(),
            ],
            widths,
        );
    }
    output
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PortRow {
    local: String,
    target: String,
    source: String,
    requested: String,
    label: String,
}

fn port_row(port: &ActiveForwardPort) -> PortRow {
    PortRow {
        local: format_endpoint(&port.host_ip, port.host_port),
        target: format_target(port),
        source: port.source.as_str().to_owned(),
        requested: format_requested(port),
        label: port
            .label
            .as_deref()
            .filter(|label| !label.is_empty())
            .unwrap_or("-")
            .to_owned(),
    }
}

fn write_row(output: &mut String, columns: [&str; 5], widths: [usize; 5]) {
    let _ = writeln!(
        output,
        "{:<width0$}  {:<width1$}  {:<width2$}  {:<width3$}  {:<width4$}",
        columns[0],
        columns[1],
        columns[2],
        columns[3],
        columns[4],
        width0 = widths[0],
        width1 = widths[1],
        width2 = widths[2],
        width3 = widths[3],
        width4 = widths[4],
    );
}

fn format_requested(port: &ActiveForwardPort) -> String {
    if port.host_port == port.requested_host_port {
        "-".to_owned()
    } else {
        format_endpoint(&port.host_ip, port.requested_host_port)
    }
}

fn format_target(port: &ActiveForwardPort) -> String {
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

    fn port(host_port: u16, requested_host_port: u16) -> ActiveForwardPort {
        ActiveForwardPort {
            host_ip: "127.0.0.1".to_owned(),
            host_port,
            requested_host_port,
            service: None,
            container_port: 3000,
            protocol: "tcp".to_owned(),
            source: ForwardStatusSource::Configured,
            label: Some("web".to_owned()),
        }
    }

    #[test]
    fn renders_no_active_ports() {
        assert_eq!(render_ports_table(&[]), "No active forwarded ports\n");
    }

    #[test]
    fn renders_active_ports_table() {
        let mut ports = vec![port(3001, 3000)];
        ports[0].service = Some("app".to_owned());
        ports[0].source = ForwardStatusSource::Auto;

        let table = render_ports_table(&ports);

        assert!(table.contains("LOCAL"));
        assert!(table.contains("127.0.0.1:3001"));
        assert!(table.contains("app:3000/tcp"));
        assert!(table.contains("auto"));
        assert!(table.contains("127.0.0.1:3000"));
        assert!(table.contains("web"));
    }

    #[test]
    fn formats_ipv6_endpoints_with_brackets() {
        assert_eq!(format_endpoint("::1", 3000), "[::1]:3000");
        assert_eq!(format_endpoint("[::1]", 3000), "[::1]:3000");
    }
}
