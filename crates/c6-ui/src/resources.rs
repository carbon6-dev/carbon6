//! Confirmed head data shared by the signed-in page, not by each picker or
//! workspace tile. Resources belong to Main so closing one consumer cannot
//! dispose data still used by another.

use std::collections::HashMap;

use leptos::prelude::*;

use crate::api;

pub type Connections = LocalResource<Result<Vec<c6_api::Connection>, String>>;

#[derive(Clone)]
struct Page {
    owner: Owner,
    connections: Connections,
    sessions: StoredValue<HashMap<String, SessionData>>,
}

#[derive(Clone, Copy)]
pub struct SessionData {
    pub detail: LocalResource<Result<c6_api::SessionDetail, String>>,
    /// The actual folder from Started (which may be a worktree), never the
    /// requested cwd. The transcript supplies it when already loaded.
    pub folder: RwSignal<Option<String>>,
}

pub fn provide() {
    provide_context(Page {
        owner: Owner::current().expect("signed-in page owner"),
        connections: LocalResource::new(api::connections),
        sessions: StoredValue::new(HashMap::new()),
    });
}

pub fn connections() -> Connections {
    expect_context::<Page>().connections
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
            tile.cleanup();
            let other = session("s1".into());
            assert_eq!(data.detail.to_any_source(), other.detail.to_any_source(), "panes share the transcript's session request");
            other.folder.set(Some("/actual/worktree".into()));
            assert_eq!(data.folder.get_untracked().as_deref(), Some("/actual/worktree"));
            assert_ne!(data.detail.to_any_source(), session("s2".into()).detail.to_any_source(), "different sessions are separate");
            assert_eq!(first.to_any_source(), connections().to_any_source());
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
    fn connection_consumers_do_not_create_their_own_fetchers() {
        for (name, source) in [
            ("assistant", include_str!("assistant.rs")),
            ("details", include_str!("details.rs")),
            ("memex", include_str!("memex.rs")),
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
