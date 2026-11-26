//! Stable connection IDs whose concrete API account can change between calls.
use c6_core::now_ms;
use sqlx::{FromRow, SqliteConnection};

use crate::store::{ConnectionRow, Store};
use crate::{bad_request, not_found};

pub(crate) const KIND: &str = "alias";

#[derive(FromRow)]
pub(crate) struct Alias {
    pub alias_id: String,
    pub target_id: String,
    pub target_kind: String,
    pub switched_at: i64,
}

async fn target(db: &mut SqliteConnection, id: &str) -> anyhow::Result<ConnectionRow> {
    let target: ConnectionRow = sqlx::query_as("SELECT * FROM connections WHERE id = ?")
        .bind(id.trim())
        .fetch_optional(db)
        .await?
        .ok_or_else(|| bad_request("no such alias target"))?;
    if !matches!(target.kind.as_str(), "openai" | "openai_compat" | "anthropic" | "chatgpt") {
        return Err(bad_request("an alias must target an OpenAI, OpenAI-compatible, Anthropic, or ChatGPT connection"));
    }
    Ok(target)
}

async fn record(db: &mut SqliteConnection, alias: &Alias, previous: Option<&str>, by: &str) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO connection_alias_history (alias_id, previous_target_id, target_id, switched_at, changed_by) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&alias.alias_id)
    .bind(previous)
    .bind(&alias.target_id)
    .bind(alias.switched_at)
    .bind(by)
    .execute(db)
    .await?;
    Ok(())
}

pub(crate) async fn create(store: &Store, row: &ConnectionRow, target_id: &str, by: &str) -> anyhow::Result<Alias> {
    let mut tx = store.pool.begin_with("BEGIN IMMEDIATE").await?;
    let target = target(&mut tx, target_id).await?;
    let alias = Alias { alias_id: row.id.clone(), target_id: target.id, target_kind: target.kind, switched_at: row.created_at };
    Store::insert_connection_on(&mut tx, row).await?;
    sqlx::query("INSERT INTO connection_aliases (alias_id, target_id, target_kind, switched_at) VALUES (?, ?, ?, ?)")
        .bind(&alias.alias_id)
        .bind(&alias.target_id)
        .bind(&alias.target_kind)
        .bind(alias.switched_at)
        .execute(&mut *tx)
        .await?;
    record(&mut tx, &alias, None, by).await?;
    tx.commit().await?;
    Ok(alias)
}

pub(crate) async fn switch(store: &Store, id: &str, target_id: &str, by: &str) -> anyhow::Result<Alias> {
    let mut tx = store.pool.begin_with("BEGIN IMMEDIATE").await?;
    let mut alias: Alias = sqlx::query_as("SELECT * FROM connection_aliases WHERE alias_id = ?")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| not_found("no such connection alias"))?;
    let target = target(&mut tx, target_id).await?;
    if target.kind != alias.target_kind {
        return Err(bad_request(format!("this alias must target a {} connection", alias.target_kind)));
    }
    if alias.target_id != target.id {
        let previous = std::mem::replace(&mut alias.target_id, target.id);
        alias.switched_at = now_ms();
        sqlx::query("UPDATE connection_aliases SET target_id = ?, switched_at = ? WHERE alias_id = ?")
            .bind(&alias.target_id)
            .bind(alias.switched_at)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        record(&mut tx, &alias, Some(&previous), by).await?;
    }
    tx.commit().await?;
    Ok(alias)
}

pub(crate) async fn get(store: &Store, id: &str) -> anyhow::Result<Option<Alias>> {
    Ok(sqlx::query_as("SELECT * FROM connection_aliases WHERE alias_id = ?").bind(id).fetch_optional(&store.pool).await?)
}

pub(crate) async fn record_grant(
    store: &Store,
    worker: c6_core::WorkerId,
    root: &str,
    grant: &c6_proto::CredentialGrant,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO connection_alias_grants (root_session_id, worker_id, alias_id, target_id, alias_name, target_name) \
         VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT (root_session_id, worker_id, alias_id, target_id) DO UPDATE SET \
         alias_name = excluded.alias_name, target_name = excluded.target_name",
    )
    .bind(root)
    .bind(worker.to_string())
    .bind(&grant.connection_id)
    .bind(&grant.resolved_connection_id)
    .bind(&grant.alias_connection_name)
    .bind(&grant.resolved_connection_name)
    .execute(&store.pool)
    .await?;
    Ok(())
}

/// A saved prefix may outlive an alias switch. Retain only an account already
/// granted to this worker/root, through an alias the same-realm tree still
/// uses. Revoking either the alias's own availability or the concrete account
/// revokes this permission; the alias's NEW target is deliberately irrelevant.
pub(crate) async fn prior_target_allowed(
    store: &Store,
    session: &crate::store::SessionRow,
    target: &ConnectionRow,
) -> anyhow::Result<bool> {
    if session.status == "failed" || !crate::realms::available(&target.realms, &session.realm) {
        return Ok(false);
    }
    let aliases: Vec<String> = sqlx::query_scalar(
        "SELECT a.realms FROM connection_alias_grants g \
         JOIN connections a ON a.id = g.alias_id AND a.kind = 'alias' \
         JOIN connection_aliases d ON d.alias_id = a.id AND d.target_kind = ? \
         WHERE g.root_session_id = ? AND g.worker_id = ? AND g.target_id = ? \
         AND EXISTS (SELECT 1 FROM sessions s WHERE (s.id = g.root_session_id OR s.root_id = g.root_session_id) \
                     AND s.connection_id = a.id AND s.realm = ? AND s.status != 'failed')",
    )
    .bind(&target.kind)
    .bind(session.root_id.as_deref().unwrap_or(&session.id))
    .bind(&session.worker_id)
    .bind(&target.id)
    .bind(&session.realm)
    .fetch_all(&store.pool)
    .await?;
    Ok(aliases.iter().any(|realms| crate::realms::available(realms, &session.realm)))
}

pub(crate) async fn verify_usage(db: &mut SqliteConnection, session_id: &str, usage: &mut c6_proto::Usage) -> anyhow::Result<()> {
    if !usage.resolved_connection_id.is_empty() {
        let names: Option<(String, String)> = sqlx::query_as(
            "SELECT g.alias_name, g.target_name FROM sessions s JOIN connection_alias_grants g \
             ON g.root_session_id = COALESCE(s.root_id, s.id) AND g.worker_id = s.worker_id AND g.alias_id = s.connection_id \
             WHERE s.id = ? AND g.target_id = ?",
        )
        .bind(session_id)
        .bind(&usage.resolved_connection_id)
        .fetch_optional(&mut *db)
        .await?;
        if let Some((alias, target)) = names {
            usage.alias_connection_name = alias;
            usage.resolved_connection_name = target;
            return Ok(());
        }
        // Maintenance calls pin a direct connection too. Keep that actual
        // account only when it is this session's selected concrete connection;
        // never accept another session's grant or worker-supplied display name.
        if usage.keep_warm {
            let direct: Option<(String, String, String)> = sqlx::query_as(
                "SELECT c.name, c.realms, s.realm FROM sessions s JOIN connections c ON c.id = s.connection_id \
                 WHERE s.id = ? AND c.id = ? AND c.kind NOT IN ('alias', 'pool')",
            )
            .bind(session_id)
            .bind(&usage.resolved_connection_id)
            .fetch_optional(&mut *db)
            .await?;
            if let Some((name, realms, realm)) = direct
                && crate::realms::available(&realms, &realm)
            {
                usage.resolved_connection_name = name;
                usage.alias_connection_name.clear();
                return Ok(());
            }
        }
    }
    usage.resolved_connection_id.clear();
    usage.resolved_connection_name.clear();
    usage.alias_connection_name.clear();
    Ok(())
}

pub(crate) async fn history(store: &Store, id: &str) -> anyhow::Result<Vec<c6_api::ConnectionAliasChange>> {
    let changes: Vec<(Option<String>, String, i64, String)> = sqlx::query_as(
        "SELECT previous_target_id, target_id, switched_at, changed_by FROM connection_alias_history WHERE alias_id = ? ORDER BY seq DESC",
    )
    .bind(id)
    .fetch_all(&store.pool)
    .await?;
    if changes.is_empty() {
        return Err(not_found("no such connection alias"));
    }
    Ok(changes
        .into_iter()
        .map(|(previous_target_id, target_id, switched_at, changed_by)| c6_api::ConnectionAliasChange {
            previous_target_id,
            target_id,
            switched_at,
            changed_by,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;
    use serde_json::{Value, json};

    #[test]
    fn alias_management_is_admin_only_and_not_available_to_agents() {
        for op in ["connections.alias.switch", "connections.alias.history"] {
            assert_eq!(crate::catalog::scope(op), Some(crate::catalog::Scope::Admin));
            assert_eq!(crate::agent_ops::class(crate::catalog::find(op).unwrap()), crate::agent_ops::Class::Never);
        }
    }

    #[tokio::test]
    async fn alias_crud_and_next_head_call_use_the_real_account() {
        use axum::{
            Json, Router,
            http::HeaderMap,
            routing::{get, post},
        };

        let provider = Router::new()
            .route(
                "/v1/models",
                get(|headers: HeaderMap| async move {
                    let account = headers["authorization"].to_str().unwrap().trim_start_matches("Bearer ");
                    Json(json!({"data": [{"id": format!("model-{account}") }]}))
                }),
            )
            .route(
                "/v1/chat/completions",
                post(|headers: HeaderMap| async move {
                    let account = headers["authorization"].to_str().unwrap().trim_start_matches("Bearer ");
                    let chunk = json!({"choices": [{"delta": {"content": account}, "finish_reason": "stop"}]});
                    ([("content-type", "text/event-stream")], format!("data: {chunk}\n\ndata: [DONE]\n\n"))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let provider_url = format!("http://{}/v1", listener.local_addr().unwrap());
        let provider = tokio::spawn(async move { axum::serve(listener, provider).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let head = crate::start(crate::HeadConfig {
            data_dir: dir.path().to_owned(),
            bind: vec!["127.0.0.1".parse().unwrap()],
            ui_port: 0,
            worker_port: 0,
            auth: false,
            ..Default::default()
        })
        .await
        .unwrap();
        let base = format!("http://{}/api/v1/connections", head.ui_addrs[0]);
        let http = reqwest::Client::new();
        let add = async |body: Value| -> Value {
            http.post(&base).json(&body).send().await.unwrap().error_for_status().unwrap().json().await.unwrap()
        };
        let a =
            add(json!({"name": "a", "kind": "openai_compat", "base_url": provider_url, "api_key": "one", "default_model": "model-one"}))
                .await;
        let b =
            add(json!({"name": "b", "kind": "openai_compat", "base_url": provider_url, "api_key": "two", "default_model": "model-two"}))
                .await;
        let other = add(json!({"name": "other", "kind": "anthropic", "base_url": provider_url, "api_key": "three"})).await;
        let pool = add(json!({"name": "pool", "kind": "pool", "members": [{"connection_id": a["id"], "slots": 1}]})).await;
        let alias = add(json!({"name": "Stable", "kind": "alias", "alias_target_id": a["id"]})).await;
        assert_eq!(alias["alias_target_kind"], "openai_compat");
        assert_eq!(alias["alias_target_id"], a["id"]);
        assert_eq!(alias["default_model"], "model-one");
        assert_eq!(alias["has_api_key"], false);
        let id = alias["id"].as_str().unwrap();
        let old = crate::sessions::credential_for(
            &head.state,
            Some(c6_core::WorkerId::new()),
            "old-worker",
            c6_proto::CredentialRequest { connection_id: id.into(), ..Default::default() },
        )
        .await;
        assert!(old.error.contains("update this worker"));
        assert!(old.bearer.is_empty());

        // A worker may ask on behalf of a root while its child uses the alias.
        // The authorization must not let the root's direct calls claim it.
        let store = &head.state.store;
        let worker = c6_core::WorkerId::new();
        let a_id = a["id"].as_str().unwrap();
        let b_id = b["id"].as_str().unwrap();
        let other_id = other["id"].as_str().unwrap();
        sqlx::query("UPDATE connections SET realms = '[\"other\"]' WHERE id = ?").bind(other_id).execute(&store.pool).await.unwrap();
        for (session, connection, root) in [("root", a_id, None), ("child", id, Some("root"))] {
            sqlx::query(
                "INSERT INTO sessions (id, worker_id, title, cwd, connection_id, model, status, created_at, updated_at, realm, root_id) \
                 VALUES (?, ?, '', '', ?, 'model', 'idle', 0, 0, 'home', ?)",
            )
            .bind(session)
            .bind(worker.to_string())
            .bind(connection)
            .bind(root)
            .execute(&store.pool)
            .await
            .unwrap();
        }
        let granted = crate::sessions::credential_for(
            &head.state,
            Some(worker),
            "worker",
            c6_proto::CredentialRequest { connection_id: id.into(), session_id: "root".into(), supports_alias: true, ..Default::default() },
        )
        .await;
        assert!(granted.error.is_empty(), "{}", granted.error);
        assert_eq!(granted.resolved_connection_id, a_id);

        let switch = async |target: &Value| {
            http.post(format!("{base}/{id}/alias/switch")).json(&json!({"target_id": target["id"]})).send().await.unwrap()
        };
        for invalid in [&pool, &other, &alias] {
            assert_eq!(switch(invalid).await.status(), 400);
        }
        assert_eq!(http.delete(format!("{base}/{}", a["id"].as_str().unwrap())).send().await.unwrap().status(), 409);
        let models = async || -> Vec<String> {
            http.get(format!("{base}/{id}/models")).send().await.unwrap().error_for_status().unwrap().json().await.unwrap()
        };
        assert_eq!(models().await, ["model-one"]);
        let client = crate::pools::client(&head.state, id, "home", Default::default()).await.unwrap();
        let req = c6_llm::ModelRequest {
            model: "model".into(),
            system: String::new(),
            messages: vec![],
            tools: vec![],
            max_output_tokens: None,
            cache_key: None,
            effort: None,
            web_search: false,
            json: false,
        };
        let account = async |expected: &Value, text: &str| {
            let mut stream = client.stream(&req).await.unwrap();
            let Some(Ok(c6_llm::ModelEvent::Account(a))) = stream.next().await else { panic!("no account metadata") };
            assert_eq!(a.resolved_connection_id, expected["id"].as_str().unwrap());
            assert_eq!(a.resolved_connection_name, expected["name"].as_str().unwrap());
            assert_eq!(a.alias_connection_name, "Stable");
            let mut result = c6_llm::Accumulator::default();
            while let Some(event) = stream.next().await {
                result.push(&event.unwrap());
            }
            assert!(result.finish(true).blocks.iter().any(|b| b.text() == Some(text)));
        };
        account(&a, "one").await;
        let switched: Value = switch(&b).await.error_for_status().unwrap().json().await.unwrap();
        assert_eq!(switched["id"], id);
        assert_eq!(switched["alias_target_id"], b["id"]);
        let listed: Vec<Value> = http.get(&base).send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
        assert_eq!(listed.iter().find(|c| c["id"] == id).unwrap()["default_model"], "model-two");
        account(&b, "two").await;
        assert_eq!(models().await, ["model-two"]);
        switch(&b).await.error_for_status().unwrap();
        assert_eq!(http.delete(format!("{base}/{}", a["id"].as_str().unwrap())).send().await.unwrap().status(), 204);

        // The old account is still legitimate after switching and deleting it.
        // The current account was never granted to this worker, and a direct
        // session cannot claim either a restricted account or its child's grant.
        use c6_proto::session_event::Kind;
        for (session, seq, claimed, accepted) in
            [("root", 1, other_id, false), ("root", 2, a_id, false), ("child", 1, b_id, false), ("child", 2, a_id, true)]
        {
            let event = c6_proto::SessionEvent {
                seq,
                ts_ms: 1,
                kind: Some(Kind::Usage(c6_proto::Usage {
                    input_tokens: 1,
                    resolved_connection_id: claimed.into(),
                    resolved_connection_name: "spoofed account".into(),
                    alias_connection_name: "spoofed alias".into(),
                    ..Default::default()
                })),
            };
            let ingested = store.ingest(worker, session, &[event]).await.unwrap().unwrap();
            let Some(Kind::Usage(usage)) = &ingested.events[0].kind else { panic!("expected usage") };
            let expected = if accepted { (a_id, "a", "Stable") } else { ("", "", "") };
            assert_eq!(
                (usage.resolved_connection_id.as_str(), usage.resolved_connection_name.as_str(), usage.alias_connection_name.as_str()),
                expected,
            );
            let accounted: String = sqlx::query_scalar("SELECT connection_id FROM usage WHERE session_id = ? AND seq = ?")
                .bind(session)
                .bind(seq as i64)
                .fetch_one(&store.pool)
                .await
                .unwrap();
            assert_eq!(accounted, if accepted || session == "root" { a_id } else { id });
        }

        let changes: Vec<Value> =
            http.get(format!("{base}/{id}/alias/history")).send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0]["previous_target_id"], a["id"]);
        assert_eq!(changes[0]["target_id"], b["id"]);
        assert_eq!(changes[0]["changed_by"], "user");
        assert_eq!(changes[1]["target_id"], a["id"]);

        // A client constructed before the restriction cannot bypass it, nor
        // can any realm outside the intersection of the alias and target.
        sqlx::query("UPDATE connections SET realms = '[\"other\"]' WHERE id = ?")
            .bind(b["id"].as_str().unwrap())
            .execute(&head.state.store.pool)
            .await
            .unwrap();
        let Err(err) = client.stream(&req).await else { panic!("unauthorized call") };
        assert!(err.to_string().contains("not available to realm home"));
        assert!(crate::sessions::head_credential(&head.state, "other", id).await.error.is_empty());
        sqlx::query("UPDATE connections SET realms = '[\"home\"]' WHERE id = ?").bind(id).execute(&head.state.store.pool).await.unwrap();
        assert!(!crate::sessions::head_credential(&head.state, "other", id).await.error.is_empty());
        assert!(!crate::sessions::head_credential(&head.state, "home", id).await.error.is_empty());

        assert_eq!(http.delete(format!("{base}/{id}")).send().await.unwrap().status(), 204);
        assert_eq!(http.delete(format!("{base}/{}", b["id"].as_str().unwrap())).send().await.unwrap().status(), 204);
        head.shutdown().await.unwrap();
        provider.abort();
    }
}
