pub(crate) mod container_tools;
pub(crate) mod credentials;
pub(crate) mod daemon;
pub(crate) mod forward;
pub(crate) mod protocol;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the bounded daemon dispatch connects the query coordinator in follow-up task #431"
    )
)]
pub(crate) mod query;
pub(crate) mod query_context;
pub(crate) mod runtime;
