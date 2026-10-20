//! Waiting in tests (docs/testing.md): on a condition, never on time.
//!
//! A test fails when what it checks is false, not when the machine is
//! slow. So every wait is for a condition, with one deadline that only
//! catches a hang (the condition never comes true): long enough that no
//! CI machine, however loaded, reaches it while things are working.

use std::future::Future;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

/// How long a test waits for anything before calling it a hang:
/// `C6_TEST_DEADLINE_SECS`, default 300.
pub fn deadline() -> Duration {
    let secs = std::env::var("C6_TEST_DEADLINE_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(300);
    Duration::from_secs(secs)
}

/// Polls `f` until it returns `Some`, and returns that. Panics, naming
/// `what`, only if it never does (a hang).
pub async fn eventually<T>(what: &str, mut f: impl AsyncFnMut() -> Option<T>) -> T {
    let end = tokio::time::Instant::now() + deadline();
    loop {
        if let Some(v) = f().await {
            return v;
        }
        assert!(tokio::time::Instant::now() < end, "still waiting for {what} after {:?}: it never happened", deadline());
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Awaits `fut`. Panics, naming `what`, only if it never finishes (a hang).
pub async fn within<F: Future>(what: &str, fut: F) -> F::Output {
    match tokio::time::timeout(deadline(), fut).await {
        Ok(v) => v,
        Err(_) => panic!("still waiting for {what} after {:?}: it never happened", deadline()),
    }
}

/// A port for a server that must come back on the same port (a head that
/// restarts while its workers keep its address). Ports are handed out once
/// per test process from a range below every OS's ephemeral range, so no
/// other test, binding port 0, is ever given one.
pub fn port() -> u16 {
    static NEXT: AtomicU16 = AtomicU16::new(0);
    // Test binaries run one at a time; each starts at its own offset in
    // case two ever overlap.
    let base = 20_000 + (std::process::id() % 40) as u16 * 200;
    loop {
        let p = base + NEXT.fetch_add(1, Ordering::Relaxed) % 200;
        // Something outside the tests may hold it: take the next.
        if std::net::TcpListener::bind(("127.0.0.1", p)).is_ok() {
            return p;
        }
    }
}

/// Whether `tool` (a program on PATH) is here: see [`need`].
pub fn have(tool: &str) -> bool {
    need(tool, std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(tool).is_file())))
}

/// `present`, for something a test needs from the machine (a browser, a
/// tool). On a developer's machine a test without it may skip; in CI
/// (`GITHUB_ACTIONS`) it must be there, so its absence fails the test
/// instead of skipping it quietly.
pub fn need(what: &str, present: bool) -> bool {
    if !present && std::env::var_os("GITHUB_ACTIONS").is_some() {
        panic!("{what} is missing in CI: provide it in the workflow (docs/testing.md)");
    }
    if !present {
        eprintln!("{what} is not here: skipped");
    }
    present
}
