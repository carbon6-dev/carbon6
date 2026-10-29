//! The memex's integration tests, in one binary. Tests that set
//! environment variables (google, media) stay binaries of their own.

#[cfg(target_os = "macos")]
#[used]
#[unsafe(link_section = "__DATA,__mod_init_func")]
static RAISE_NOFILE: unsafe extern "C" fn() = raise_nofile_before_tests;

#[cfg(target_os = "macos")]
unsafe extern "C" fn raise_nofile_before_tests() {
    c6_testkit::raise_open_file_limit();
}

mod assistant;
mod chat;
mod claims;
mod compose;
mod docs;
mod files;
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
