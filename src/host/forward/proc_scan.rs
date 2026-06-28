use std::{
    collections::BTreeSet,
    fs, io,
    net::{Ipv4Addr, Ipv6Addr},
    path::Path,
};

use anyhow::{Context, Result, bail};
use decune_container_protocol::ForwardAgentScanRequest;

pub(super) fn detect_listen_ports(scan: &ForwardAgentScanRequest) -> Result<Vec<u16>> {
    detect_listen_ports_from_proc_paths(
        scan,
        Path::new("/proc/net/tcp"),
        Path::new("/proc/net/tcp6"),
        Path::new("/proc/sys/net/ipv6/bindv6only"),
    )
}

fn detect_listen_ports_from_proc_paths(
    scan: &ForwardAgentScanRequest,
    tcp_path: &Path,
    tcp6_path: &Path,
    bindv6only_path: &Path,
) -> Result<Vec<u16>> {
    let tcp = read_required_proc_file(tcp_path)?;
    let tcp6 = read_proc_file(tcp6_path)?.unwrap_or_default();
    let tcp6_dual_stack =
        read_ipv6_bindv6only(bindv6only_path)?.is_some_and(|bindv6only| !bindv6only);
    listen_ports_from_proc_contents(
        tcp.as_str(),
        tcp6.as_str(),
        tcp6_dual_stack,
        scan.min,
        scan.max,
        &scan.ignore,
    )
}

fn read_proc_file(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| {
            format!(
                "Failed to read {} for automatic port forwarding",
                path.display()
            )
        }),
    }
}

fn read_required_proc_file(path: &Path) -> Result<String> {
    read_proc_file(path)?.ok_or_else(|| {
        anyhow::anyhow!(
            "Failed to read {} for automatic port forwarding: file does not exist",
            path.display()
        )
    })
}

fn read_ipv6_bindv6only(path: &Path) -> Result<Option<bool>> {
    let Some(value) = read_proc_file(path)? else {
        return Ok(None);
    };
    match value.trim() {
        "0" => Ok(Some(false)),
        "1" => Ok(Some(true)),
        value => bail!("Invalid /proc/sys/net/ipv6/bindv6only value: {value}"),
    }
}

fn listen_ports_from_proc_contents(
    tcp_content: &str,
    tcp6_content: &str,
    tcp6_dual_stack: bool,
    min: u16,
    max: u16,
    ignore: &[u16],
) -> Result<Vec<u16>> {
    let ignored = ignore.iter().copied().collect::<BTreeSet<_>>();
    let mut ports = BTreeSet::new();

    for line in tcp_content.lines().skip(1) {
        let Some(port) = parse_proc_net_tcp_listen_port(line)? else {
            continue;
        };
        if port >= min && port < max && !ignored.contains(&port) {
            ports.insert(port);
        }
    }
    for line in tcp6_content.lines().skip(1) {
        let Some(port) = parse_proc_net_tcp6_listen_port(line, tcp6_dual_stack)? else {
            continue;
        };
        if port >= min && port < max && !ignored.contains(&port) {
            ports.insert(port);
        }
    }

    Ok(ports.into_iter().collect())
}

fn parse_proc_net_tcp_listen_port(line: &str) -> Result<Option<u16>> {
    let fields = line.split_whitespace().collect::<Vec<_>>();
    if fields.len() < 4 || fields[3] != "0A" {
        return Ok(None);
    }
    let local_address = fields[1];
    let (address_hex, port_hex) = local_address
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("Invalid /proc/net/tcp local address: {local_address}"))?;
    if !proc_net_tcp_address_is_ipv4_reachable(address_hex)? {
        return Ok(None);
    }
    let port = u16::from_str_radix(port_hex, 16)
        .with_context(|| format!("Invalid /proc/net/tcp local port: {port_hex}"))?;

    Ok(Some(port))
}

fn proc_net_tcp_address_is_ipv4_reachable(address_hex: &str) -> Result<bool> {
    let address = parse_proc_net_tcp_address(address_hex)?;

    Ok(address == Ipv4Addr::LOCALHOST || address.is_unspecified())
}

fn parse_proc_net_tcp_address(address_hex: &str) -> Result<Ipv4Addr> {
    if address_hex.len() != 8 {
        bail!("Invalid /proc/net/tcp local address: {address_hex}");
    }
    let address = u32::from_str_radix(address_hex, 16)
        .with_context(|| format!("Invalid /proc/net/tcp local address: {address_hex}"))?;

    Ok(Ipv4Addr::from(address.to_le_bytes()))
}

fn parse_proc_net_tcp6_listen_port(line: &str, tcp6_dual_stack: bool) -> Result<Option<u16>> {
    let fields = line.split_whitespace().collect::<Vec<_>>();
    if fields.len() < 4 || fields[3] != "0A" {
        return Ok(None);
    }
    let local_address = fields[1];
    let (address_hex, port_hex) = local_address
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("Invalid /proc/net/tcp6 local address: {local_address}"))?;
    if !proc_net_tcp6_address_is_ipv4_reachable(address_hex, tcp6_dual_stack)? {
        return Ok(None);
    }
    let port = u16::from_str_radix(port_hex, 16)
        .with_context(|| format!("Invalid /proc/net/tcp6 local port: {port_hex}"))?;

    Ok(Some(port))
}

fn proc_net_tcp6_address_is_ipv4_reachable(
    address_hex: &str,
    tcp6_dual_stack: bool,
) -> Result<bool> {
    let address = parse_proc_net_tcp6_address(address_hex)?;
    let ipv6 = Ipv6Addr::from(address);
    if ipv6.is_unspecified() {
        return Ok(tcp6_dual_stack);
    }
    let Some(ipv4) = ipv6.to_ipv4_mapped() else {
        return Ok(false);
    };

    Ok(ipv4 == Ipv4Addr::LOCALHOST || ipv4.is_unspecified())
}

fn parse_proc_net_tcp6_address(address_hex: &str) -> Result<[u8; 16]> {
    if address_hex.len() != 32 {
        bail!("Invalid /proc/net/tcp6 local address: {address_hex}");
    }
    let mut address = [0u8; 16];
    for chunk in 0..4 {
        let start = chunk * 8;
        let value = u32::from_str_radix(&address_hex[start..start + 8], 16)
            .with_context(|| format!("Invalid /proc/net/tcp6 local address: {address_hex}"))?;
        address[chunk * 4..chunk * 4 + 4].copy_from_slice(&value.to_le_bytes());
    }
    Ok(address)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use decune_container_protocol::ForwardAgentScanRequest;
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn proc_net_tcp_parser_detects_listen_ports_from_tcp_and_tcp6() {
        let tcp = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:10E1 00000000:0000 0A 00000000:00000000 00:00000000 00000000 0 0 1 1 0000000000000000 100 0 0 10 0
   1: 0100007F:10E2 00000000:0000 01 00000000:00000000 00:00000000 00000000 0 0 2 1 0000000000000000 100 0 0 10 0
";
        let tcp6 = "\
  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0000000000000000FFFF00000100007F:10E3 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000 0 0 3 1 0000000000000000 100 0 0 10 0
";

        let ports = listen_ports_from_proc_contents(tcp, tcp6, true, 4321, 4324, &[4323]).unwrap();

        assert_eq!(ports, vec![4321]);
    }

    #[test]
    fn detect_listen_ports_continues_when_ipv6_proc_files_are_missing() {
        let temp = TempDir::new().unwrap();
        let net_dir = temp.path().join("net");
        fs::create_dir(&net_dir).unwrap();
        let tcp_path = net_dir.join("tcp");
        fs::write(
            &tcp_path,
            "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:10E1 00000000:0000 0A 00000000:00000000 00:00000000 00000000 0 0 1 1 0000000000000000 100 0 0 10 0
",
        )
        .unwrap();
        let scan = ForwardAgentScanRequest {
            min: 4321,
            max: 4322,
            ignore: Vec::new(),
        };

        let ports = detect_listen_ports_from_proc_paths(
            &scan,
            &tcp_path,
            &net_dir.join("tcp6"),
            &temp.path().join("sys/net/ipv6/bindv6only"),
        )
        .unwrap();

        assert_eq!(ports, vec![4321]);
    }

    #[test]
    fn proc_net_tcp_parser_ignores_ipv4_addresses_unreachable_from_localhost() {
        let tcp = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000:10E1 00000000:0000 0A 00000000:00000000 00:00000000 00000000 0 0 1 1 0000000000000000 100 0 0 10 0
   1: 0100007F:10E2 00000000:0000 0A 00000000:00000000 00:00000000 00000000 0 0 2 1 0000000000000000 100 0 0 10 0
   2: 0200007F:10E3 00000000:0000 0A 00000000:00000000 00:00000000 00000000 0 0 3 1 0000000000000000 100 0 0 10 0
   3: 020012AC:10E4 00000000:0000 0A 00000000:00000000 00:00000000 00000000 0 0 4 1 0000000000000000 100 0 0 10 0
";

        let ports = listen_ports_from_proc_contents(tcp, "", true, 4321, 4325, &[]).unwrap();

        assert_eq!(ports, vec![4321, 4322]);
    }

    #[test]
    fn proc_net_tcp6_parser_ignores_ipv6_only_listen_ports() {
        let tcp6 = "\
  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000000000000000000001000000:10E1 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000 0 0 3 1 0000000000000000 100 0 0 10 0
";

        let ports = listen_ports_from_proc_contents("", tcp6, true, 4321, 4322, &[]).unwrap();

        assert!(ports.is_empty());
    }

    #[test]
    fn proc_net_tcp6_parser_detects_ipv4_mapped_loopback_listen_ports() {
        let tcp6 = "\
  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0000000000000000FFFF00000100007F:10E1 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000 0 0 3 1 0000000000000000 100 0 0 10 0
   1: 0000000000000000FFFF00000200007F:10E2 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000 0 0 4 1 0000000000000000 100 0 0 10 0
";

        let ports = listen_ports_from_proc_contents("", tcp6, true, 4321, 4323, &[]).unwrap();

        assert_eq!(ports, vec![4321]);
    }

    #[test]
    fn proc_net_tcp6_parser_detects_dual_stack_unspecified_listen_ports() {
        let tcp6 = "\
  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000000000000000000000000000:10E1 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000 0 0 3 1 0000000000000000 100 0 0 10 0
";

        let ports = listen_ports_from_proc_contents("", tcp6, true, 4321, 4322, &[]).unwrap();

        assert_eq!(ports, vec![4321]);
    }

    #[test]
    fn proc_net_tcp6_parser_ignores_unspecified_listen_ports_when_ipv6_only() {
        let tcp6 = "\
  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000000000000000000000000000:10E1 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000 0 0 3 1 0000000000000000 100 0 0 10 0
";

        let ports = listen_ports_from_proc_contents("", tcp6, false, 4321, 4322, &[]).unwrap();

        assert!(ports.is_empty());
    }
}
