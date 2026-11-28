//! Deterministic prompt history; no model routing or calls. See
//! docs/session-history.md. The effective event log is the authority.

use c6_api::{self as api, Event, EventKind, SessionPrompt};

use crate::{
    HeadState,
    catchup::{remember, visible},
    realms::Caller,
};

#[derive(Clone, Default)]
pub(crate) struct Index {
    read: u64,
    prompts: Vec<SessionPrompt>,
    pending: Vec<usize>,
    active: Option<(u64, i64, Vec<usize>)>,
}

fn first_line(text: &str) -> String {
    text.lines().find(|s| !s.trim().is_empty()).unwrap_or("").chars().take(240).collect()
}

impl Index {
    fn finish(&mut self, at: i64) {
        if let Some((_, start, indices)) = self.active.take() {
            for i in indices {
                let p = &mut self.prompts[i];
                p.end = Some(at);
                p.duration_ms = Some(at.saturating_sub(start).max(0));
            }
        }
    }

    fn feed(&mut self, e: &Event) {
        match &e.kind {
            EventKind::User { command_id, text, blocked, from_session_id, from_title, origin, .. } => {
                let i = self.prompts.len();
                let follow = self.active.is_some() && blocked.is_empty();
                self.prompts.push(SessionPrompt {
                    seq: e.seq,
                    ts: e.ts,
                    command_id: command_id.clone(),
                    text: text.clone(),
                    first_line: first_line(text),
                    origin: if !from_session_id.is_empty() {
                        "session"
                    } else if follow {
                        "follow_up"
                    } else {
                        "user"
                    }
                    .into(),
                    from_session_id: from_session_id.clone(),
                    from_title: from_title.clone(),
                    provenance: origin.clone(),
                    ..Default::default()
                });
                if blocked.is_empty() {
                    if let Some((turn, start, indices)) = &mut self.active {
                        self.prompts[i].turn = Some(*turn);
                        self.prompts[i].start = Some(*start);
                        indices.push(i);
                    } else {
                        self.pending.push(i);
                    }
                }
            }
            EventKind::TurnStarted { turn, .. } => {
                // A worker lost without an end marker: close at the next start.
                self.finish(e.ts);
                let indices = std::mem::take(&mut self.pending);
                for &i in &indices {
                    self.prompts[i].turn = Some(*turn);
                    self.prompts[i].start = Some(e.ts);
                }
                self.active = Some((*turn, e.ts, indices));
            }
            EventKind::TurnEnded { turn, .. } if self.active.as_ref().is_some_and(|(t, _, _)| t == turn) => self.finish(e.ts),
            EventKind::Assistant { turn, blocks, .. } => {
                if let Some((t, _, indices)) = &self.active
                    && t == turn
                {
                    let text = blocks
                        .iter()
                        .filter_map(|b| match b {
                            api::ContentBlock::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !text.trim().is_empty() {
                        let reply = crate::timeline::first_sentence(&text);
                        for &i in indices {
                            self.prompts[i].reply = reply.clone();
                        }
                    }
                }
            }
            _ => {}
        }
        self.read = e.seq;
    }
}

fn page(mut prompts: Vec<SessionPrompt>, q: &api::PromptsQuery) -> api::PromptsPage {
    prompts.sort_by_key(|p| (p.ts, p.seq));
    let mut out = api::PromptsPage { first_ts: prompts.first().map(|p| p.ts), last_ts: prompts.last().map(|p| p.ts), ..Default::default() };
    let cursor = q.after_ts.zip(q.after_seq);
    let limit = q.limit.unwrap_or(500).clamp(1, 1000);
    let mut selected = prompts
        .into_iter()
        .filter(|p| q.since.is_none_or(|t| p.ts >= t) && q.until.is_none_or(|t| p.ts < t) && cursor.is_none_or(|c| (p.ts, p.seq) > c));
    out.prompts.extend(selected.by_ref().take(limit));
    if selected.next().is_some()
        && let Some(p) = out.prompts.last()
    {
        out.next_ts = Some(p.ts);
        out.next_seq = Some(p.seq);
    }
    out
}

pub async fn get(st: &HeadState, id: &str, q: &api::PromptsQuery, caller: &Caller) -> anyhow::Result<api::PromptsPage> {
    let row = visible(st, id, caller).await?;
    if q.after_ts.is_some() != q.after_seq.is_some() || q.since.zip(q.until).is_some_and(|(a, b)| a > b) {
        return Err(crate::bad_request("use both cursor fields and an ordered time range"));
    }
    let mut index = st.catchup.prompts.lock().unwrap().get(id).cloned().unwrap_or_default();
    let last = st.store.last_seq(id).await?;
    while index.read < last {
        let events = st.store.events_after(id, index.read, 1000).await?;
        let Some(end) = events.last().map(|e| e.seq) else { break };
        if events.iter().any(|e| matches!(e.kind, Some(c6_proto::session_event::Kind::Rewound(_)))) {
            index = Index::default();
            for e in st.store.effective_events(id).await?.iter().filter(|e| e.seq <= last).filter_map(crate::convert::event) {
                index.feed(&e);
            }
            index.read = last;
            break;
        }
        for e in events.iter().filter_map(crate::convert::event) {
            index.feed(&e);
        }
        index.read = end;
    }
    let mut out = page(index.prompts.clone(), q);
    out.first_ts = Some(out.first_ts.unwrap_or(row.created_at).min(row.created_at));
    out.last_ts = Some(out.last_ts.unwrap_or(row.updated_at).max(row.updated_at));
    remember(&st.catchup.prompts, id.to_owned(), index);
    if q.after_ts.is_none() {
        out.queued = st
            .store
            .queued_inputs(id)
            .await?
            .iter()
            .map(crate::convert::queued)
            .filter(|p| q.since.is_none_or(|t| p.created_at >= t) && q.until.is_none_or(|t| p.created_at < t))
            .map(|p| SessionPrompt {
                ts: p.created_at,
                first_line: first_line(&p.text),
                text: p.text,
                command_id: p.command_id,
                queued: true,
                origin: "follow_up".into(),
                ..Default::default()
            })
            .collect();
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catchup::tests::{ev, from_session, reply, user};

    #[test]
    fn every_input_keeps_its_timestamp_and_shared_turn_timing() {
        let mut blocked = user("Blocked input");
        if let EventKind::User { blocked, .. } = &mut blocked {
            *blocked = "policy".into();
        }
        let events = [
            ev(1, 100, user("First line\nComplete original prompt")),
            ev(2, 110, EventKind::TurnStarted { turn: 1, context_window: 0 }),
            ev(3, 120, user("Also do this")),
            ev(4, 125, from_session("Release", "Is it ready?")),
            ev(5, 130, blocked),
            ev(6, 140, reply(1, "It is ready. Tests passed.", &[])),
            ev(7, 160, EventKind::TurnEnded { turn: 1, reason: api::TurnEndReason::Completed }),
            ev(8, 170, user("Next task")),
            ev(9, 180, EventKind::TurnStarted { turn: 2, context_window: 0 }),
        ];
        let mut index = Index::default();
        for e in &events {
            index.feed(e);
        }
        let got = &index.prompts;
        assert_eq!(
            got.iter().map(|p| (p.seq, p.ts, p.origin.as_str())).collect::<Vec<_>>(),
            [(1, 100, "user"), (3, 120, "follow_up"), (4, 125, "session"), (5, 130, "user"), (8, 170, "user")]
        );
        assert_eq!(got[0].text, "First line\nComplete original prompt");
        assert_eq!(got[0].first_line, "First line");
        for p in &got[..3] {
            assert_eq!((p.turn, p.start, p.end, p.duration_ms), (Some(1), Some(110), Some(160), Some(50)));
            assert_eq!(p.reply, "It is ready.");
        }
        assert_eq!((&got[2].from_session_id[..], &got[2].from_title[..]), ("other", "Release"));
        assert_eq!((got[3].turn, got[3].duration_ms), (None, None), "blocked input has no fabricated turn");
        assert_eq!((got[4].start, got[4].end, got[4].duration_ms), (Some(180), None, None));
        // Duplicate end markers from an earlier turn cannot close a newer one.
        index.feed(&ev(10, 200, EventKind::TurnEnded { turn: 1, reason: api::TurnEndReason::Completed }));
        assert_eq!(index.prompts[4].end, None);
    }

    #[test]
    fn range_and_cursor_keep_equal_timestamp_inputs_without_duplicates() {
        let prompts = [(5, 200), (1, 100), (3, 100), (7, 300)]
            .into_iter()
            .map(|(seq, ts)| SessionPrompt { seq, ts, ..Default::default() })
            .collect::<Vec<_>>();
        let q = api::PromptsQuery { since: Some(100), until: Some(300), limit: Some(1), ..Default::default() };
        let a = page(prompts.clone(), &q);
        assert_eq!(a.prompts.iter().map(|p| p.seq).collect::<Vec<_>>(), [1]);
        assert_eq!((a.next_ts, a.next_seq), (Some(100), Some(1)));
        let b = page(prompts.clone(), &api::PromptsQuery { after_ts: a.next_ts, after_seq: a.next_seq, ..q.clone() });
        assert_eq!(b.prompts[0].seq, 3);
        let c = page(prompts, &api::PromptsQuery { after_ts: b.next_ts, after_seq: b.next_seq, ..q });
        assert_eq!(c.prompts[0].seq, 5);
        assert_eq!((c.next_ts, c.next_seq), (None, None));
    }

    #[tokio::test]
    async fn prompts_endpoint_reads_stored_inputs_pages_queue_and_realm_checks_without_models() {
        use c6_proto::{SessionEvent, UserMessage, session_event::Kind};
        let (_dir, head, mock, _work) = crate::work::tests::routing_fixture().await;
        let events = vec![
            SessionEvent {
                seq: 1,
                ts_ms: 100,
                kind: Some(Kind::User(UserMessage {
                    command_id: "first".into(),
                    text: "First\nFull text".into(),
                    human_origin: true,
                    origin_recorded: true,
                    ..Default::default()
                })),
            },
            SessionEvent { seq: 2, ts_ms: 110, kind: Some(Kind::TurnStarted(c6_proto::TurnStarted { turn: 1, ..Default::default() })) },
            SessionEvent {
                seq: 3,
                ts_ms: 120,
                kind: Some(Kind::User(UserMessage { command_id: "follow".into(), text: "Follow".into(), ..Default::default() })),
            },
            SessionEvent {
                seq: 4,
                ts_ms: 150,
                kind: Some(Kind::TurnEnded(c6_proto::TurnEnded { turn: 1, reason: c6_proto::TurnEndReason::Completed as i32 })),
            },
            SessionEvent {
                seq: 5,
                ts_ms: 200,
                kind: Some(Kind::User(UserMessage {
                    command_id: "other".into(),
                    text: "Other session".into(),
                    from_session_id: "sender".into(),
                    from_title: "Release".into(),
                    ..Default::default()
                })),
            },
        ];
        crate::store::tests::insert_events(&head.state.store, "session", &events).await;
        sqlx::query("INSERT INTO commands (id, session_id, worker_id, data, text, created_at, state) VALUES ('queued', 'session', '', X'', 'Up next text', 210, 'pending')")
            .execute(&head.state.store.pool).await.unwrap();
        let http = reqwest::Client::new();
        let url = format!("http://{}/api/v1/sessions/session/prompts", head.ui_addrs[0]);
        let a = http.get(format!("{url}?limit=2")).header("x-c6-realm", "home").send().await.unwrap();
        assert_eq!(a.status(), reqwest::StatusCode::OK);
        let a: api::PromptsPage = a.json().await.unwrap();
        assert_eq!(
            a.prompts.iter().map(|p| (p.seq, p.ts, p.origin.as_str())).collect::<Vec<_>>(),
            [(1, 100, "user"), (3, 120, "follow_up")]
        );
        assert_eq!(a.prompts[0].provenance.as_deref(), Some(&api::MessageOrigin::Human));
        assert_eq!((a.prompts[1].start, a.prompts[1].end, a.prompts[1].duration_ms), (Some(110), Some(150), Some(40)));
        assert_eq!(a.queued.len(), 1);
        assert_eq!((a.queued[0].seq, a.queued[0].queued, a.queued[0].ts), (0, true, 210));
        let b: api::PromptsPage = http
            .get(format!("{url}?after_ts={}&after_seq={}", a.next_ts.unwrap(), a.next_seq.unwrap()))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            b.prompts.iter().map(|p| (p.seq, p.origin.as_str(), p.from_session_id.as_str())).collect::<Vec<_>>(),
            [(5, "session", "sender")]
        );
        assert!(b.queued.is_empty(), "queue is included only on the first page");
        let range: api::PromptsPage = http.get(format!("{url}?since=120&until=200")).send().await.unwrap().json().await.unwrap();
        assert_eq!(range.prompts.iter().map(|p| p.seq).collect::<Vec<_>>(), [3]);
        assert!(range.queued.is_empty());
        assert!(get(&head.state, "session", &Default::default(), &Caller::in_realm("other")).await.is_err());
        crate::realms::create(&head.state, &api::RealmIn { id: "other".into(), name: "Other".into(), ..Default::default() }, "you")
            .await
            .unwrap();
        let token = head.state.store.create_api_token("other", Some("other")).await.unwrap().token.unwrap();
        for request in [http.get(&url).header("x-c6-realm", "other"), http.get(&url).bearer_auth(token)] {
            let denied = request.send().await.unwrap();
            assert_eq!(denied.status(), reqwest::StatusCode::NOT_FOUND);
        }
        // The administration console is intentionally allowed to read every
        // realm; it is not an unauthorized caller for a realm-denial test.
        let admin = http.get(&url).header("x-c6-realm", "admin").send().await.unwrap();
        assert_eq!(admin.status(), reqwest::StatusCode::OK);
        let invalid = http.get(format!("{url}?after_ts=100")).send().await.unwrap();
        assert_eq!(invalid.status(), reqwest::StatusCode::BAD_REQUEST);
        assert!(mock.requests().is_empty(), "history must not call any model");
        head.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn rewind_invalidates_cached_input_history() {
        let (_dir, head, mock, _work) = crate::work::tests::routing_fixture().await;
        let events = crate::store::tests::rewind_history();
        crate::store::tests::insert_events(&head.state.store, "session", &events[..11]).await;
        let before = get(&head.state, "session", &Default::default(), &Caller::root()).await.unwrap();
        assert!(before.prompts.iter().any(|p| p.text.contains("PRIVATE")));
        crate::store::tests::insert_events(&head.state.store, "session", &events[11..]).await;
        let after = get(&head.state, "session", &Default::default(), &Caller::root()).await.unwrap();
        assert!(!after.prompts.iter().any(|p| p.text.contains("PRIVATE")));
        assert!(after.prompts.iter().any(|p| p.text.contains("replacement")));
        assert_eq!(get(&head.state, "session", &Default::default(), &Caller::root()).await.unwrap(), after);
        assert!(mock.requests().is_empty());
        head.shutdown().await.unwrap();
    }
}
