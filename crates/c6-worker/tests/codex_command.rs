//! A configured Codex outside PATH finds its sibling interpreter, without
//! changing the worker environment. One test owns this process's environment.

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use c6_api::{CodexLimitsSettings, ConnectionQuota};
use c6_head::HeadConfig;
use c6_testkit::{eventually, within};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::json;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn configured_codex_finds_sibling_node_and_retries_without_waiting_for_backoff() {
    let (dir, state, bins, empty_path) =
        (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let home = std::env::var_os("HOME");
    let codex_home = std::env::var_os("CODEX_HOME");
    // SAFETY: this standalone binary has one test, and no worker/head tasks
    // have started. No real Codex or node may be found through PATH.
    unsafe { std::env::set_var("PATH", empty_path.path()) };
    let binary = bins.path().join("codex");
    let node = bins.path().join("node");
    let shell = bins.path().join("shell");
    std::fs::write(
        &shell,
        format!("#!/bin/sh\n[ \"$1\" = --version ] && exit 0\nprintf '%s\\n' called >> '{}/shell-calls'\nexit 1\n", bins.path().display()),
    )
    .unwrap();
    std::fs::write(
        &binary,
        r##"#!/usr/bin/env node
[ "$1" = "--version" ] && exit 0
[ "$*" = "app-server --listen stdio://" ] || exit 2
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*) printf '%s\n' '{"id":1,"result":{}}';;
    *'"method":"account/rateLimits/read"'*) printf '%s\n' '{"id":2,"result":{"accountId":"fake-account","rateLimits":{"planType":"plus","primary":{"usedPercent":34,"windowDurationMins":300}}}}';;
    *'"method":"account/read"'*) printf '%s\n' '{"id":4,"result":{"account":{"type":"chatgpt","email":"fake@example.test","planType":"plus"}}}';;
  esac
done
"##,
    )
    .unwrap();
    std::fs::write(
        &node,
        r##"#!/bin/sh
[ "$1" = "--version" ] && exit 0
printf '%s' "$PATH" > "$0.path"
printf '%s\n' called >> "$0.calls"
printf '%s\n%s' "${HOME+x}" "$HOME" > "$0.home"
printf '%s\n%s' "${CODEX_HOME+x}" "$CODEX_HOME" > "$0.codex_home"
exec /bin/sh "$@"
"##,
    )
    .unwrap();
    // SAFETY: still before starting any processes/tasks in this one-test binary.
    unsafe { std::env::set_var("SHELL", &shell) };
    for script in [&binary, &node, &shell] {
        std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o755)).unwrap();
        c6_testkit::runnable(script, &["--version"]).await;
    }
    let head = c6_head::start(HeadConfig {
        data_dir: dir.path().into(),
        bind: vec!["127.0.0.1".parse().unwrap()],
        ui_port: 0,
        worker_port: 0,
        auth: false,
        ..Default::default()
    })
    .await
    .unwrap();
    let http = reqwest::Client::new();
    let api = format!("http://127.0.0.1:{}/api/v1", head.ui_addrs[0].port());
    let url = format!("{api}/settings/codex_limits");
    let enrollment = c6_head::api::new_enrollment(&head.state, Some("command-path-worker")).await.unwrap();
    enroll::enroll(&enrollment.token).await.unwrap().save(state.path()).unwrap();
    let worker = Worker::open(state.path(), Identity::load(state.path()).unwrap()).await.unwrap();
    let id = worker.identity.meta.worker_id.to_string();
    {
        let mut o = worker.overrides.lock().unwrap();
        o.codex_shell_timeout = Some(c6_testkit::wait::deadline());
        o.codex_every = Some(Duration::from_secs(3600));
        o.codex_config_every = Some(Duration::from_millis(100));
        o.codex_timeout = Some(c6_testkit::wait::deadline());
    }
    let stop = CancellationToken::new();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let run = tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    within("worker attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    let initial: CodexLimitsSettings = http.get(&url).send().await.unwrap().json().await.unwrap();
    assert!(initial.commands.is_empty());
    for invalid in [json!({"workers":[id],"commands":{&id:"relative/codex"}}), json!({"commands":{"not-a-worker":"/bin/codex"}})] {
        assert_eq!(http.put(&url).json(&invalid).send().await.unwrap().status(), reqwest::StatusCode::BAD_REQUEST);
    }
    // The old settings shape still works, but shell discovery initially fails.
    assert!(http.put(&url).json(&json!({"workers":[id]})).send().await.unwrap().status().is_success());
    let quotas =
        async || -> Vec<ConnectionQuota> { http.get(format!("{api}/connections/quotas")).send().await.unwrap().json().await.unwrap() };
    let quota_id = format!("codex-cli:{id}");
    eventually("missing Codex diagnostic without a configured command", async || {
        quotas().await.into_iter().find(|q| q.connection_id == quota_id && q.error == "Codex not found: set its path")
    })
    .await;
    assert!(!node.with_extension("path").exists(), "the fake interpreter is outside PATH too");
    assert_eq!(std::fs::read_to_string(bins.path().join("shell-calls")).unwrap().lines().count(), 1);
    let configured = json!({"workers":[id],"commands":{&id:binary}});
    assert!(http.put(&url).json(&configured).send().await.unwrap().status().is_success());
    let saved: serde_json::Value = http.get(&url).send().await.unwrap().json().await.unwrap();
    assert_eq!(saved, configured);
    let config = worker.link.call("quotas.codex.config", json!({})).await.unwrap();
    assert_eq!(config["command"], binary.to_str().unwrap());
    // No override bypasses the configured path. The prior failure's backoff
    // and the successful poll interval both exceed the test hang deadline.
    let snapshot = eventually("command change retries immediately and uses sibling node", async || {
        quotas().await.into_iter().find(|q| q.connection_id == quota_id && q.error.is_empty() && !q.windows.is_empty())
    })
    .await;
    assert_eq!(snapshot.windows[0].used_percent, 34.0);
    assert_eq!(snapshot.origin.as_ref().unwrap().command.as_deref(), binary.to_str());
    assert!(!snapshot.origin.as_ref().unwrap().command_from_shell);
    let child_path = std::env::join_paths([bins.path(), empty_path.path()]).unwrap();
    assert_eq!(std::fs::read_to_string(node.with_extension("path")).unwrap(), child_path.to_str().unwrap());
    for (name, value) in [("home", &home), ("codex_home", &codex_home)] {
        let expected = value.as_ref().map(|v| format!("x\n{}", v.to_str().unwrap())).unwrap_or_else(|| "\n".into());
        assert_eq!(std::fs::read_to_string(node.with_extension(name)).unwrap(), expected);
    }
    assert_eq!(std::env::var_os("PATH").unwrap(), empty_path.path());
    assert_eq!(std::env::var_os("HOME"), home);
    assert_eq!(std::env::var_os("CODEX_HOME"), codex_home);
    // Clearing the path also retries shell discovery promptly.
    assert!(http.put(&url).json(&json!({"workers":[id],"commands":{}})).send().await.unwrap().status().is_success());
    eventually("cleared command retries shell discovery", async || {
        quotas().await.into_iter().find(|q| q.connection_id == quota_id && q.error == "Codex not found: set its path")
    })
    .await;
    assert_eq!(std::fs::read_to_string(bins.path().join("shell-calls")).unwrap().lines().count(), 2);
    // Switch to a successful noisy login shell. A setting change invalidates
    // the prior failed discovery without waiting for its error backoff.
    std::fs::write(&shell, format!("#!/bin/sh\n[ \"$1\" = --version ] && exit 0\n[ \"$1\" = -lic ] && [ \"$2\" = 'command -v codex' ] || exit 2\nprintf '%s\\n' called >> '{}/shell-calls'\nprintf '%s\\n' 'Welcome!' '/ignore/old/path' '{}' 'goodbye'\n", bins.path().display(), binary.display())).unwrap();
    c6_testkit::runnable(&shell, &["--version"]).await;
    assert!(http.put(&url).json(&configured).send().await.unwrap().status().is_success());
    eventually("manual override works while shell is changed", async || {
        quotas().await.into_iter().find(|q| q.connection_id == quota_id && q.error.is_empty())
    })
    .await;
    worker.overrides.lock().unwrap().codex_every = Some(Duration::ZERO);
    assert!(http.put(&url).json(&json!({"workers":[id]})).send().await.unwrap().status().is_success());
    eventually("automatic discovery identifies the user's shell in quota metadata", async || {
        quotas().await.into_iter().find(|q| {
            q.connection_id == quota_id
                && q.error.is_empty()
                && q.origin.as_ref().is_some_and(|o| o.command_from_shell && o.command.as_deref() == binary.to_str())
        })
    })
    .await;
    let calls_before = std::fs::read_to_string(node.with_extension("calls")).unwrap().lines().count();
    eventually("several successful polls using the cached executable", async || {
        (std::fs::read_to_string(node.with_extension("calls")).unwrap().lines().count() >= calls_before + 2).then_some(())
    })
    .await;
    assert_eq!(
        std::fs::read_to_string(bins.path().join("shell-calls")).unwrap().lines().count(),
        3,
        "do not spawn the shell on every poll"
    );
    // A cached executable disappearing invalidates discovery. With a zero
    // test poll interval the failed read can retry without a real backoff.
    let replacement = bins.path().join("codex-new");
    std::fs::write(&shell, format!("#!/bin/sh\n[ \"$1\" = --version ] && exit 0\nprintf '%s\\n' called >> '{}/shell-calls'\nprintf '%s\\n' 'Welcome!' '{}' 'goodbye'\n", bins.path().display(), replacement.display())).unwrap();
    c6_testkit::runnable(&shell, &["--version"]).await;
    std::fs::rename(&binary, &replacement).unwrap();
    eventually("a failed cached executable is rediscovered", async || {
        quotas().await.into_iter().find(|q| {
            q.connection_id == quota_id
                && q.error.is_empty()
                && q.origin.as_ref().is_some_and(|o| o.command_from_shell && o.command.as_deref() == replacement.to_str())
        })
    })
    .await;
    assert_eq!(std::fs::read_to_string(bins.path().join("shell-calls")).unwrap().lines().count(), 4);
    stop.cancel();
    within("worker shutdown", run).await.unwrap().unwrap();
    head.shutdown().await.unwrap();
}
