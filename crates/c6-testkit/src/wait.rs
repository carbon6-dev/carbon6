//! Waiting in tests (docs/testing.md): on a condition, never on time.
//!
//! A test fails when what it checks is false, not when the machine is
//! slow. So every wait is for a condition, with one deadline that only
//! catches a hang (the condition never comes true): long enough that no
//! CI machine, however loaded, reaches it while things are working.

use std::future::Future;
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
/// restarts while its workers keep its address). Ports come from a range
/// below every OS's ephemeral range, so no test binding port 0 is ever given
/// one, and each is reserved with a file lock in the temp folder for as long
/// as the test process lives: tests in other processes (nextest runs each
/// test as its own) never get the same one. The OS drops the lock when the
/// process ends, however it ends.
pub fn port() -> u16 {
    use std::sync::Mutex;
    static HELD: Mutex<Vec<std::fs::File>> = Mutex::new(Vec::new());
    const FIRST: u32 = 20_000;
    const COUNT: u32 = 10_000;
    let dir = std::env::temp_dir().join("c6-test-ports");
    let _ = std::fs::create_dir_all(&dir);
    // Start somewhere different in each process, so they rarely contend.
    let start = std::process::id().wrapping_mul(7919) % COUNT;
    for i in 0..COUNT {
        let p = (FIRST + (start + i) % COUNT) as u16;
        let Ok(file) = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join(p.to_string())) else {
            continue;
        };
        if file.try_lock().is_err() {
            continue; // another test process has it
        }
        // Something outside the tests may hold it: take the next.
        if std::net::TcpListener::bind(("127.0.0.1", p)).is_err() {
            continue;
        }
        HELD.lock().unwrap().push(file);
        return p;
    }
    panic!("no free test port in {FIRST}..{}", FIRST + COUNT);
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

#[cfg(test)]
mod tests {
    #[test]
    fn ports_are_never_handed_out_twice() {
        let a: std::collections::HashSet<u16> = (0..50).map(|_| super::port()).collect();
        assert_eq!(a.len(), 50);
        assert!(a.iter().all(|p| (20_000..30_000).contains(p)));
    }
}
