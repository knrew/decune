#![allow(dead_code)]

use std::collections::HashMap;

use bollard::query_parameters::{ListContainersOptions, ListContainersOptionsBuilder};

use crate::docker::resource::managed_workspace_label_filters;

pub(crate) fn workspace_container_list_options(workspace_id: &str) -> ListContainersOptions {
    let filters = managed_workspace_label_filters(workspace_id)
        .into_iter()
        .collect::<HashMap<_, _>>();

    ListContainersOptionsBuilder::new()
        .all(true)
        .filters(&filters)
        .build()
}

#[cfg(test)]
mod tests {
    use super::workspace_container_list_options;

    #[test]
    fn workspace_container_list_options_searches_only_managed_workspace_containers() {
        let options = workspace_container_list_options("abc123def456");
        let filters = options.filters.unwrap();

        assert!(options.all);
        assert_eq!(
            filters.get("label"),
            Some(&vec![
                "decune.managed=true".to_owned(),
                "decune.workspace_id=abc123def456".to_owned(),
            ])
        );
    }
}
