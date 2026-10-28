//! The memex's integration tests, in one binary. Tests that set
//! environment variables (google, media) stay binaries of their own.

mod chat;
mod claims;
mod docs;
mod fixtures;
mod knowledge;
mod memex;
mod move_in;
mod pools;
mod realms;
mod records;
mod relations;
mod triage;
mod understanding;

/// The c6-extract kit's code, in this process (what workers run).
pub fn extract_here() {
    c6_memex::extract::set_override(std::sync::Arc::new(c6_memex::extract::InProcess::default()));
}
