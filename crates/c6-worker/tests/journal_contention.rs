use std::sync::Arc;
use std::time::Duration;

use c6_proto::{CreateSession, SessionEvent, SessionStarted, UserMessage, session_event};
use c6_worker::journal::Journal;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{Connection, SqliteConnection};
use tokio::sync::Barrier;

fn started() -> SessionEvent {
    SessionEvent { seq: 1, kind: Some(session_event::Kind::Started(SessionStarted::default())), ..Default::default() }
}

fn input(seq: u64, command_id: String) -> SessionEvent {
    SessionEvent {
        seq,
        kind: Some(session_event::Kind::User(UserMessage { command_id, text: "hello".into(), ..Default::default() })),
        ..Default::default()
    }
}

#[tokio::test]
async fn append_waits_for_external_writer_past_default_busy_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    let journal = Journal::open(&path).await.unwrap();
    let started = started();
    journal.create_session("s", &CreateSession::default(), &started).await.unwrap();
    let event = input(2, "c2".into());

    let options = SqliteConnectOptions::new().filename(&path);
    let mut holder = SqliteConnection::connect_with(&options).await.unwrap();
    let mut probe = SqliteConnection::connect_with(&options.busy_timeout(Duration::from_secs(8))).await.unwrap();
    let lock = holder.begin_with("BEGIN IMMEDIATE").await.unwrap();

    // Release only after SQLite confirms the lock outlasted SQLx's default
    // five-second busy timeout. No sleep to guess when a writer is blocked.
    {
        let append = journal.append("s", &event);
        tokio::pin!(append);
        tokio::select! {
            biased;
            result = &mut append => panic!("append returned while the external writer still held the lock: {result:?}"),
            result = sqlx::query("BEGIN IMMEDIATE").execute(&mut probe) => {
                let error = result.expect_err("probe unexpectedly acquired the external writer lock");
                assert_eq!(error.as_database_error().and_then(|e| e.code()).as_deref(), Some("5"), "{error}");
            }
        }
        lock.rollback().await.unwrap();
        append.await.unwrap();
    }

    assert_eq!(journal.events_after("s", 0, 10).await.unwrap(), vec![started, event]);
    assert_eq!(journal.unacked().await.unwrap()[0].last_seq, 2);
    assert!(journal.is_applied("c2").await.unwrap());
}

#[tokio::test]
async fn concurrent_sessions_preserve_sequences_acks_and_receipts() {
    const SESSIONS: usize = 16;
    const LAST: u64 = 24;
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(&dir.path().join("journal.db")).await.unwrap();
    let start = Arc::new(Barrier::new(SESSIONS));
    let started = started();
    let sessions: Vec<_> = (0..SESSIONS).map(|n| format!("s{n}")).collect();
    for id in &sessions {
        journal.create_session(id, &CreateSession::default(), &started).await.unwrap();
    }

    // Mix the transaction-backed appends and standalone ack updates from
    // independent sessions, but keep one sequence producer per session.
    let writers = sessions.iter().map(|id| {
        let start = start.clone();
        let journal = &journal;
        async move {
            start.wait().await;
            for seq in 2..=LAST {
                journal.append(id, &input(seq, format!("{id}:{seq}"))).await.unwrap();
                journal.set_acked(id, seq - 1).await.unwrap();
            }
        }
    });
    futures::future::join_all(writers).await;

    let cursors = journal.unacked().await.unwrap();
    assert_eq!(cursors.len(), SESSIONS);
    for cursor in cursors {
        let id = cursor.session_id;
        assert_eq!((cursor.last_seq, cursor.acked_seq), (LAST, LAST - 1), "{id}");
        let expected = std::iter::once(started.clone()).chain((2..=LAST).map(|seq| input(seq, format!("{id}:{seq}")))).collect::<Vec<_>>();
        assert_eq!(journal.events_after(&id, 0, LAST as u32).await.unwrap(), expected, "{id}");
        for seq in 2..=LAST {
            assert!(journal.is_applied(&format!("{id}:{seq}")).await.unwrap(), "missing receipt for {id}:{seq}");
        }
    }
}
