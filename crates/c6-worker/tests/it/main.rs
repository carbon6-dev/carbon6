//! The worker's integration tests, in one binary (one link, run in
//! parallel). Tests that set environment variables (ssh, hosts,
//! templates) stay binaries of their own.

mod agents;
mod analytics;
mod api_reference;
mod apps;
mod assistant;
mod attach;
mod attachments;
mod automations;
mod background;
mod cards;
mod chaos;
mod collections;
mod commands;
mod decisions;
mod examples;
mod explorer;
mod extract;
mod fixtures;
mod grants;
mod hooks;
mod ide;
mod imports;
mod journal;
mod kitjobs;
mod labels;
mod m4;
mod memory;
mod notify;
mod operations;
mod outputs;
mod packages;
mod policies;
mod prompts;
mod providers;
mod push;
mod realm_pages;
mod realms;
mod rewind;
mod routing;
mod runs;
mod scratch;
mod search;
mod secrets;
mod series;
mod session;
mod side_chat;
mod subagent_models;
mod subagents;
mod suggestions;
mod task_cards;
mod triggers;
mod updates;
mod usage;
mod views;
mod websearch;
mod workflows;

/// Opens the worker in `state`. A worker just stopped there may still be
/// letting go of the state directory's lock (its tasks drop their handles as
/// they end), so that error is retried for a while; any other fails at once.
pub async fn open_worker(state: &std::path::Path) -> std::sync::Arc<c6_worker::Worker> {
    let deadline = tokio::time::Instant::now() + c6_testkit::wait::deadline();
    loop {
        match c6_worker::Worker::open(state, c6_worker::Identity::load(state).unwrap()).await {
            Ok(w) => return w,
            Err(e) if format!("{e}").contains("already running") && tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await
            }
            Err(e) => panic!("opening the worker: {e:#}"),
        }
    }
}

/// Starts a head again on the ports it had (its workers know them): ports
/// from `c6_testkit::port()`, which no other test is given. Binding is
/// still retried while the old listener lets go.
pub async fn restart_head(cfg: c6_head::HeadConfig) -> c6_head::Head {
    let deadline = tokio::time::Instant::now() + c6_testkit::wait::deadline();
    loop {
        match c6_head::start(cfg.clone()).await {
            Ok(h) => return h,
            Err(e) if format!("{e:#}").contains("binding") && tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await
            }
            Err(e) => panic!("starting the head again: {e:#}"),
        }
    }
}
