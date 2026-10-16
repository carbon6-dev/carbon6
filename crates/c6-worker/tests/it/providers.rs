//! OAuth providers declared by packages (docs/knowledge.md §6.2, #2): the
//! head runs the sign-in with your client (realm secrets) at its own
//! redirect address, keeps the refresh token, and hands an app's kit jobs
//! fresh access tokens only. Against a stand-in provider.

use std::sync::{Arc, Mutex};

use axum::extract::Form;
use axum::routing::{get, post};
use c6_api::{Install, OAuthAccount};
use c6_head::HeadConfig;
use serde_json::{Value, json};

#[tokio::test]
async fn apps_sign_in_with_their_providers() {
    // The stand-in: codes and refresh tokens become access tokens.
    let issued = Arc::new(Mutex::new(0u32));
    let n = issued.clone();
    let app = axum::Router::new()
        .route(
            "/token",
            post(move |Form(f): Form<std::collections::HashMap<String, String>>| {
                let n = n.clone();
                async move {
                    assert_eq!(f.get("client_secret").map(String::as_str), Some("s3cret"));
                    let mut k = n.lock().unwrap();
                    *k += 1;
                    let body = match f.get("grant_type").map(String::as_str) {
                        Some("authorization_code") => {
                            assert_eq!(f.get("code").map(String::as_str), Some("the-code"));
                            json!({"access_token": format!("access-{k}"), "refresh_token": "refresh-1", "expires_in": 1})
                        }
                        _ => json!({"access_token": format!("access-{k}"), "expires_in": 3600}),
                    };
                    axum::Json(body)
                }
            }),
        )
        .route("/me", get(|| async { axum::Json(json!({"email": "ada@example.com", "name": "Ada"})) }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await });

    let head_dir = tempfile::tempdir().unwrap();
    let head = c6_head::start(HeadConfig {
        data_dir: head_dir.path().to_owned(),
        bind: vec!["127.0.0.1".parse().unwrap()],
        ui_port: 0,
        worker_port: 0,
        auth: false,
        ..Default::default()
    })
    .await
    .unwrap();
    let api = format!("http://127.0.0.1:{}/api/v1", head.ui_addrs[0].port());
    let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
    use reqwest::Method as M;
    let send = |method: M, path: &str, body: Value| {
        let (http, url) = (http.clone(), format!("{api}/{path}"));
        async move { http.request(method, &url).json(&body).send().await.unwrap() }
    };
    let ok = async |r: reqwest::Response| -> Value {
        let s = r.status();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        assert!(s.is_success(), "{s}: {v}");
        v
    };

    // Checked: the client's secrets are among what it asks for.
    let bad = ok(send(
        M::POST,
        "apps/plan",
        json!({"package": "x", "version": "1.0.0",
        "providers": [{"name": "mail", "authorize_url": format!("{base}/auth"), "token_url": format!("{base}/token"),
                       "client_id_secret": "MAIL_ID", "client_secret_secret": "MAIL_SECRET"}]}),
    )
    .await)
    .await;
    assert!(bad["problems"].to_string().contains("MAIL_ID is among those the package asks for"), "{bad}");

    let pkg = json!({"package": "mailer", "version": "1.0.0",
        "permissions": {"secrets": ["MAIL_ID", "MAIL_SECRET"]},
        "providers": [{"name": "mail", "authorize_url": format!("{base}/auth"), "token_url": format!("{base}/token"),
                       "userinfo_url": format!("{base}/me"), "scopes": ["read", "send"], "params": {"access_type": "offline"},
                       "client_id_secret": "MAIL_ID", "client_secret_secret": "MAIL_SECRET"}]});
    ok(send(M::POST, "packages", json!({"manifest": pkg})).await).await;
    let i: Install = serde_json::from_value(ok(send(M::POST, "installs", json!({"package": "mailer"})).await).await).unwrap();
    ok(send(M::POST, &format!("decisions/{}/answer", i.decision.unwrap()), json!({"option": "Install"})).await).await;

    // Without your client, it says what to set.
    let r = send(M::POST, "apps/home/mailer/providers/mail/connect", json!({})).await;
    assert!(r.text().await.unwrap().contains("set your OAuth client first"));
    ok(send(M::PUT, "settings/secrets/MAIL_ID", json!({"value": "client-1"})).await).await;
    ok(send(M::PUT, "settings/secrets/MAIL_SECRET", json!({"value": "s3cret"})).await).await;

    // You sign in at the provider; it sends you back with a code.
    let start = ok(send(M::POST, "apps/home/mailer/providers/mail/connect", json!({})).await).await;
    let url = reqwest::Url::parse(start["authorize_url"].as_str().unwrap()).unwrap();
    let q: std::collections::HashMap<String, String> = url.query_pairs().into_owned().collect();
    assert_eq!((q["client_id"].as_str(), q["scope"].as_str(), q["access_type"].as_str()), ("client-1", "read send", "offline"));
    assert!(q["redirect_uri"].ends_with("/auth/oauth/callback"), "{q:?}");
    let back = http
        .get(format!("http://127.0.0.1:{}/auth/oauth/callback?code=the-code&state={}", head.ui_addrs[0].port(), q["state"]))
        .send()
        .await
        .unwrap();
    assert!(back.headers()["location"].to_str().unwrap().contains("oauth=connected"), "{:?}", back.headers());
    let accounts: Vec<OAuthAccount> = serde_json::from_value(ok(send(M::GET, "apps/home/mailer/accounts", json!({})).await).await).unwrap();
    assert_eq!((accounts[0].account.as_str(), accounts[0].name.as_str()), ("ada@example.com", "Ada"));

    // Its kit jobs get access tokens (refreshed when expired), never the refresh token.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let env = c6_head::providers::tokens_for(&head.state, "home", "mailer").await.unwrap();
    let list: Value = serde_json::from_str(&env[0].1).unwrap();
    assert_eq!(env[0].0, "C6_ACCOUNTS");
    assert_eq!((list[0]["account"].as_str(), list[0]["access_token"].as_str()), (Some("ada@example.com"), Some("access-2")), "{list}");
    assert!(!env[0].1.contains("refresh-1"));
    // A token still fresh is reused.
    let again = c6_head::providers::tokens_for(&head.state, "home", "mailer").await.unwrap();
    assert!(again[0].1.contains("access-2"));
    assert_eq!(*issued.lock().unwrap(), 2);

    // Disconnecting forgets it.
    ok(send(M::DELETE, &format!("apps/home/mailer/accounts/{}", accounts[0].id), json!({})).await).await;
    assert_eq!(ok(send(M::GET, "apps/home/mailer/accounts", json!({})).await).await, json!([]));
    head.shutdown().await.unwrap();
}
