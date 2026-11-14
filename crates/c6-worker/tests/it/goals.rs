//! A session's goal (docs/goals.md): set on an idle session it starts at
//! once; after each turn a check decides whether it is reached, needs you,
//! or the session keeps going, and turns, stalls and your own say bound it.

use c6_api::{SessionGoal, SessionStatus};
use c6_testkit::{Reply, eventually};
use serde_json::{Value, json};

use crate::batch_capture::{Env, user_texts};

const CHECK: &str = "An agent is working toward a goal";

fn verdict(v: &str, why: &str, left: &str) -> Reply {
    Reply::Text(json!({"verdict": v, "why": why, "left": left}).to_string())
}

/// PUTs, failing the test on an error status.
async fn put(env: &Env, path: &str, body: Value) -> Value {
    let r = env.http.put(format!("{}/{path}", env.api)).json(&body).send().await.unwrap();
    let status = r.status();
    let text = r.text().await.unwrap();
    assert!(status.is_success(), "PUT {path}: {status} {text}");
    serde_json::from_str(&text).unwrap()
}

async fn patch(env: &Env, id: &str, state: &str) -> reqwest::StatusCode {
    env.http.patch(format!("{}/sessions/{id}/goal", env.api)).json(&json!({"state": state})).send().await.unwrap().status()
}

async fn goal(env: &Env, id: &str) -> SessionGoal {
    serde_json::from_value(env.get(&format!("sessions/{id}/goal")).await).unwrap()
}

async fn settled(env: &Env, id: &str, state: &str) -> SessionGoal {
    eventually(&format!("the goal to be {state}"), async || {
        let g: Option<SessionGoal> = serde_json::from_value(env.get(&format!("sessions/{id}/goal")).await).ok()?;
        let idle = env.get(&format!("sessions/{id}")).await["session"]["status"] == json!(SessionStatus::Idle);
        g.filter(|g| g.state == state && idle)
    })
    .await
}

#[tokio::test]
async fn a_goal_keeps_a_session_going_until_it_is_reached() {
    let env = Env::new().await;
    let here = tempfile::tempdir().unwrap();
    env.mock.push_about("look at the parser", Reply::Text("Looked.".into()));
    let s = env.start(here.path(), "look at the parser").await;
    env.ended(&s.id, 1).await;

    // Set while idle: it starts at once. Each check says what is left, and
    // the next message says it; the last one sees it reached.
    env.mock.push_about("Work toward this goal", Reply::Text("Fixed the parser; the docs are next.".into()));
    env.mock.push_about_all(&[CHECK, "Fixed the parser"], verdict("continue", "the parser is fixed", "update the docs"));
    env.mock.push_about("Goal check (turn 2 of 20)", Reply::Text("Updated the docs; all tests pass.".into()));
    env.mock.push_about_all(&[CHECK, "Updated the docs"], verdict("achieved", "tests pass and docs are updated", ""));
    let set = put(&env, &format!("sessions/{}/goal", s.id), json!({"text": "the parser handles empty input, with docs"})).await;
    assert_eq!(set["state"], "active");
    let g = settled(&env, &s.id, "achieved").await;
    assert_eq!((g.turns, g.why.as_str()), (2, "tests pass and docs are updated"));
    let said = user_texts(&env.events(&s.id).await);
    assert!(said.iter().any(|t| t.starts_with("Work toward this goal")), "{said:?}");
    let next: Vec<&String> = said.iter().filter(|t| t.starts_with("Goal check (turn 2 of 20)")).collect();
    assert!(next.len() == 1 && next[0].contains("Left: update the docs"), "{said:?}");
    // The live list carries it, for the strip above the message box.
    let listed: Vec<Value> = serde_json::from_value(env.get("sessions").await).unwrap();
    assert_eq!(listed.iter().find(|x| x["id"] == json!(s.id)).unwrap()["goal"]["state"], "achieved");

    // No progress twice in a row: it stops rather than going in circles.
    env.mock.push_about("until it is reached: ship the release", Reply::Text("Tried; CI is red.".into()));
    env.mock.push_about_all(&[CHECK, "Tried; CI is red."], verdict("continue", "CI is red", "make CI green"));
    env.mock.push_about("Goal check (turn 2 of 20)", Reply::Text("Tried again; CI is still red.".into()));
    env.mock.push_about_all(&[CHECK, "Tried again"], verdict("continue", "CI is red", "Make CI green"));
    env.mock.push_about("Goal check (turn 3 of 20)", Reply::Text("Third try; CI is still red.".into()));
    env.mock.push_about_all(&[CHECK, "Third try"], verdict("continue", "CI is red", "make  CI green"));
    put(&env, &format!("sessions/{}/goal", s.id), json!({"text": "ship the release"})).await;
    let g = settled(&env, &s.id, "stopped").await;
    assert!(g.why.contains("no progress"), "{g:?}");
    assert_eq!(g.turns, 3);

    // Blocked on you: it stops and says so.
    env.mock.push_about("until it is reached: deploy to staging", Reply::Text("I need the staging token.".into()));
    env.mock.push_about_all(&[CHECK, "I need the staging token"], verdict("needs_you", "it needs the staging token", ""));
    put(&env, &format!("sessions/{}/goal", s.id), json!({"text": "deploy to staging"})).await;
    assert_eq!(settled(&env, &s.id, "needs_you").await.why, "it needs the staging token");

    // Out of turns.
    env.mock.push_about("until it is reached: rename the crate", Reply::Text("Renamed half.".into()));
    env.mock.push_about_all(&[CHECK, "Renamed half"], verdict("continue", "half done", "rename the rest"));
    put(&env, &format!("sessions/{}/goal", s.id), json!({"text": "rename the crate", "max_turns": 1})).await;
    let g = settled(&env, &s.id, "stopped").await;
    assert!(g.why.contains("used its 1 turns"), "{g:?}");

    // Paused while a turn runs: no next turn when it ends. Resumed, the
    // next message says what was left.
    env.mock.push_about("until it is reached: rename the crate", Reply::Text("Renamed the library.".into()));
    env.mock.push_about_all(&[CHECK, "Renamed the library"], verdict("continue", "the library is renamed", "rename the binaries"));
    let gate = env.mock.push_gated_about("Goal check (turn 2 of 5)", Reply::Text("Renamed one binary.".into()));
    put(&env, &format!("sessions/{}/goal", s.id), json!({"text": "rename the crate", "max_turns": 5})).await;
    eventually("the second turn toward it to be running", async || {
        let detail = env.get(&format!("sessions/{}", s.id)).await;
        (detail["session"]["status"] == json!(SessionStatus::Running) && goal(&env, &s.id).await.turns == 2).then_some(())
    })
    .await;
    assert!(patch(&env, &s.id, "paused").await.is_success());
    let turns = env.events(&s.id).await.iter().filter(|e| matches!(e.kind, c6_api::EventKind::TurnEnded { .. })).count();
    gate.notify_one();
    let after = env.ended(&s.id, turns + 1).await;
    let g = goal(&env, &s.id).await;
    assert_eq!((g.state.as_str(), g.turns), ("paused", 2), "paused: no check, no next turn");
    assert!(!user_texts(&after).iter().any(|t| t.starts_with("Goal check (turn 3 of 5)")), "no third turn while paused");
    env.mock.push_about("Goal check (turn 3 of 5)", Reply::Text("Renamed every binary.".into()));
    env.mock.push_about_all(&[CHECK, "Renamed every binary"], verdict("achieved", "all renamed", ""));
    assert!(patch(&env, &s.id, "active").await.is_success());
    let g = settled(&env, &s.id, "achieved").await;
    assert_eq!(g.turns, 3);
    let resumed: Vec<String> =
        user_texts(&env.events(&s.id).await).into_iter().filter(|t| t.starts_with("Goal check (turn 3 of 5)")).collect();
    assert!(resumed.len() == 1 && resumed[0].contains("Left: rename the binaries"), "{resumed:?}");

    // Reached stays reached: no pausing or resuming it; clear it or set a
    // new one.
    assert_eq!(patch(&env, &s.id, "paused").await, reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(patch(&env, &s.id, "active").await, reqwest::StatusCode::BAD_REQUEST);
    let r = env.http.delete(format!("{}/sessions/{}/goal", env.api, s.id)).send().await.unwrap();
    assert!(r.status().is_success());
    assert!(env.get(&format!("sessions/{}/goal", s.id)).await.is_null());
    env.stop.cancel();
}
