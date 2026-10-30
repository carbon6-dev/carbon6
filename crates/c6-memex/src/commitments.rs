//! Commitment ownership, ageing and the decisions that survive another read.

use serde_json::Value;

use crate::{Memex, people::Account};

const DAY: i64 = 86_400_000;
const AGE: i64 = 60 * DAY;
const EXPIRE: &str = "UPDATE commitments INDEXED BY commitments_expiry SET status = 'expired' WHERE status = 'open' \
    AND made_by LIKE 'understand-%' AND decided_at IS NULL AND max(said_at, coalesce(due, said_at)) < ?";

fn words(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

pub(crate) fn who(value: &str, accounts: &[Account]) -> String {
    let value = value.trim().to_lowercase();
    let name = words(&value);
    let address = value.split_once('<').and_then(|(_, rest)| rest.strip_suffix('>')).unwrap_or(&value).trim();
    if matches!(name.as_str(), "me" | "the owner")
        || accounts.iter().any(|a| {
            (!a.email.trim().is_empty() && address == a.email.trim().to_lowercase())
                || (!a.name.trim().is_empty() && name == words(&a.name))
        })
    {
        "me".into()
    } else {
        value
    }
}

fn expired(made_by: &str, said_at: i64, due: Option<i64>, now: i64) -> bool {
    made_by.to_ascii_lowercase().starts_with("understand-") && said_at.max(due.unwrap_or(said_at)) < now.saturating_sub(AGE)
}

pub(crate) fn status(value: &str) -> anyhow::Result<()> {
    anyhow::ensure!(matches!(value, "open" | "done" | "dropped" | "expired"), "a commitment is open, done, dropped or expired");
    Ok(())
}

pub(crate) async fn save(m: &Memex, items: &[String], values: &Value, made_by: &str, now: i64) -> anyhow::Result<()> {
    // Keep replacement together so a decision either precedes it or sees a changed id.
    let mut tx = m.store.pool.begin_with("BEGIN IMMEDIATE").await?;
    save_in(m, &mut tx, items, values, made_by, now).await?;
    tx.commit().await?;
    Ok(())
}

pub(crate) async fn save_in(
    m: &Memex,
    conn: &mut sqlx::SqliteConnection,
    items: &[String],
    values: &Value,
    made_by: &str,
    now: i64,
) -> anyhow::Result<()> {
    let Some(last) = items.last() else { return Ok(()) };
    let accounts = m.accounts();
    let items = serde_json::to_string(items)?;
    let said_at = sqlx::query_scalar::<_, i64>("SELECT created_at FROM items WHERE id = ?")
        .bind(last)
        .fetch_optional(&mut *conn)
        .await?
        .unwrap_or(now);
    sqlx::query("DELETE FROM commitments WHERE item_id IN (SELECT value FROM json_each(?)) AND made_by LIKE 'understand-%' AND status = 'open' AND decided_at IS NULL")
        .bind(&items)
        .execute(&mut *conn)
        .await?;
    for c in values.as_array().into_iter().flatten() {
        let what = c["what"].as_str().unwrap_or("").trim();
        if what.is_empty() {
            continue;
        }
        let due = c["due"]
            .as_str()
            .and_then(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
            .and_then(|d| d.and_hms_opt(17, 0, 0))
            .map(|d| d.and_utc().timestamp_millis());
        let owner = who(c["who"].as_str().unwrap_or("me"), &accounts);
        // An expired promise may be reaffirmed on a newer item; a decision by hand still stands.
        let previous: Vec<String> = sqlx::query_scalar(
            "SELECT who FROM commitments WHERE item_id IN (SELECT value FROM json_each(?1)) AND lower(what) = lower(?2) \
             AND (status != 'expired' OR item_id = ?3 OR decided_at IS NOT NULL)",
        )
        .bind(&items)
        .bind(what)
        .bind(last)
        .fetch_all(&mut *conn)
        .await?;
        if previous.iter().any(|stored| who(stored, &accounts) == owner) {
            continue;
        }
        sqlx::query(
            "INSERT INTO commitments (item_id, who, what, due, made_by, created_at, said_at, status) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(last)
        .bind(owner)
        .bind(what)
        .bind(due)
        .bind(made_by)
        .bind(now)
        .bind(said_at)
        .bind(if expired(made_by, said_at, due, now) { "expired" } else { "open" })
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

pub(crate) async fn set(m: &Memex, id: i64, value: &str, now: i64) -> anyhow::Result<()> {
    status(value)?;
    let changed = sqlx::query("UPDATE commitments SET status = ?, decided_at = ? WHERE id = ?")
        .bind(value)
        .bind(now)
        .bind(id)
        .execute(&m.store.pool)
        .await?
        .rows_affected();
    anyhow::ensure!(changed != 0, "this commitment has changed; reload");
    Ok(())
}

pub(crate) async fn expire(m: &Memex, now: i64) -> anyhow::Result<u64> {
    Ok(sqlx::query(EXPIRE).bind(now.saturating_sub(AGE)).execute(&m.store.pool).await?.rows_affected())
}

async fn cleanup(m: &Memex, now: i64) -> anyhow::Result<()> {
    let done: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM memex_once WHERE name = 'commitment-owners')").fetch_one(&m.store.pool).await?;
    if done {
        return Ok(());
    }
    let accounts = m.accounts();
    // The head sends accounts after startup. Leave the pass pending until they arrive.
    if !accounts.iter().any(|a| !a.email.trim().is_empty() || !a.name.trim().is_empty()) {
        return Ok(());
    }
    let mut tx = m.store.pool.begin_with("BEGIN IMMEDIATE").await?;
    let done: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM memex_once WHERE name = 'commitment-owners')").fetch_one(&mut *tx).await?;
    if done {
        return Ok(());
    }
    let names: Vec<String> = sqlx::query_scalar("SELECT DISTINCT who FROM commitments").fetch_all(&mut *tx).await?;
    for name in names {
        let mapped = who(&name, &accounts);
        if mapped != name {
            sqlx::query("UPDATE commitments SET who = ? WHERE who = ?").bind(mapped).bind(name).execute(&mut *tx).await?;
        }
    }
    sqlx::query("INSERT INTO memex_once (name, at) VALUES ('commitment-owners', ?)").bind(now).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

pub(crate) async fn maintain(m: &Memex, now: i64) -> anyhow::Result<()> {
    expire(m, now).await?;
    cleanup(m, now).await
}

pub(crate) async fn dates(m: &Memex, items: &[String], now: i64) -> anyhow::Result<(String, String)> {
    let last: Option<i64> = sqlx::query_scalar("SELECT MAX(created_at) FROM items WHERE id IN (SELECT value FROM json_each(?))")
        .bind(serde_json::to_string(items)?)
        .fetch_one(&m.store.pool)
        .await?;
    let date = |ms| chrono::DateTime::from_timestamp_millis(ms).map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default();
    Ok((last.map(date).unwrap_or_default(), date(now)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use c6_api as api;
    use serde_json::json;
    use sqlx::Row;

    async fn memex() -> (tempfile::TempDir, std::sync::Arc<Memex>) {
        c6_llm::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let m = Memex::open(dir.path(), Default::default()).await.unwrap();
        (dir, m)
    }

    async fn item(m: &Memex, id: &str, at: i64) -> String {
        m.store
            .put(
                &api::MemexItemIn {
                    kind: "mail".into(),
                    source: "test".into(),
                    external_id: id.into(),
                    text: "Plans for the workshop".into(),
                    created_at: Some(at),
                    ..Default::default()
                },
                "test",
            )
            .await
            .unwrap()
    }

    async fn insert(m: &Memex, item: &str, what: &str, status: &str, said: i64, due: Option<i64>, by: &str) -> i64 {
        sqlx::query_scalar("INSERT INTO commitments (item_id, who, what, status, said_at, due, made_by, created_at) VALUES (?, 'me', ?, ?, ?, ?, ?, ?) RETURNING id")
            .bind(item).bind(what).bind(status).bind(said).bind(due).bind(by).bind(c6_core::now_ms())
            .fetch_one(&m.store.pool).await.unwrap()
    }

    async fn apply(m: &Memex, kind: &str, items: &[String], values: Value) {
        crate::understand::apply(
            m,
            &crate::llm::Task { connection_id: "test".into(), model: "test".into(), effort: None },
            kind,
            items,
            &json!({"summary": "Plans for the workshop.", "commitments": values}),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn saving_maps_only_the_owners_addresses_and_whole_names() {
        let (_dir, m) = memex().await;
        m.accounts.lock().unwrap().push(Account { email: "Rae@Example.net".into(), name: "Rae Quinn".into(), ..Default::default() });
        for kind in ["thread", "session"] {
            let said = c6_core::now_ms() - DAY;
            let id = item(&m, kind, said).await;
            let cases = [
                (" RAE@example.NET ", "me"),
                ("Some Name <rae@example.net>", "me"),
                (" Rae   Quinn ", "me"),
                ("THE OWNER", "me"),
                (" Me ", "me"),
                ("Rae", "rae"),
                ("Other Rae Quinn", "other rae quinn"),
                ("Sam Lee", "sam lee"),
                ("sam@example.net", "sam@example.net"),
                ("Sam <sam@example.net>", "sam <sam@example.net>"),
            ];
            let values: Vec<Value> =
                cases.iter().enumerate().map(|(n, (who, _))| json!({"who":who, "what":format!("Promise {n}")})).collect();
            apply(&m, kind, std::slice::from_ref(&id), json!(values)).await;
            let rows: Vec<(String, i64)> = sqlx::query_as("SELECT who, said_at FROM commitments WHERE item_id = ? ORDER BY id")
                .bind(id)
                .fetch_all(&m.store.pool)
                .await
                .unwrap();
            assert_eq!(rows, cases.iter().map(|(_, who)| (who.to_string(), said)).collect::<Vec<_>>());
        }
    }

    #[tokio::test]
    async fn equal_promises_are_kept_for_each_owner_and_decisions_stay_with_them() {
        let (_dir, m) = memex().await;
        let now = c6_core::now_ms();
        m.accounts.lock().unwrap().push(Account { email: "rae@example.net".into(), name: "Rae Quinn".into(), ..Default::default() });
        let id = item(&m, "plans", now).await;
        let values = json!([
            {"who":"Rae Quinn", "what":"Send availability"},
            {"who":"RAE@example.net", "what":"send availability"},
            {"who":"Pat@example.net", "what":"Send availability"},
            {"who":"PAT@EXAMPLE.NET", "what":"SEND AVAILABILITY"}
        ]);
        save(&m, std::slice::from_ref(&id), &values, "understand-v1", now).await.unwrap();
        let rows: Vec<(i64, String)> =
            sqlx::query_as("SELECT id, who FROM commitments ORDER BY who").fetch_all(&m.store.pool).await.unwrap();
        assert_eq!(rows.iter().map(|(_, who)| who.as_str()).collect::<Vec<_>>(), ["me", "pat@example.net"]);
        for status in ["done", "expired"] {
            set(&m, rows[0].0, status, now).await.unwrap();
            save(&m, std::slice::from_ref(&id), &values, "understand-v1", now).await.unwrap();
            let kept: Vec<(i64, String, String, Option<i64>)> =
                sqlx::query_as("SELECT id, who, status, decided_at FROM commitments ORDER BY who").fetch_all(&m.store.pool).await.unwrap();
            assert_eq!(kept.len(), 2);
            assert_eq!(kept[0], (rows[0].0, "me".into(), status.into(), Some(now)));
            assert_eq!((kept[1].1.as_str(), kept[1].2.as_str(), kept[1].3), ("pat@example.net", "open", None));
        }
    }

    #[tokio::test]
    async fn decided_legacy_owner_aliases_block_rereads_before_cleanup() {
        let (_dir, m) = memex().await;
        let now = c6_core::now_ms();
        m.accounts.lock().unwrap().push(Account { email: "rae@example.net".into(), name: "Rae Quinn".into(), ..Default::default() });
        for alias in ["RAE@example.NET", "Rae   Quinn", "Some Name <rae@example.net>"] {
            for status in ["done", "dropped", "expired"] {
                let item = item(&m, &format!("{alias}:{status}"), now).await;
                let id = insert(&m, &item, "Send availability", status, now, None, "understand-v1").await;
                sqlx::query("UPDATE commitments SET who = ?, decided_at = ? WHERE id = ?")
                    .bind(alias)
                    .bind(now)
                    .bind(id)
                    .execute(&m.store.pool)
                    .await
                    .unwrap();
                save(&m, std::slice::from_ref(&item), &json!([{"who":"me", "what":"send availability"}]), "understand-v1", now)
                    .await
                    .unwrap();
                let rows: Vec<(i64, String, String, Option<i64>)> =
                    sqlx::query_as("SELECT id, who, status, decided_at FROM commitments WHERE item_id = ?")
                        .bind(&item)
                        .fetch_all(&m.store.pool)
                        .await
                        .unwrap();
                assert_eq!(rows, [(id, alias.into(), status.into(), Some(now))]);
            }
        }
        let cleaned: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM memex_once WHERE name = 'commitment-owners')")
            .fetch_one(&m.store.pool)
            .await
            .unwrap();
        assert!(!cleaned);
    }

    #[tokio::test]
    async fn migration_backfills_the_source_date_or_the_original_read_time() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::raw_sql(
            "CREATE TABLE items (id TEXT PRIMARY KEY, created_at INTEGER NOT NULL); \
            CREATE TABLE commitments (item_id TEXT, created_at INTEGER, status TEXT, made_by TEXT, due INTEGER); \
            INSERT INTO items VALUES ('present', 100); \
            INSERT INTO commitments VALUES ('present', 900, 'open', 'understand-v1', NULL), ('gone', 800, 'open', 'understand-v1', NULL);",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::raw_sql(include_str!("../migrations/0026_commitment_age.sql")).execute(&pool).await.unwrap();
        let dates: Vec<(i64, Option<i64>)> =
            sqlx::query_as("SELECT said_at, decided_at FROM commitments ORDER BY item_id").fetch_all(&pool).await.unwrap();
        assert_eq!(dates, [(800, None), (100, None)]);
    }

    #[tokio::test]
    async fn expiry_uses_the_later_date_and_a_strict_sixty_days() {
        let (_dir, m) = memex().await;
        let now = 200 * DAY;
        for (name, said, due, by, expires) in [
            ("just under", now - AGE + 1, None, "understand-v1", false),
            ("exactly", now - AGE, None, "understand-v1", false),
            ("just over", now - AGE - 1, None, "understand-v1", true),
            ("due just under", now - 100 * DAY, Some(now - AGE + 1), "understand-v1", false),
            ("due exactly", now - 100 * DAY, Some(now - AGE), "understand-v1", false),
            ("due just over", now - 100 * DAY, Some(now - AGE - 1), "understand-v1", true),
            ("recently reaffirmed", now - DAY, Some(now - 100 * DAY), "understand-v1", false),
            ("not pipeline", 0, None, "session:s1", false),
        ] {
            insert(&m, "source", name, "open", said, due, by).await;
            assert_eq!(expired(by, said, due, now), expires, "{name}");
        }
        assert_eq!(expire(&m, now).await.unwrap(), 2);
        let gone: Vec<String> = sqlx::query_scalar("SELECT what FROM commitments WHERE status = 'expired' ORDER BY what")
            .fetch_all(&m.store.pool)
            .await
            .unwrap();
        assert_eq!(gone, ["due just over", "just over"]);
        let plan = sqlx::query(sqlx::AssertSqlSafe(format!("EXPLAIN QUERY PLAN {EXPIRE}")))
            .bind(now - AGE)
            .fetch_all(&m.store.pool)
            .await
            .unwrap();
        let plan = plan.iter().map(|r| r.get::<String, _>("detail")).collect::<Vec<_>>().join("\n");
        assert!(plan.contains("SEARCH commitments USING INDEX commitments_expiry"), "{plan}");
    }

    #[tokio::test]
    async fn old_promises_are_expired_when_saved_unless_the_due_date_is_recent() {
        let (_dir, m) = memex().await;
        let now = 200 * DAY;
        let id = item(&m, "old", now - 100 * DAY).await;
        let due = chrono::DateTime::from_timestamp_millis(now + DAY).unwrap().format("%Y-%m-%d").to_string();
        save(
            &m,
            &[id],
            &json!([
                {"what":"Undated old promise"}, {"what":"Still due", "due":due}
            ]),
            "understand-v1",
            now,
        )
        .await
        .unwrap();
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT what, status FROM commitments ORDER BY id").fetch_all(&m.store.pool).await.unwrap();
        assert_eq!(rows, [("Undated old promise".into(), "expired".into()), ("Still due".into(), "open".into())]);
    }

    #[tokio::test]
    async fn rereading_keeps_decisions_and_only_renews_expired_promises_on_new_mail() {
        let (_dir, m) = memex().await;
        let now = c6_core::now_ms();
        let old = item(&m, "old", now - 100 * DAY).await;
        let newer = item(&m, "new", now).await;
        let mut kept = Vec::new();
        for status in ["done", "dropped", "expired"] {
            kept.push(insert(&m, &old, status, status, now - 100 * DAY, None, "understand-v1").await);
        }
        let reopened = insert(&m, &old, "reopened", "expired", now - 100 * DAY, None, "understand-v1").await;
        crate::understand::set_commitment(&m, reopened, "open").await.unwrap();
        let manual = insert(&m, &old, "manual expired", "open", now, None, "understand-v1").await;
        crate::understand::set_commitment(&m, manual, "expired").await.unwrap();
        let other = insert(&m, &old, "other open", "open", now, None, "session:s1").await;
        let replaced = insert(&m, &old, "replace", "open", now, None, "understand-v1").await;
        let values = json!([{"what":"done"}, {"what":"dropped"}, {"what":"expired"}, {"what":"reopened"},
            {"what":"manual expired"}, {"what":"other open"}, {"what":"replace"}]);
        expire(&m, now).await.unwrap();
        apply(&m, "thread", std::slice::from_ref(&old), values.clone()).await;
        assert_eq!(sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM commitments").fetch_one(&m.store.pool).await.unwrap(), 7);
        apply(&m, "thread", &[old.clone(), newer.clone()], values).await;
        for id in kept.into_iter().chain([reopened, manual, other]) {
            assert!(
                sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM commitments WHERE id = ?)")
                    .bind(id)
                    .fetch_one(&m.store.pool)
                    .await
                    .unwrap()
            );
        }
        assert!(
            !sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM commitments WHERE id = ?)")
                .bind(replaced)
                .fetch_one(&m.store.pool)
                .await
                .unwrap()
        );
        let (status, decided): (String, Option<i64>) = sqlx::query_as("SELECT status, decided_at FROM commitments WHERE id = ?")
            .bind(reopened)
            .fetch_one(&m.store.pool)
            .await
            .unwrap();
        assert_eq!(status, "open");
        assert!(decided.is_some());
        let renewed: Vec<(String, String)> = sqlx::query_as("SELECT what, status FROM commitments WHERE item_id = ? ORDER BY what")
            .bind(newer)
            .fetch_all(&m.store.pool)
            .await
            .unwrap();
        assert_eq!(renewed, [("expired".into(), "open".into()), ("replace".into(), "open".into())]);
    }

    #[tokio::test]
    async fn open_order_uses_due_then_said_and_history_uses_said() {
        let (_dir, m) = memex().await;
        let id = item(&m, "mail", 100).await;
        for (what, said, due) in [
            ("old undated", 10, None),
            ("due later", 40, Some(300)),
            ("overdue", 30, Some(1)),
            ("new undated", 50, None),
            ("same date", 50, None),
        ] {
            insert(&m, &id, what, "open", said, due, "test").await;
        }
        let list = crate::understand::commitments(&m, "open", &["mail".into()]).await.unwrap();
        assert_eq!(
            list.iter().map(|c| c.what.as_str()).collect::<Vec<_>>(),
            ["overdue", "due later", "same date", "new undated", "old undated"]
        );
        assert_eq!(list[0].said_at, 30);
        sqlx::query("UPDATE commitments SET status = 'expired'").execute(&m.store.pool).await.unwrap();
        let list = crate::understand::commitments(&m, "expired", &["mail".into()]).await.unwrap();
        assert_eq!(
            list.iter().map(|c| c.what.as_str()).collect::<Vec<_>>(),
            ["same date", "new undated", "due later", "overdue", "old undated"]
        );
        assert!(crate::understand::commitments(&m, "unknown", &[]).await.is_err());
        assert!(crate::understand::set_commitment(&m, list[0].id, "unknown").await.is_err());
    }

    #[tokio::test]
    async fn owner_cleanup_waits_for_accounts_and_runs_once_for_all_statuses() {
        let (_dir, m) = memex().await;
        for status in ["open", "done", "dropped", "expired"] {
            insert(&m, "mail", status, status, 1, None, "test").await;
        }
        sqlx::query("UPDATE commitments SET who = CASE status WHEN 'open' THEN 'Rae <RAE@example.net>' WHEN 'done' THEN 'rae@example.net' ELSE 'Rae   Quinn' END")
            .execute(&m.store.pool).await.unwrap();
        cleanup(&m, 10).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM memex_once WHERE name = 'commitment-owners'")
                .fetch_one(&m.store.pool)
                .await
                .unwrap(),
            0
        );
        m.accounts.lock().unwrap().push(Account { email: "rae@example.net".into(), name: "Rae Quinn".into(), ..Default::default() });
        cleanup(&m, 20).await.unwrap();
        let names: Vec<String> = sqlx::query_scalar("SELECT who FROM commitments").fetch_all(&m.store.pool).await.unwrap();
        assert_eq!(names, ["me", "me", "me", "me"]);
        sqlx::query("UPDATE commitments SET who = 'rae@example.net' WHERE status = 'done'").execute(&m.store.pool).await.unwrap();
        cleanup(&m, 30).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT who FROM commitments WHERE status = 'done'").fetch_one(&m.store.pool).await.unwrap(),
            "rae@example.net"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT at FROM memex_once WHERE name = 'commitment-owners'")
                .fetch_one(&m.store.pool)
                .await
                .unwrap(),
            20
        );
    }

    #[tokio::test]
    async fn thread_prompt_dates_do_not_require_an_edited_prompt_to_use_them() {
        let (_dir, m) = memex().await;
        let early = item(&m, "early", 0).await;
        let last = item(&m, "last", DAY).await;
        let (last, today) = dates(&m, &[last, early], 3 * DAY).await.unwrap();
        assert_eq!((last.as_str(), today.as_str()), ("1970-01-02", "1970-01-04"));
        let fields = [("owner", "rae@example.net"), ("last", last.as_str()), ("today", today.as_str())];
        assert_eq!(api::prompts::fill("Owner: {{owner}}", &fields), "Owner: rae@example.net");
        let prompt = api::prompts::fill(api::prompts::UNDERSTAND_THREAD, &fields);
        assert!(prompt.contains("1970-01-02") && prompt.contains("1970-01-04"));
    }

    #[tokio::test]
    async fn rereading_keeps_a_decision_committed_before_it_starts() {
        let (_dir, m) = memex().await;
        let now = c6_core::now_ms();
        let item = item(&m, "plans", now).await;
        let id = insert(&m, &item, "Send the figures", "open", now, None, "understand-v1").await;
        set(&m, id, "done", now).await.unwrap();
        save(&m, &[item], &json!([{"what":"Send the figures"}]), "understand-v1", now).await.unwrap();
        let rows: Vec<(i64, String, Option<i64>)> =
            sqlx::query_as("SELECT id, status, decided_at FROM commitments").fetch_all(&m.store.pool).await.unwrap();
        assert_eq!(rows, [(id, "done".into(), Some(now))]);
    }

    #[tokio::test]
    async fn a_decision_during_replacement_waits_then_reports_the_changed_id() {
        let (_dir, m) = memex().await;
        let now = c6_core::now_ms();
        let item = item(&m, "plans", now).await;
        let id = insert(&m, &item, "Send the figures", "open", now, None, "understand-v1").await;
        let mut tx = m.store.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
        save_in(&m, &mut tx, &[item], &json!([{"what":"Send the figures"}]), "understand-v1", now).await.unwrap();
        let rows: Vec<(i64, String, Option<i64>)> =
            sqlx::query_as("SELECT id, status, decided_at FROM commitments").fetch_all(&mut *tx).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_ne!(rows[0].0, id);

        let (started, ready) = tokio::sync::oneshot::channel();
        let setter = tokio::spawn({
            let m = m.clone();
            async move {
                started.send(()).unwrap();
                crate::understand::set_commitment(&m, id, "done").await
            }
        });
        c6_testkit::within("the setter starts", ready).await.unwrap();
        for _ in 0..5 {
            tokio::task::yield_now().await;
            assert!(!setter.is_finished(), "the decision must wait for the replacement transaction");
        }
        tx.commit().await.unwrap();
        let error = c6_testkit::within("the setter after replacement commits", setter).await.unwrap().unwrap_err();
        assert!(error.to_string().contains("this commitment has changed; reload"));
        let saved: Vec<(i64, String, Option<i64>)> =
            sqlx::query_as("SELECT id, status, decided_at FROM commitments").fetch_all(&m.store.pool).await.unwrap();
        assert_eq!(saved, [(rows[0].0, "open".into(), None)]);
    }

    #[tokio::test]
    async fn a_session_reread_as_a_document_keeps_only_decided_commitments() {
        let (_dir, m) = memex().await;
        let mut input = api::MemexItemIn {
            kind: "session".into(),
            source: "test".into(),
            external_id: "plans".into(),
            text: "Plans for the workshop".into(),
            created_at: Some(c6_core::now_ms()),
            ..Default::default()
        };
        let id = m.store.put(&input, "test").await.unwrap();
        apply(
            &m,
            "session",
            std::slice::from_ref(&id),
            json!([
                {"what":"Open promise"}, {"what":"Finished promise"}, {"what":"Reopened promise"}
            ]),
        )
        .await;
        let rows: Vec<(String, i64)> =
            sqlx::query_as("SELECT what, id FROM commitments ORDER BY what").fetch_all(&m.store.pool).await.unwrap();
        crate::understand::set_commitment(&m, rows[0].1, "done").await.unwrap();
        crate::understand::set_commitment(&m, rows[2].1, "expired").await.unwrap();
        crate::understand::set_commitment(&m, rows[2].1, "open").await.unwrap();
        input.kind = "document".into();
        assert_eq!(m.store.put(&input, "test").await.unwrap(), id);
        apply(&m, "document", &[id], json!([{"what":"Not a session promise"}])).await;
        let kept: Vec<(i64, String)> =
            sqlx::query_as("SELECT id, status FROM commitments ORDER BY what").fetch_all(&m.store.pool).await.unwrap();
        assert_eq!(kept, [(rows[0].1, "done".into()), (rows[2].1, "open".into())]);
    }

    #[tokio::test]
    async fn a_decision_on_a_missing_or_replaced_id_fails() {
        let (_dir, m) = memex().await;
        let now = c6_core::now_ms();
        let item = item(&m, "plans", now).await;
        let id = insert(&m, &item, "Send the figures", "open", now, None, "understand-v1").await;
        save(&m, &[item], &json!([{"what":"Send the figures"}]), "understand-v1", now).await.unwrap();
        for id in [id, -1] {
            let error = crate::understand::set_commitment(&m, id, "done").await.unwrap_err();
            assert!(error.to_string().contains("this commitment has changed; reload"));
        }
        let statuses: Vec<(String, Option<i64>)> =
            sqlx::query_as("SELECT status, decided_at FROM commitments").fetch_all(&m.store.pool).await.unwrap();
        assert_eq!(statuses, [("open".into(), None)]);
    }
}
