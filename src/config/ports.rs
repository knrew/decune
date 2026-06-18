#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PortSpecSegments<'a> {
    One {
        container: &'a str,
    },
    Two {
        left: &'a str,
        container: &'a str,
        bracketed_host_ip: bool,
    },
    Three {
        host_ip: &'a str,
        host: &'a str,
        container: &'a str,
        bracketed_host_ip: bool,
    },
}

pub(crate) fn split_port_spec(value: &str) -> Result<PortSpecSegments<'_>, String> {
    if value.is_empty() {
        return Err("port specification must not be empty".to_owned());
    }

    if value.starts_with('[') {
        return split_bracketed_host_port_spec(value);
    }

    if value.contains('[') || value.contains(']') {
        return Err(format!(
            "malformed bracketed host IP in port specification: {value}"
        ));
    }

    let segments = value.split(':').collect::<Vec<_>>();
    match segments.as_slice() {
        [container] => Ok(PortSpecSegments::One { container }),
        [left, container] => Ok(PortSpecSegments::Two {
            left,
            container,
            bracketed_host_ip: false,
        }),
        [host_ip, host, container] => Ok(PortSpecSegments::Three {
            host_ip,
            host,
            container,
            bracketed_host_ip: false,
        }),
        _ => Err(format!(
            "invalid port specification: too many ':' separators in {value}; IPv6 host IP must be enclosed in brackets"
        )),
    }
}

fn split_bracketed_host_port_spec(value: &str) -> Result<PortSpecSegments<'_>, String> {
    let close = value
        .find(']')
        .ok_or_else(|| format!("missing closing ']' in port specification: {value}"))?;
    let host_ip = &value[1..close];
    if host_ip.is_empty() {
        return Err(format!(
            "bracketed host IP must not be empty in port specification: {value}"
        ));
    }

    let after_bracket = &value[close + 1..];
    let Some(rest) = after_bracket.strip_prefix(':') else {
        return Err(format!(
            "bracketed host IP must be followed by ':' in port specification: {value}"
        ));
    };
    if rest.is_empty() {
        return Err(format!(
            "missing port after bracketed host IP in port specification: {value}"
        ));
    }
    if rest.contains('[') || rest.contains(']') {
        return Err(format!(
            "malformed bracketed host IP in port specification: {value}"
        ));
    }

    let segments = rest.split(':').collect::<Vec<_>>();
    match segments.as_slice() {
        [container] => Ok(PortSpecSegments::Two {
            left: host_ip,
            container,
            bracketed_host_ip: true,
        }),
        [host, container] => Ok(PortSpecSegments::Three {
            host_ip,
            host,
            container,
            bracketed_host_ip: true,
        }),
        _ => Err(format!(
            "invalid port specification: too many ':' separators after bracketed host IP in {value}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_bracketed_ipv6_port_specs() {
        assert_eq!(
            split_port_spec("[::1]:3000").unwrap(),
            PortSpecSegments::Two {
                left: "::1",
                container: "3000",
                bracketed_host_ip: true,
            }
        );
        assert_eq!(
            split_port_spec("[2001:db8::1]:8080:3000").unwrap(),
            PortSpecSegments::Three {
                host_ip: "2001:db8::1",
                host: "8080",
                container: "3000",
                bracketed_host_ip: true,
            }
        );
    }

    #[test]
    fn rejects_malformed_bracketed_port_specs() {
        for value in ["[::1:3000", "[]:3000", "[::1]", "[::1]:3000:3000:tcp"] {
            assert!(split_port_spec(value).is_err(), "{value}");
        }
    }

    #[test]
    fn rejects_unbracketed_ipv6_port_specs() {
        let error = split_port_spec("::1:8080:3000").unwrap_err();

        assert!(error.contains("IPv6 host IP must be enclosed in brackets"));
    }
}
