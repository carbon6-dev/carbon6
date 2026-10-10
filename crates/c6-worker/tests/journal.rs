//! Journal bookkeeping: trimming, lost session rows, restores over leftovers.

use c6_proto::{CreateSession, SessionEvent, SessionStarted, UserMessage, session_event};
use c6_worker::journal::Journal;

fn started() -> SessionEvent {
    SessionEvent { seq: 1, ts_ms: 0, kind: Some(session_event::Kind::Started(SessionStarted::default())) }
}

fn user(seq: u64, command_id: &str) -> SessionEvent {
    SessionEvent {
        seq,
        ts_ms: 0,
        kind: Some(session_event::Kind::User(UserMessage { command_id: command_id.into(), text: "hi".into() })),
    }
}

#[tokio::test]
async fn trim_keeps_recently_active_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let j = Journal::open(&dir.path().join("journal.db")).await.unwrap();
    j.create_session("s", &CreateSession::default(), &started()).await.unwrap();
    j.set_acked("s", 1).await.unwrap();
    // Synced and idle, but active just now: a 7-day cutoff must keep it.
    let week = 7 * 24 * 3600 * 1000;
    assert_eq!(j.trim(c6_core::now_ms() - week, &[]).await.unwrap(), 0);
    assert!(j.has_session("s").await.unwrap());
    // A cutoff in the future trims it.
    assert_eq!(j.trim(c6_core::now_ms() + 1000, &[]).await.unwrap(), 1);
    assert!(!j.has_session("s").await.unwrap());
}

#[tokio::test]
async fn append_without_a_session_row_fails() {
    let dir = tempfile::tempdir().unwrap();
    let j = Journal::open(&dir.path().join("journal.db")).await.unwrap();
    let err = j.append("missing", &user(2, "c")).await.unwrap_err();
    assert!(format!("{err:#}").contains("not in the journal"), "{err:#}");
}

#[tokio::test]
async fn restore_replaces_leftover_events() {
    let dir = tempfile::tempdir().unwrap();
    let j = Journal::open(&dir.path().join("journal.db")).await.unwrap();
    j.create_session("s", &CreateSession::default(), &started()).await.unwrap();
    j.append("s", &user(2, "c1")).await.unwrap();
    // The old bug: the session row is gone but its events remain.
    let db = sqlx::SqlitePool::connect(&format!("sqlite:{}", dir.path().join("journal.db").display())).await.unwrap();
    sqlx::query("DELETE FROM sessions").execute(&db).await.unwrap();
    db.close().await;
    assert_eq!(j.drop_orphans().await.unwrap(), 2);
    assert!(!j.is_applied("c1").await.unwrap());
    j.import("s", &CreateSession::default(), &[started()]).await.unwrap();
    assert!(!j.is_applied("c1").await.unwrap(), "the restored log has no c1, so it must run again");
    let (_, events) = j.load_session("s").await.unwrap().unwrap();
    assert_eq!(events.len(), 1);
}
