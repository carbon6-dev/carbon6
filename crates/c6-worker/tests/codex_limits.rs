//! Local Codex app-server reads: protocol, bounded cleanup, opt-in and attribution.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use c6_api::{CodexLimitsReport, ConnectionQuota};
use c6_head::HeadConfig;
use c6_testkit::{eventually, within};
use c6_worker::{ConnState, Identity, Worker, codex_limits, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

fn result(account: Option<&str>) -> Value {
    json!({"accountId":account,"rateLimits":{"planType":"plus",
        // Deliberately swapped: durations, not primary/secondary, name them.
        "primary":{"usedPercent":71,"windowDurationMins":10080,"resetsAt":4102444800i64},
        "secondary":{"usedPercent":34,"windowDurationMins":300,"resetsAt":4102441200i64}}})
}

async fn fake(dir: &Path, mode: &str, body: Value) -> PathBuf {
    let binary = dir.join("codex");
    std::fs::write(dir.join("mode"), mode).unwrap();
    std::fs::write(dir.join("response"), format!("{}\n", json!({"id":2,"result":body,"error":null}))).unwrap();
    std::fs::write(
        &binary,
        r#"#!/bin/sh
[ "$1" = --version ] && exit 0
[ "$1" = app-server ] && [ "$2" = --listen ] && [ "$3" = stdio:// ] || exit 90
cd "$(dirname "$0")" || exit 91
echo $$ > pid
mode=$(cat mode)
trap 'echo terminated > exited; exit 0' TERM
while IFS= read -r line; do
    printf '%s\n' "$line" >> requests
    case "$line" in
        *'"method":"initialize"'*)
            [ "$mode" = hang ] && continue
            [ "$mode" = malformed ] && { echo 'secret-not-json'; continue; }
            [ "$mode" = oversized ] && { printf '%070000d\n' 0; continue; }
            echo '{"method":"server/notification"}'
            echo '{"id":1,"result":{"userAgent":"fake"}}';;
        *'"method":"initialized"'*) :;;
        *'"method":"account/rateLimits/read"'*)
            [ "$mode" = auth ] && { echo '{"id":2,"error":{"code":-32000,"message":"secret-error-detail"}}'; continue; }
            if [ "$mode" = old ] || [ "$mode" = old-request ]; then
                case "$line" in
                    *'"params"'*)
                        code=-32602
                        [ "$mode" = old-request ] && code=-32600
                        printf '{"id":2,"error":{"code":%s,"message":"invalid params"}}\n' "$code";;
                    *) sed 's/"id":2/"id":3/' response;;
                esac
            else
                # More than a pipe buffer: stderr must be drained concurrently.
                i=0; while [ "$i" -lt 2000 ]; do printf '%080d\n' 0 >&2; i=$((i+1)); done
                cat response
            fi;;
        *) echo forbidden > forbidden; exit 92;;
    esac
done
echo eof > exited
"#,
    )
    .unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    c6_testkit::runnable(&binary, &["--version"]).await;
    binary
}

fn requests(dir: &Path) -> Vec<Value> {
    std::fs::read_to_string(dir.join("requests")).unwrap().lines().map(|s| serde_json::from_str(s).unwrap()).collect()
}

#[tokio::test]
async fn newline_rpc_handshake_and_old_version_retry() {
    for mode in ["new", "old", "old-request"] {
        let dir = tempfile::tempdir().unwrap();
        let binary = fake(dir.path(), mode, result(Some("account-a"))).await;
        let report =
            within("Codex plan read", codex_limits::read(&binary, c6_testkit::wait::deadline(), &CancellationToken::new())).await.unwrap();
        assert_eq!(report.account_id.as_deref(), Some("account-a"));
        assert_eq!(report.plan_type.as_deref(), Some("plus"));
        assert_eq!(report.windows.iter().map(|w| (w.window_minutes, w.used_percent)).collect::<Vec<_>>(), [(10080, 71.0), (300, 34.0)]);
        let asked = requests(dir.path());
        assert_eq!(asked[0]["method"], "initialize");
        assert_eq!(asked[0]["params"]["clientInfo"]["name"], "carbon6");
        assert_eq!(asked[1], json!({"method":"initialized"}));
        assert_eq!(asked[2], json!({"id":2,"method":"account/rateLimits/read","params":{"excludeResetCreditDetails":true}}));
        assert_eq!(asked.len(), if mode == "new" { 3 } else { 4 });
        if mode != "new" {
            assert_eq!(asked[3], json!({"id":3,"method":"account/rateLimits/read"}));
        }
        assert!(!dir.path().join("forbidden").exists(), "never starts a turn");
        let pid = std::fs::read_to_string(dir.path().join("pid")).unwrap();
        assert!(nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid.trim().parse().unwrap()), None).is_err(), "app server reaped");
    }
}

#[test]
fn multi_limit_schema_and_missing_identity() {
    let mut body = result(None);
    body["rateLimitsByLimitId"] = json!({"other":{"primary":{}},"codex":result(None)["rateLimits"]});
    body["rateLimitsByLimitId"]["codex"]["extraNewField"] = json!(true);
    let parsed = codex_limits::parse(&body).unwrap();
    assert_eq!(parsed.account_id, None);
    assert_eq!(parsed.windows.len(), 2);
    body["accountId"] = json!("outer-account");
    assert_eq!(codex_limits::parse(&body).unwrap().account_id.as_deref(), Some("outer-account"));
    body["rateLimitsByLimitId"]["codex"]["primary"]["windowDurationMins"] = Value::Null;
    assert_eq!(codex_limits::parse(&body).unwrap().windows.len(), 1, "unknown duration is not guessed");
    body["rateLimitsByLimitId"]["codex"]["primary"]["windowDurationMins"] = json!(10080);
    assert!(codex_limits::parse(&json!({"rateLimits":null})).is_err());
    body["rateLimitsByLimitId"]["codex"]["primary"]["usedPercent"] = json!("bad");
    assert!(codex_limits::parse(&body).is_err());
}

#[tokio::test]
async fn missing_binary_malformed_output_and_timeout_are_safe_errors() {
    let dir = tempfile::tempdir().unwrap();
    let stop = CancellationToken::new();
    let error = codex_limits::read(&dir.path().join("absent"), c6_testkit::wait::deadline(), &stop).await.unwrap_err();
    assert!(error.to_string().contains("not found"));
    let binary = fake(dir.path(), "malformed", result(None)).await;
    let error = within("malformed response", codex_limits::read(&binary, c6_testkit::wait::deadline(), &stop)).await.unwrap_err();
    assert!(error.to_string().contains("malformed JSON"));
    assert!(!error.to_string().contains("secret-not-json"));
    let binary = fake(dir.path(), "hang", result(None)).await;
    // This short duration is the behavior under test, not a test deadline:
    // the server never replies, however quickly or slowly it is scheduled.
    let error = within("hung app server timeout", codex_limits::read(&binary, Duration::from_millis(100), &stop)).await.unwrap_err();
    assert!(error.to_string().contains("timed out"));
    if let Ok(pid) = std::fs::read_to_string(dir.path().join("pid")) {
        let pid = nix::unistd::Pid::from_raw(pid.trim().parse().unwrap());
        eventually("Codex child to be reaped", async || nix::sys::signal::kill(pid, None).is_err().then_some(())).await;
    }
}

#[tokio::test]
async fn cancellation_oversized_output_and_auth_errors_do_not_leak_or_retry() {
    for (mode, expected) in [("oversized", "oversized line"), ("auth", "check its ChatGPT sign-in")] {
        let dir = tempfile::tempdir().unwrap();
        let binary = fake(dir.path(), mode, result(None)).await;
        let error = within("invalid Codex reply", codex_limits::read(&binary, c6_testkit::wait::deadline(), &CancellationToken::new()))
            .await
            .unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
        assert!(!error.to_string().contains("secret-error-detail"));
        assert_eq!(requests(dir.path()).len(), if mode == "auth" { 3 } else { 1 });
    }
    let dir = tempfile::tempdir().unwrap();
    let binary = fake(dir.path(), "hang", result(None)).await;
    let stop = CancellationToken::new();
    let token = stop.clone();
    let task = tokio::spawn(async move { codex_limits::read(&binary, c6_testkit::wait::deadline(), &token).await });
    eventually("initialization sent", async || dir.path().join("requests").exists().then_some(())).await;
    stop.cancel();
    let error = within("cancelled app server to be reaped", task).await.unwrap().unwrap_err();
    assert!(error.to_string().contains("cancelled"));
    let pid = std::fs::read_to_string(dir.path().join("pid")).unwrap();
    assert!(nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid.trim().parse().unwrap()), None).is_err());
}

#[tokio::test]
async fn fake_codex_worker_reports_only_after_opt_in_and_matches_exact_accounts() {
    let (dir, state, bins) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let binary = fake(bins.path(), "new", result(Some("account-a"))).await;
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
    // No accidental live ChatGPT endpoint reads by the head's legacy poller.
    head.state.store.set_setting(c6_head::quotas::CHATGPT_USAGE_URL, "http://127.0.0.1:1/unused").await.unwrap();
    for (id, account) in [("matching", "account-a"), ("different", "account-b"), ("legacy", "")] {
        let tokens = json!({"client_id":"c","access_token":"never-used","refresh_token":"r","expires_at_ms":i64::MAX/2,"scope":"openid","workspace":account});
        head.state
            .store
            .insert_connection(&c6_head::store::ConnectionRow {
                id: id.into(),
                name: id.into(),
                kind: "chatgpt".into(),
                base_url: "".into(),
                api_key: Some(head.state.secrets.encrypt(&tokens.to_string()).unwrap()),
                default_model: None,
                context_window: None,
                created_at: 0,
                realms: "[]".into(),
                members: "[]".into(),
                subagent_model: None,
                compact_at: None,
            })
            .await
            .unwrap();
    }
    let enrollment = c6_head::api::new_enrollment(&head.state, Some("test-codex-worker")).await.unwrap();
    enroll::enroll(&enrollment.token).await.unwrap().save(state.path()).unwrap();
    let worker = Worker::open(state.path(), Identity::load(state.path()).unwrap()).await.unwrap();
    {
        let mut o = worker.overrides.lock().unwrap();
        o.codex_bin = Some(binary);
        o.codex_every = Some(Duration::from_secs(3600));
        o.codex_config_every = Some(Duration::from_millis(100));
        o.codex_timeout = Some(c6_testkit::wait::deadline());
    }
    let stop = CancellationToken::new();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let run = tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    within("worker attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    let config = worker.link.call("quotas.codex.config", json!({})).await.unwrap();
    assert_eq!(config["enabled"], false);
    assert!(!bins.path().join("requests").exists(), "off by default");
    let report = codex_limits::parse(&result(Some("account-a"))).unwrap();
    assert!(worker.link.call("quotas.codex.report", serde_json::to_value(&report).unwrap()).await.is_err(), "head enforces opt-in too");
    let response = http
        .put(format!("{api}/settings/codex_limits"))
        .json(&json!({"workers":[worker.identity.meta.worker_id.to_string()]}))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let quotas =
        async || -> Vec<ConnectionQuota> { http.get(format!("{api}/connections/quotas")).send().await.unwrap().json().await.unwrap() };
    let matching = eventually("Codex windows over the worker protocol and HTTP API", async || {
        quotas().await.into_iter().find(|q| q.connection_id == "matching" && q.source == "codex-cli")
    })
    .await;
    assert_eq!(matching.windows.iter().map(|w| (w.name.as_str(), w.used_percent)).collect::<Vec<_>>(), [("5h", 34.0), ("week", 71.0)]);
    assert_eq!(matching.windows[0].resets_at, Some(4_102_441_200_000));
    let origin = matching.origin.unwrap();
    assert_eq!(origin.worker_name, "test-codex-worker");
    assert_eq!(origin.account_id.as_deref(), Some("account-a"));
    assert_eq!(origin.plan_type.as_deref(), Some("plus"));
    assert!(!quotas().await.iter().any(|q| ["different", "legacy"].contains(&q.connection_id.as_str()) && q.source == "codex-cli"));
    eventually("the existing persistence path", async || {
        head.state.store.setting("quotas_last").await.unwrap().filter(|s| s.contains("codex-cli"))
    })
    .await;
    // Shut down the poller before exercising alternate accounts so no fake
    // report can race the assertions below. Its link is closed on exit.
    stop.cancel();
    within("worker shutdown", run).await.unwrap().unwrap();
    for account in [Some("account-other"), None] {
        let report = codex_limits::parse(&result(account)).unwrap();
        c6_head::quotas::from_codex(&head.state, worker.identity.meta.worker_id, report).await.unwrap();
        let unmatched = quotas().await.into_iter().find(|q| q.connection_id.starts_with("codex-cli:")).unwrap();
        assert_eq!(unmatched.origin.unwrap().account_id.as_deref(), account);
        assert_eq!(unmatched.windows.len(), 2);
        assert_eq!(head.state.quotas.get("matching").unwrap().windows[0].used_percent, 34.0);
    }
    let failed = CodexLimitsReport { observed_at: c6_core::now_ms(), error: "Codex not found: set its path".into(), ..Default::default() };
    c6_head::quotas::from_codex(&head.state, worker.identity.meta.worker_id, failed).await.unwrap();
    let unmatched = quotas().await.into_iter().find(|q| q.connection_id.starts_with("codex-cli:")).unwrap();
    assert_eq!(unmatched.windows.len(), 2, "errors retain the last windows");
    assert!(unmatched.error.contains("not found"));
    head.shutdown().await.unwrap();
}
