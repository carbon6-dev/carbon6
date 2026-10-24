//! Where this page is (docs/realms-ux.md §2): one realm's app
//! (`/r/<realm>/…`) or the console (`/admin/…`). It is fixed for the
//! page's life: switching realm loads the app again, there.

use std::cell::RefCell;

use leptos::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Place {
    Realm(String),
    Console,
}

thread_local! {
    static PLACE: RefCell<Option<Place>> = const { RefCell::new(None) };
}

/// The place this browser was in last (opening the app goes back there).
const LAST: &str = "c6.place";
/// What the console is called in addresses and requests.
pub const CONSOLE: &str = "admin";

pub fn here() -> Option<Place> {
    PLACE.with(|p| p.borrow().clone())
}

/// The realm of this page (none in the console).
pub fn realm() -> Option<String> {
    match here() {
        Some(Place::Realm(r)) => Some(r),
        _ => None,
    }
}

pub fn is_console() -> bool {
    here() == Some(Place::Console)
}

/// What requests say about where they come from (`X-C6-Realm`).
pub fn header() -> Option<String> {
    here().map(|p| match p {
        Place::Realm(r) => r,
        Place::Console => CONSOLE.to_owned(),
    })
}

/// The start of this page's addresses: `/r/home` or `/admin`.
pub fn base() -> String {
    match here() {
        Some(Place::Realm(r)) => format!("/r/{r}"),
        Some(Place::Console) => format!("/{CONSOLE}"),
        None => String::new(),
    }
}

/// A view's address on this page.
pub fn href(path: &str) -> String {
    format!("{}{path}", base())
}

/// The address of a place (for switching to it).
pub fn url_of(place: &Place) -> String {
    match place {
        Place::Realm(r) => format!("/r/{r}/"),
        Place::Console => format!("/{CONSOLE}/"),
    }
}

/// The path within the place (`/r/home/sessions/x` → `/sessions/x`).
pub fn strip(path: &str) -> String {
    let base = base();
    let rest = path.strip_prefix(base.as_str()).unwrap_or(path);
    if rest.is_empty() { "/".into() } else { rest.to_owned() }
}

/// Reads where this page is from its address. When the address names no
/// place (the app's root, or a link from before realms had pages), sends
/// the browser to one and returns false: nothing should render.
pub fn resolve() -> bool {
    let loc = window().location();
    let path = loc.pathname().unwrap_or_default();
    let tail = format!("{}{}", loc.search().unwrap_or_default(), loc.hash().unwrap_or_default());
    let mut parts = path.trim_start_matches('/').splitn(3, '/');
    let place = match (parts.next(), parts.next()) {
        (Some("r"), Some(r)) if !r.is_empty() => Some(Place::Realm(r.to_owned())),
        (Some(CONSOLE), _) => Some(Place::Console),
        // Signing in happens before any place.
        (Some("login"), _) => return true,
        _ => None,
    };
    if let Some(p) = place {
        crate::util::storage_set_global(
            LAST,
            &match &p {
                Place::Realm(r) => r.clone(),
                Place::Console => CONSOLE.to_owned(),
            },
        );
        PLACE.with(|x| *x.borrow_mut() = Some(p));
        return true;
    }
    let last = match crate::util::storage_get_global(LAST).filter(|l| !l.is_empty()) {
        Some(l) if l == CONSOLE => Place::Console,
        Some(l) => Place::Realm(l),
        None => Place::Realm("home".into()),
    };
    let go = |to: String| {
        let _ = window().location().replace(&to);
    };
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    match segments.as_slice() {
        // A session: in its own realm.
        ["sessions", id, ..] => {
            let (id, path, tail) = (id.to_string(), path.clone(), tail.clone());
            // Nothing is mounted, so Leptos's executor isn't running.
            wasm_bindgen_futures::spawn_local(async move {
                let realm = crate::api::locate_session(&id).await.ok();
                let base = match (realm, &last) {
                    // Root's sessions (the console assistant's) are the console's.
                    (Some(r), _) if r == "root" => format!("/{CONSOLE}"),
                    (Some(r), _) => format!("/r/{r}"),
                    (None, p) => url_of(p).trim_end_matches('/').to_owned(),
                };
                go(format!("{base}{path}{tail}"));
            });
        }
        // The console's settings (and sign-in redirects back to them).
        ["settings", s, ..] if crate::settings::in_console(s) => go(format!("/{CONSOLE}{path}{tail}")),
        ["settings"] if tail.contains("signed_in") => go(format!("/{CONSOLE}{path}{tail}")),
        _ => go(format!("{}{}{tail}", url_of(&last).trim_end_matches('/'), if path.is_empty() { "/" } else { &path })),
    }
    false
}
