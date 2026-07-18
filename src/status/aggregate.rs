use std::collections::BTreeSet;

use crate::state::{LifecycleCompletion, LifecycleState};

use super::types::{EnvironmentStatus, HealthStatus, LifecycleStatus, RuntimeRunState};

pub(super) fn aggregate_environment_status(
    run_states: impl IntoIterator<Item = RuntimeRunState>,
) -> EnvironmentStatus {
    let mut containers = 0;
    let mut running = 0;
    for run_state in run_states {
        containers += 1;
        match run_state {
            RuntimeRunState::Running => running += 1,
            RuntimeRunState::Stopped => {}
            RuntimeRunState::Unknown => return EnvironmentStatus::Unknown,
        }
    }

    let stopped = containers - running;
    match (running, stopped) {
        (0, 0) => EnvironmentStatus::Missing,
        (0, _) => EnvironmentStatus::Stopped,
        (_, 0) => EnvironmentStatus::Running,
        _ => EnvironmentStatus::Partial,
    }
}

pub(super) fn aggregate_health_status(
    statuses: impl IntoIterator<Item = HealthStatus>,
) -> HealthStatus {
    let statuses = statuses.into_iter().collect::<BTreeSet<_>>();
    if statuses.is_empty() || statuses.contains(&HealthStatus::Unknown) {
        return HealthStatus::Unknown;
    }
    if statuses.len() == 1 {
        return statuses.into_iter().next().unwrap_or(HealthStatus::Unknown);
    }
    HealthStatus::Mixed
}

pub(super) fn aggregate_lifecycle_status(lifecycle: Option<LifecycleState>) -> LifecycleStatus {
    let Some(lifecycle) = lifecycle else {
        return LifecycleStatus::Unknown;
    };
    if [
        LifecycleCompletion::OnCreate,
        LifecycleCompletion::UpdateContent,
        LifecycleCompletion::PostCreate,
    ]
    .into_iter()
    .all(|completion| lifecycle.is_completed(completion))
    {
        LifecycleStatus::Complete
    } else {
        LifecycleStatus::Incomplete
    }
}

pub(super) fn should_report_unhealthy(
    aggregate: HealthStatus,
    statuses: impl IntoIterator<Item = HealthStatus>,
) -> bool {
    aggregate == HealthStatus::Unhealthy
        || (aggregate == HealthStatus::Mixed
            && statuses
                .into_iter()
                .any(|status| status == HealthStatus::Unhealthy))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_status_covers_empty_uniform_mixed_and_unknown_states() {
        assert_eq!(aggregate_environment_status([]), EnvironmentStatus::Missing);
        assert_eq!(
            aggregate_environment_status([RuntimeRunState::Running]),
            EnvironmentStatus::Running
        );
        assert_eq!(
            aggregate_environment_status([RuntimeRunState::Stopped]),
            EnvironmentStatus::Stopped
        );
        assert_eq!(
            aggregate_environment_status([RuntimeRunState::Running, RuntimeRunState::Stopped]),
            EnvironmentStatus::Partial
        );
        assert_eq!(
            aggregate_environment_status([RuntimeRunState::Running, RuntimeRunState::Unknown]),
            EnvironmentStatus::Unknown
        );
    }

    #[test]
    fn health_status_covers_empty_uniform_mixed_and_unknown_states() {
        assert_eq!(aggregate_health_status([]), HealthStatus::Unknown);
        assert_eq!(
            aggregate_health_status([HealthStatus::Healthy]),
            HealthStatus::Healthy
        );
        assert_eq!(
            aggregate_health_status([HealthStatus::Unhealthy]),
            HealthStatus::Unhealthy
        );
        assert_eq!(
            aggregate_health_status([HealthStatus::Starting]),
            HealthStatus::Starting
        );
        assert_eq!(
            aggregate_health_status([HealthStatus::None]),
            HealthStatus::None
        );
        assert_eq!(
            aggregate_health_status([HealthStatus::Healthy, HealthStatus::None]),
            HealthStatus::Mixed
        );
        assert_eq!(
            aggregate_health_status([HealthStatus::Healthy, HealthStatus::Unknown]),
            HealthStatus::Unknown
        );
    }

    #[test]
    fn unhealthy_issue_requires_an_unhealthy_mixed_member() {
        assert!(!should_report_unhealthy(
            HealthStatus::Mixed,
            [HealthStatus::Healthy, HealthStatus::None]
        ));
        assert!(should_report_unhealthy(
            HealthStatus::Mixed,
            [HealthStatus::Healthy, HealthStatus::Unhealthy]
        ));
        assert!(should_report_unhealthy(
            HealthStatus::Unhealthy,
            [HealthStatus::Unhealthy]
        ));
        assert!(!should_report_unhealthy(
            HealthStatus::Unknown,
            [HealthStatus::Unknown, HealthStatus::Unhealthy]
        ));
    }

    #[test]
    fn lifecycle_status_covers_available_and_unavailable_state() {
        assert_eq!(aggregate_lifecycle_status(None), LifecycleStatus::Unknown);
        assert_eq!(
            aggregate_lifecycle_status(Some(LifecycleState::default())),
            LifecycleStatus::Incomplete
        );
        assert_eq!(
            aggregate_lifecycle_status(Some(LifecycleState::all_completed())),
            LifecycleStatus::Complete
        );
    }
}
