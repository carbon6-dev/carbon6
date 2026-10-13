//! The worker's integration tests, in one binary (one link, run in
//! parallel). Tests that set environment variables (ssh, hosts,
//! templates) stay binaries of their own.

mod agents;
mod api_reference;
mod assistant;
mod attach;
mod attachments;
mod automations;
mod background;
mod chaos;
mod commands;
mod fixtures;
mod hooks;
mod ide;
mod journal;
mod labels;
mod m4;
mod memory;
mod notify;
mod outputs;
mod policies;
mod push;
mod rewind;
mod routing;
mod runs;
mod scratch;
mod search;
mod secrets;
mod session;
mod side_chat;
mod usage;
mod websearch;
mod workflows;
