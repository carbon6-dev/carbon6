//! Sessions you viewed lately, shared by your devices: each realm keeps
//! its last [`KEEP`] as a setting (`recent_sessions@<realm>`), newest
//! first. Archived sessions, subagents and side chats aren't kept.

use c6_api::RecentView;
use c6_core::now_ms;

use crate::{HeadState, not_found};

/// How many a realm keeps.
pub const KEEP: usize = 20;
/// A session viewed again this soon isn't written again.
const THROTTLE_MS: i64 = 30_000;

/// One write at a time, so two views don't lose one another.
static WRITING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn key(realm: &str) -> String {
    format!("recent_sessions@{realm}")
}

async fn read(st: &HeadState, realm: &str) -> anyhow::Result<Vec<RecentView>> {
    Ok(st.store.setting(&key(realm)).await?.and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default())
}

/// `list` with `id` first, viewed `at`; `None` if it was viewed less than
/// [`THROTTLE_MS`] before (nothing to write).
fn viewed(mut list: Vec<RecentView>, id: &str, at: i64) -> Option<Vec<RecentView>> {
    if list.iter().any(|v| v.session_id == id && (at - v.at).abs() < THROTTLE_MS) {
        return None;
    }
    list.retain(|v| v.session_id != id);
    list.insert(0, RecentView { session_id: id.to_owned(), at });
    list.truncate(KEEP);
    Some(list)
}

/// You viewed session `id`: it goes first in its realm's list.
pub async fn view(st: &HeadState, id: &str) -> anyhow::Result<()> {
    let Some(s) = st.store.session(id).await? else { return Err(not_found("no such session")) };
    if s.archived != 0 || s.parent_id.is_some() {
        return Ok(());
    }
    let _one = WRITING.lock().await;
    if let Some(list) = viewed(read(st, &s.realm).await?, id, now_ms()) {
        st.store.set_setting(&key(&s.realm), &serde_json::to_string(&list)?).await?;
    }
    Ok(())
}

/// `realm`'s sessions viewed lately, newest first (those since archived
/// or gone left out).
pub async fn list(st: &HeadState, realm: &str) -> anyhow::Result<Vec<RecentView>> {
    let list = read(st, realm).await?;
    let ids = serde_json::to_string(&list.iter().map(|v| &v.session_id).collect::<Vec<_>>())?;
    let live: std::collections::HashSet<String> = sqlx::query_scalar(
        "SELECT id FROM sessions WHERE realm = ? AND archived = 0 AND parent_id IS NULL AND id IN (SELECT value FROM json_each(?))",
    )
    .bind(realm)
    .bind(ids)
    .fetch_all(&st.store.pool)
    .await?
    .into_iter()
    .collect();
    Ok(list.into_iter().filter(|v| live.contains(&v.session_id)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(list: &[RecentView]) -> Vec<&str> {
        list.iter().map(|v| v.session_id.as_str()).collect()
    }

    #[test]
    fn the_latest_view_goes_first_once() {
        let list = viewed(Vec::new(), "a", 0).unwrap();
        let list = viewed(list, "b", 1_000).unwrap();
        assert_eq!(ids(&list), ["b", "a"]);
        let list = viewed(list, "a", 40_000).unwrap();
        assert_eq!(ids(&list), ["a", "b"]);
        assert_eq!(list[0].at, 40_000);
    }

    #[test]
    fn a_view_soon_after_the_last_is_not_written() {
        let list = viewed(Vec::new(), "a", 0).unwrap();
        assert!(viewed(list.clone(), "a", 29_000).is_none());
        assert!(viewed(list, "a", 31_000).is_some());
    }

    #[test]
    fn only_the_last_ones_are_kept() {
        let mut list = Vec::new();
        for i in 0..KEEP + 5 {
            list = viewed(list, &format!("s{i}"), i as i64).unwrap();
        }
        assert_eq!(list.len(), KEEP);
        assert_eq!(list[0].session_id, format!("s{}", KEEP + 4));
    }
}
