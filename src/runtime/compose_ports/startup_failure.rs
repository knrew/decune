use crate::runtime::compose_ports::{
    ComposePortEligibility, ComposePortEntry, ComposePortProtocol, ComposePublishedHostPort,
    ComposePublishedPortDiagnostic, ComposePublishedPortEndpoint, ComposePublishedPortHostIpKind,
    ComposePublishedPortPlanningInput, ComposePublishedPortStartupDiagnostics,
    compose_published_port_endpoint_display,
};

use super::{
    endpoint::{endpoint_for_entry, protocol_order},
    planning::ordered_eligible_port_entries,
};

pub(crate) fn classify_compose_published_port_startup_failure(
    stderr: &str,
    diagnostics: ComposePublishedPortStartupDiagnostics<'_>,
) -> Option<ComposePublishedPortDiagnostic> {
    if !compose_startup_error_mentions_bind_conflict(stderr) {
        return None;
    }

    if diagnostics.relocation_enabled {
        for entry in &diagnostics.plan.entries {
            if !compose_startup_error_mentions_endpoint_for_protocol(
                stderr,
                &entry.planned,
                &entry.protocol,
            ) {
                continue;
            }
            return Some(ComposePublishedPortDiagnostic::BindRace {
                service: entry.service.clone(),
                requested: entry.requested.clone(),
                planned: entry.planned.clone(),
                target_port: entry.target_port,
                protocol: entry.protocol.clone(),
            });
        }
        return classify_unsupported_compose_published_port_startup_failure(
            stderr,
            diagnostics.input,
        );
    }

    for entry in ordered_eligible_port_entries(diagnostics.input) {
        let requested = endpoint_for_entry(entry);
        if !compose_startup_error_mentions_endpoint_for_protocol(
            stderr,
            &requested,
            &entry.protocol,
        ) {
            continue;
        }
        return Some(ComposePublishedPortDiagnostic::Collision {
            service: entry.service.clone(),
            requested,
            target_port: entry
                .target_port
                .expect("eligible Compose published port entry has target port"),
            protocol: entry.protocol.clone(),
        });
    }

    classify_unsupported_compose_published_port_startup_failure(stderr, diagnostics.input)
}

fn classify_unsupported_compose_published_port_startup_failure(
    stderr: &str,
    input: &ComposePublishedPortPlanningInput,
) -> Option<ComposePublishedPortDiagnostic> {
    ordered_unsupported_published_port_entries(input)
        .into_iter()
        .find_map(|entry| {
            let target_port = entry.target_port?;
            let requested =
                unsupported_requested_endpoint_display_for_startup_failure(stderr, entry)?;
            Some(ComposePublishedPortDiagnostic::Unsupported {
                service: entry.service.clone(),
                port_entry_index: entry.entry_index,
                requested,
                target_port,
                protocol: entry.protocol.clone(),
                reason: entry.unsupported_reason.clone().unwrap_or_else(|| {
                    "Compose published port entry is not relocation-eligible".to_owned()
                }),
            })
        })
}

fn ordered_unsupported_published_port_entries(
    input: &ComposePublishedPortPlanningInput,
) -> Vec<&ComposePortEntry> {
    let mut entries = input
        .port_entries
        .iter()
        .filter(|entry| {
            entry.eligibility != ComposePortEligibility::EligibleFixedTcp
                && entry.eligibility != ComposePortEligibility::UnsupportedContainerOnly
                && entry.eligibility != ComposePortEligibility::UnsupportedInvalid
                && entry.target_port.is_some()
        })
        .collect::<Vec<_>>();
    let service_order = input.services.ordered_services_for_planning();
    entries.sort_by_key(|entry| {
        (
            service_order
                .iter()
                .position(|service| service == &entry.service)
                .unwrap_or(usize::MAX),
            entry.service.as_str(),
            entry.entry_index,
            protocol_order(&entry.protocol),
        )
    });
    entries
}

fn unsupported_requested_endpoint_display_for_startup_failure(
    stderr: &str,
    entry: &ComposePortEntry,
) -> Option<String> {
    let (host_ip_kind, host_ip_value) = endpoint_host_ip_for_entry(entry)?;
    match &entry.published_host_port {
        ComposePublishedHostPort::Single(host_port) => {
            let endpoint = ComposePublishedPortEndpoint {
                host_ip_kind,
                host_ip_value,
                host_port: *host_port,
            };
            compose_startup_error_mentions_endpoint_for_protocol(stderr, &endpoint, &entry.protocol)
                .then(|| compose_published_port_endpoint_display(&endpoint))
        }
        ComposePublishedHostPort::Range(range) => {
            let (start, end) = parse_published_host_port_range(range)?;
            (start..=end)
                .any(|host_port| {
                    let endpoint = ComposePublishedPortEndpoint {
                        host_ip_kind,
                        host_ip_value: host_ip_value.clone(),
                        host_port,
                    };
                    compose_startup_error_mentions_endpoint_for_protocol(
                        stderr,
                        &endpoint,
                        &entry.protocol,
                    )
                })
                .then(|| {
                    compose_published_port_range_endpoint_display(
                        host_ip_kind,
                        &host_ip_value,
                        range,
                    )
                })
        }
        ComposePublishedHostPort::None | ComposePublishedHostPort::Invalid(_) => None,
    }
}

fn endpoint_host_ip_for_entry(
    entry: &ComposePortEntry,
) -> Option<(ComposePublishedPortHostIpKind, Option<String>)> {
    match &entry.host_ip {
        crate::runtime::compose_ports::ComposePortHostIp::Omitted => {
            Some((ComposePublishedPortHostIpKind::Omitted, None))
        }
        crate::runtime::compose_ports::ComposePortHostIp::Explicit(value) => Some((
            ComposePublishedPortHostIpKind::Explicit,
            Some(value.clone()),
        )),
        crate::runtime::compose_ports::ComposePortHostIp::Invalid(_) => None,
    }
}

fn parse_published_host_port_range(range: &str) -> Option<(u16, u16)> {
    let (start, end) = range.split_once('-')?;
    let start = start.parse::<u16>().ok()?;
    let end = end.parse::<u16>().ok()?;
    if start == 0 || end == 0 || start > end {
        return None;
    }
    Some((start, end))
}

fn compose_published_port_range_endpoint_display(
    host_ip_kind: ComposePublishedPortHostIpKind,
    host_ip_value: &Option<String>,
    range: &str,
) -> String {
    match host_ip_kind {
        ComposePublishedPortHostIpKind::Omitted => {
            format!("<host_ip omitted>:{range}")
        }
        ComposePublishedPortHostIpKind::Explicit => {
            format!("{}:{range}", host_ip_value.as_deref().unwrap_or(""))
        }
    }
}

fn compose_startup_error_mentions_bind_conflict(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("bind")
        && (lower.contains("address already in use")
            || lower.contains("port is already allocated")
            || lower.contains("port is already in use")
            || lower.contains("ports are not available")
            || lower.contains("port is unavailable"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComposeStartupErrorProtocolHint {
    Tcp,
    Udp,
}

fn compose_startup_error_mentions_endpoint_for_protocol(
    stderr: &str,
    endpoint: &ComposePublishedPortEndpoint,
    protocol: &ComposePortProtocol,
) -> bool {
    if !compose_startup_error_mentions_endpoint(stderr, endpoint) {
        return false;
    }

    match compose_startup_error_protocol_hint(stderr) {
        Some(hint) => compose_port_protocol_matches_startup_error_hint(protocol, hint),
        None => true,
    }
}

fn compose_startup_error_protocol_hint(stderr: &str) -> Option<ComposeStartupErrorProtocolHint> {
    let lower = stderr.to_ascii_lowercase();
    let mentions_tcp = lower.contains("listen tcp");
    let mentions_udp = lower.contains("listen udp");

    match (mentions_tcp, mentions_udp) {
        (true, false) => Some(ComposeStartupErrorProtocolHint::Tcp),
        (false, true) => Some(ComposeStartupErrorProtocolHint::Udp),
        _ => None,
    }
}

fn compose_port_protocol_matches_startup_error_hint(
    protocol: &ComposePortProtocol,
    hint: ComposeStartupErrorProtocolHint,
) -> bool {
    matches!(
        (protocol, hint),
        (
            ComposePortProtocol::Tcp,
            ComposeStartupErrorProtocolHint::Tcp
        ) | (
            ComposePortProtocol::Udp,
            ComposeStartupErrorProtocolHint::Udp
        )
    )
}

fn compose_startup_error_mentions_endpoint(
    stderr: &str,
    endpoint: &ComposePublishedPortEndpoint,
) -> bool {
    let lower = stderr.to_ascii_lowercase();
    let port_token = format!(":{}", endpoint.host_port);
    if !contains_endpoint_token_with_port_boundary(&lower, &port_token) {
        return false;
    }

    match endpoint.host_ip_kind {
        ComposePublishedPortHostIpKind::Omitted => true,
        ComposePublishedPortHostIpKind::Explicit => endpoint
            .host_ip_value
            .as_deref()
            .map(|host_ip| {
                let host_ip = host_ip.to_ascii_lowercase();
                contains_endpoint_token_with_port_boundary(
                    &lower,
                    &format!("{host_ip}:{}", endpoint.host_port),
                ) || contains_endpoint_token_with_port_boundary(
                    &lower,
                    &format!("[{host_ip}]:{}", endpoint.host_port),
                )
            })
            .unwrap_or(false),
    }
}

fn contains_endpoint_token_with_port_boundary(haystack: &str, needle: &str) -> bool {
    haystack.match_indices(needle).any(|(index, _)| {
        haystack[index + needle.len()..]
            .chars()
            .next()
            .is_none_or(|next| !next.is_ascii_digit())
    })
}
#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::runtime::compose_ports::{
        COMPOSE_PUBLISHED_PORT_BIND_RACE, COMPOSE_PUBLISHED_PORT_COLLISION,
        COMPOSE_PUBLISHED_PORT_UNSUPPORTED, ComposePublishedPortPlan,
        ComposePublishedPortStartupDiagnostics,
        test_support::{plan_with_availability, planning_input},
    };

    #[test]
    fn startup_failure_classifier_reports_collision_when_relocation_is_disabled() {
        let input = planning_input(
            json!({
                "services": {
                    "app": {
                        "ports": [{"target": 3000, "published": "3000"}]
                    }
                }
            }),
            "app",
            &[],
        );
        let plan = ComposePublishedPortPlan::default();

        let diagnostic = classify_compose_published_port_startup_failure(
            "Error response from daemon: Bind for 0.0.0.0:3000 failed: port is already allocated",
            ComposePublishedPortStartupDiagnostics {
                input: &input,
                plan: &plan,
                relocation_enabled: false,
            },
        )
        .expect("bind conflict should be classified")
        .to_string();

        assert!(diagnostic.contains(COMPOSE_PUBLISHED_PORT_COLLISION));
        assert!(diagnostic.contains("service: `app`"));
        assert!(diagnostic.contains("<host_ip omitted>:3000"));
        assert!(diagnostic.contains("app:3000/tcp"));
        assert!(diagnostic.contains("not a decune forwarding listener"));
    }

    #[test]
    fn startup_failure_classifier_matches_collision_port_with_digit_boundary() {
        let input = planning_input(
            json!({
                "services": {
                    "app": {
                        "ports": [{"target": 80, "published": "80"}]
                    },
                    "web": {
                        "ports": [{"target": 8080, "published": "8080"}]
                    }
                }
            }),
            "app",
            &[],
        );
        let plan = ComposePublishedPortPlan::default();

        let diagnostic = classify_compose_published_port_startup_failure(
            "Error response from daemon: Bind for 0.0.0.0:8080 failed: port is already allocated",
            ComposePublishedPortStartupDiagnostics {
                input: &input,
                plan: &plan,
                relocation_enabled: false,
            },
        )
        .expect("bind conflict should be classified")
        .to_string();

        assert!(diagnostic.contains(COMPOSE_PUBLISHED_PORT_COLLISION));
        assert!(diagnostic.contains("service: `web`"));
        assert!(diagnostic.contains("<host_ip omitted>:8080"));
        assert!(!diagnostic.contains("service: `app`"));
        assert!(!diagnostic.contains("<host_ip omitted>:80;"));
    }

    #[test]
    fn startup_failure_classifier_reports_bind_race_when_planned_endpoint_fails() {
        let input = planning_input(
            json!({
                "services": {
                    "app": {
                        "ports": [{"host_ip": "127.0.0.1", "target": 3000, "published": "3000"}]
                    }
                }
            }),
            "app",
            &[],
        );
        let plan = plan_with_availability(&input, &[3000]);

        let diagnostic = classify_compose_published_port_startup_failure(
            "Ports are not available: listen tcp 127.0.0.1:3001: bind: address already in use",
            ComposePublishedPortStartupDiagnostics {
                input: &input,
                plan: &plan,
                relocation_enabled: true,
            },
        )
        .expect("planned bind conflict should be classified")
        .to_string();

        assert!(diagnostic.contains(COMPOSE_PUBLISHED_PORT_BIND_RACE));
        assert!(diagnostic.contains("requested: 127.0.0.1:3000"));
        assert!(diagnostic.contains("planned: 127.0.0.1:3001"));
        assert!(diagnostic.contains("app:3000/tcp"));
    }

    #[test]
    fn startup_failure_classifier_matches_bind_race_port_with_digit_boundary() {
        let input = planning_input(
            json!({
                "services": {
                    "app": {
                        "ports": [{"host_ip": "127.0.0.1", "target": 80, "published": "80"}]
                    },
                    "web": {
                        "ports": [{"host_ip": "127.0.0.1", "target": 8080, "published": "8080"}]
                    }
                }
            }),
            "app",
            &[],
        );
        let plan = plan_with_availability(&input, &[]);

        let diagnostic = classify_compose_published_port_startup_failure(
            "Ports are not available: listen tcp 127.0.0.1:8080: bind: address already in use",
            ComposePublishedPortStartupDiagnostics {
                input: &input,
                plan: &plan,
                relocation_enabled: true,
            },
        )
        .expect("planned bind conflict should be classified")
        .to_string();

        assert!(diagnostic.contains(COMPOSE_PUBLISHED_PORT_BIND_RACE));
        assert!(diagnostic.contains("service: `web`"));
        assert!(diagnostic.contains("requested: 127.0.0.1:8080"));
        assert!(diagnostic.contains("planned: 127.0.0.1:8080"));
        assert!(!diagnostic.contains("service: `app`"));
        assert!(!diagnostic.contains("planned: 127.0.0.1:80;"));
    }

    #[test]
    fn startup_failure_classifier_reports_unsupported_udp_published_port() {
        let input = planning_input(
            json!({
                "services": {
                    "app": {
                        "ports": [
                            {"target": 3000, "published": "3000", "protocol": "udp"},
                            {"host_ip": "127.0.0.1", "target": 3001, "published": "3001"}
                        ]
                    }
                }
            }),
            "app",
            &[],
        );
        let plan = ComposePublishedPortPlan::default();
        let diagnostics = ComposePublishedPortStartupDiagnostics {
            input: &input,
            plan: &plan,
            relocation_enabled: false,
        };

        let diagnostic = classify_compose_published_port_startup_failure(
            "Bind for 0.0.0.0:3000 failed: port is already allocated",
            diagnostics,
        )
        .expect("unsupported UDP bind conflict should be classified")
        .to_string();

        assert!(diagnostic.contains(COMPOSE_PUBLISHED_PORT_UNSUPPORTED));
        assert!(diagnostic.contains("service: `app`"));
        assert!(diagnostic.contains("port entry: 0"));
        assert!(diagnostic.contains("<host_ip omitted>:3000"));
        assert!(diagnostic.contains("app:3000/udp"));
        assert!(diagnostic.contains("UDP Compose published ports are not relocation-eligible"));
    }

    #[test]
    fn startup_failure_classifier_does_not_report_udp_failure_as_tcp_collision() {
        let input = planning_input(
            json!({
                "services": {
                    "app": {
                        "ports": [
                            {"target": 8125, "published": "8125"},
                            {"target": 8125, "published": "8125", "protocol": "udp"}
                        ]
                    }
                }
            }),
            "app",
            &[],
        );
        let plan = ComposePublishedPortPlan::default();

        let diagnostic = classify_compose_published_port_startup_failure(
            "Ports are not available: listen udp 0.0.0.0:8125: bind: address already in use",
            ComposePublishedPortStartupDiagnostics {
                input: &input,
                plan: &plan,
                relocation_enabled: false,
            },
        )
        .expect("unsupported UDP bind conflict should be classified")
        .to_string();

        assert!(diagnostic.contains(COMPOSE_PUBLISHED_PORT_UNSUPPORTED));
        assert!(diagnostic.contains("port entry: 1"));
        assert!(diagnostic.contains("app:8125/udp"));
        assert!(!diagnostic.contains(COMPOSE_PUBLISHED_PORT_COLLISION));
        assert!(!diagnostic.contains("app:8125/tcp"));
    }

    #[test]
    fn startup_failure_classifier_reports_unsupported_range_published_port() {
        let input = planning_input(
            json!({
                "services": {
                    "app": {
                        "ports": [
                            {"target": 3000, "published": "3000-3002"}
                        ]
                    }
                }
            }),
            "app",
            &[],
        );
        let plan = ComposePublishedPortPlan::default();

        let diagnostic = classify_compose_published_port_startup_failure(
            "Error response from daemon: Bind for 0.0.0.0:3001 failed: port is already allocated",
            ComposePublishedPortStartupDiagnostics {
                input: &input,
                plan: &plan,
                relocation_enabled: false,
            },
        )
        .expect("unsupported range bind conflict should be classified")
        .to_string();

        assert!(diagnostic.contains(COMPOSE_PUBLISHED_PORT_UNSUPPORTED));
        assert!(diagnostic.contains("service: `app`"));
        assert!(diagnostic.contains("<host_ip omitted>:3000-3002"));
        assert!(diagnostic.contains("app:3000/tcp"));
        assert!(diagnostic.contains("Published host port range is unsupported"));
    }

    #[test]
    fn startup_failure_classifier_reports_unsupported_after_bind_race_checks() {
        let input = planning_input(
            json!({
                "services": {
                    "app": {
                        "ports": [
                            {"host_ip": "127.0.0.1", "target": 3000, "published": "3000"},
                            {"target": 9000, "published": "9000", "protocol": "udp"},
                            {"target": 8125, "published": "8125", "protocol": "udp"}
                        ]
                    }
                }
            }),
            "app",
            &[],
        );
        let plan = plan_with_availability(&input, &[3000]);

        let diagnostic = classify_compose_published_port_startup_failure(
            "Error response from daemon: Bind for 0.0.0.0:8125 failed: port is already allocated",
            ComposePublishedPortStartupDiagnostics {
                input: &input,
                plan: &plan,
                relocation_enabled: true,
            },
        )
        .expect("unsupported UDP bind conflict should be classified after planned entries")
        .to_string();

        assert!(diagnostic.contains(COMPOSE_PUBLISHED_PORT_UNSUPPORTED));
        assert!(diagnostic.contains("port entry: 2"));
        assert!(diagnostic.contains("app:8125/udp"));
        assert!(!diagnostic.contains(COMPOSE_PUBLISHED_PORT_BIND_RACE));
    }

    #[test]
    fn startup_failure_classifier_does_not_report_udp_failure_as_tcp_bind_race() {
        let input = planning_input(
            json!({
                "services": {
                    "app": {
                        "ports": [
                            {"target": 8125, "published": "8125"},
                            {"target": 8125, "published": "8125", "protocol": "udp"}
                        ]
                    }
                }
            }),
            "app",
            &[],
        );
        let plan = plan_with_availability(&input, &[]);

        let diagnostic = classify_compose_published_port_startup_failure(
            "Ports are not available: listen udp 0.0.0.0:8125: bind: address already in use",
            ComposePublishedPortStartupDiagnostics {
                input: &input,
                plan: &plan,
                relocation_enabled: true,
            },
        )
        .expect("unsupported UDP bind conflict should be classified")
        .to_string();

        assert!(diagnostic.contains(COMPOSE_PUBLISHED_PORT_UNSUPPORTED));
        assert!(diagnostic.contains("port entry: 1"));
        assert!(diagnostic.contains("app:8125/udp"));
        assert!(!diagnostic.contains(COMPOSE_PUBLISHED_PORT_BIND_RACE));
        assert!(!diagnostic.contains("app:8125/tcp"));
    }

    #[test]
    fn startup_failure_classifier_ignores_unrelated_or_endpointless_errors() {
        let input = planning_input(
            json!({
                "services": {
                    "app": {
                        "ports": [
                            {"target": 3000},
                            {"host_ip": "127.0.0.1", "target": 3001, "published": "3001"}
                        ]
                    }
                }
            }),
            "app",
            &[],
        );
        let plan = ComposePublishedPortPlan::default();
        let diagnostics = ComposePublishedPortStartupDiagnostics {
            input: &input,
            plan: &plan,
            relocation_enabled: false,
        };

        assert!(
            classify_compose_published_port_startup_failure(
                "pull access denied for image using port 3001",
                diagnostics,
            )
            .is_none()
        );
        assert!(
            classify_compose_published_port_startup_failure(
                "Bind for 0.0.0.0:3000 failed: port is already allocated",
                diagnostics,
            )
            .is_none()
        );
        assert!(
            classify_compose_published_port_startup_failure(
                "Bind for 0.0.0.0:3001 failed: port is already allocated",
                diagnostics,
            )
            .is_none()
        );
    }
}
