//! Confirmed head data shared by the signed-in page, not by each picker or
//! workspace tile. Resources belong to Main so closing one consumer cannot
//! dispose data still used by another.

use std::collections::HashMap;

use leptos::prelude::*;

use crate::api;

pub type Connections = LocalResource<Result<Vec<c6_api::Connection>, String>>;
type ModelList = ArcLocalResource<Result<Vec<String>, String>>;
const MODEL_FRESH_MS: i64 = 10 * 60 * 1000;
const MODEL_RETRY_MS: i64 = 30 * 1000;
const MODEL_LIMIT: usize = 128;

struct ModelEntry {
    fetched_at: i64,
    touched_at: i64,
    list: ModelList,
}

#[derive(Clone)]
struct Page {
    owner: Owner,
    connections: Connections,
    sessions: StoredValue<HashMap<String, SessionData>>,
    models: StoredValue<HashMap<String, ModelEntry>>,
}

#[derive(Clone, Copy)]
pub struct SessionData {
    pub detail: LocalResource<Result<c6_api::SessionDetail, String>>,
    /// The actual folder from Started (which may be a worktree), never the
    /// requested cwd. The transcript supplies it when already loaded.
    pub folder: RwSignal<Option<String>>,
}

pub fn provide() {
    let models = StoredValue::new(HashMap::new());
    provide_context(Page {
        owner: Owner::current().expect("signed-in page owner"),
        connections: LocalResource::new(move || async move {
            let result = api::connections().await;
            if result.is_ok() {
                // Only a confirmed reload invalidates model lists. Old
                // in-flight resources cannot reinsert themselves here.
                models.try_update_value(|all| all.clear());
            }
            result
        }),
        sessions: StoredValue::new(HashMap::new()),
        models,
    });
}

pub fn connections() -> Connections {
    expect_context::<Page>().connections
}

/// A header can join an initial pane read, without accepting an old answer
/// instead of the fresh by-id check needed when a session leaves the live list.
pub fn pending_session(id: &str) -> Option<SessionData> {
    let page = expect_context::<Page>();
    page.sessions.with_value(|all| all.get(id).copied()).filter(|data| data.detail.get_untracked().is_none())
}

/// Shared completed and pending lists. Returning an Arc resource lets an
/// open picker keep using its list even when the bounded page cache evicts
/// that entry. No signal used by a mounted picker is disposed on eviction.
pub fn models(id: String) -> ModelList {
    models_at(id, crate::util::now_ms())
}

fn models_at(id: String, now: i64) -> ModelList {
    let page = expect_context::<Page>();
    page.models
        .try_update_value(|all| {
            if let Some(entry) = all.get_mut(&id) {
                entry.touched_at = now;
                let age = now.saturating_sub(entry.fetched_at);
                if age >= MODEL_FRESH_MS || (age >= MODEL_RETRY_MS && entry.list.get_untracked().is_some_and(|r| r.is_err())) {
                    entry.fetched_at = now;
                    entry.list.refetch();
                }
                return entry.list.clone();
            }
            if all.len() >= MODEL_LIMIT
                && let Some(oldest) = all.iter().min_by_key(|(_, e)| e.touched_at).map(|(id, _)| id.clone())
            {
                all.remove(&oldest);
            }
            let list = page.owner.with(|| {
                ArcLocalResource::new({
                    let id = id.clone();
                    move || api::models(id.clone())
                })
            });
            all.insert(id, ModelEntry { fetched_at: now, touched_at: now, list: list.clone() });
            list
        })
        .expect("signed-in page models")
}

pub fn session(id: String) -> SessionData {
    let page = expect_context::<Page>();
    if let Some(data) = page.sessions.with_value(|all| all.get(&id).copied()) {
        return data;
    }
    let data = page.owner.with(|| SessionData {
        detail: LocalResource::new({
            let id = id.clone();
            move || api::session(id.clone())
        }),
        folder: RwSignal::new(None),
    });
    page.sessions.update_value(|all| {
        all.insert(id, data);
    });
    data
}

#[cfg(test)]
mod tests {
    use super::*;
    use leptos::reactive::graph::ToAnySource;

    #[test]
    fn consumers_share_page_resources_after_a_tile_is_disposed() {
        let page = Owner::new();
        page.with(|| {
            provide();
            let first = connections();
            let tile = Owner::new();
            let data = tile.with(|| {
                assert_eq!(first.to_any_source(), connections().to_any_source(), "a picker must not create another connections resource");
                session("s1".into())
            });
            assert!(pending_session("unopened").is_none(), "a header must not start a hidden pane read");
            assert_eq!(
                pending_session("s1").unwrap().detail.to_any_source(),
                data.detail.to_any_source(),
                "headers join pending pane reads"
            );
            tile.cleanup();
            let other = session("s1".into());
            assert_eq!(data.detail.to_any_source(), other.detail.to_any_source(), "panes share the transcript's session request");
            other.folder.set(Some("/actual/worktree".into()));
            assert_eq!(data.folder.get_untracked().as_deref(), Some("/actual/worktree"));
            assert_ne!(data.detail.to_any_source(), session("s2".into()).detail.to_any_source(), "different sessions are separate");
            assert_eq!(first.to_any_source(), connections().to_any_source());
            data.detail.set(Some(Err("previous failure".into())));
            assert!(pending_session("s1").is_none(), "an old result must not suppress a fresh metadata check or retry");
        });
    }

    #[test]
    fn a_new_signed_in_page_has_its_own_resources() {
        let page = Owner::new();
        let first = page.with(|| {
            provide();
            connections()
        });
        let other = Owner::new();
        other.with(|| {
            provide();
            assert_ne!(first.to_any_source(), connections().to_any_source(), "do not share a previous user's or realm's resources");
        });
    }

    #[test]
    fn model_prefetch_and_pickers_share_pending_lists_and_one_refresh() {
        let page = Owner::new();
        page.with(|| {
            provide();
            let first = models_at("c".into(), 0);
            let second = models_at("c".into(), 1);
            assert_eq!(first.to_any_source(), second.to_any_source(), "mounting another box must share the pending request");
            let refreshed = models_at("c".into(), MODEL_FRESH_MS);
            assert_eq!(first.to_any_source(), refreshed.to_any_source(), "refresh keeps the existing confirmed list available");
            let shared = expect_context::<Page>();
            assert_eq!(shared.models.with_value(|all| all["c"].fetched_at), MODEL_FRESH_MS);
            models_at("c".into(), MODEL_FRESH_MS + 1);
            assert_eq!(shared.models.with_value(|all| all["c"].fetched_at), MODEL_FRESH_MS, "a second consumer must not restart refresh");
            shared.models.update_value(|all| all.clear());
            let after_edit = models_at("c".into(), MODEL_FRESH_MS + 2);
            assert_ne!(first.to_any_source(), after_edit.to_any_source(), "old in-flight lists cannot refill a cleared cache");
            assert!(first.get_untracked().is_none(), "detaching old cache entries must not dispose a mounted consumer");
        });
    }

    #[test]
    fn model_cache_is_bounded_without_disposing_an_open_picker() {
        let page = Owner::new();
        page.with(|| {
            provide();
            let open = models_at("open".into(), 0);
            for i in 0..MODEL_LIMIT {
                models_at(i.to_string(), 1);
            }
            let shared = expect_context::<Page>();
            assert_eq!(shared.models.with_value(HashMap::len), MODEL_LIMIT);
            assert!(!shared.models.with_value(|all| all.contains_key("open")));
            assert!(open.get_untracked().is_none(), "an evicted Arc resource remains usable by its picker");
        });
    }

    #[test]
    fn connection_consumers_do_not_create_their_own_fetchers() {
        assert!(
            include_str!("settings.rs").contains("api::check_models(id).await"),
            "the saved connection's Test button must not use a cached list"
        );
        for (name, source) in [
            ("assistant", include_str!("assistant.rs")),
            ("details", include_str!("details.rs")),
            ("memex", include_str!("./memex.rs")),
            ("models", include_str!("models.rs")),
            ("session", include_str!("session.rs")),
            ("sessions", include_str!("sessions.rs")),
            ("settings", include_str!("settings.rs")),
            ("usage", include_str!("usage.rs")),
        ] {
            assert!(!source.contains("api::connections"), "{name} must use the page's connections resource");
        }
    }
}
