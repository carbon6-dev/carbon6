//! The same session archive offer from a list or a session's tile menu.

use std::collections::BTreeSet;

use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api::{self, SessionArchiveDoc};
use crate::util::{BUTTON, BUTTON_QUIET, ErrorBox, Notice};

#[derive(Clone)]
struct ArchiveRequest {
    id: String,
    docs: Vec<SessionArchiveDoc>,
}

/// Lives above tiles and menus: closing the originating menu must not
/// dispose the offer, its pending request, or a no-doc archive's Undo.
#[derive(Clone, Copy)]
pub(crate) struct SessionArchive {
    request: RwSignal<Option<ArchiveRequest>>,
    checking: RwSignal<bool>,
    owner: StoredValue<WeakOwner>,
    notice: Notice,
    live: api::LiveFeed,
}

impl SessionArchive {
    pub fn provide() {
        provide_context(Self {
            request: RwSignal::new(None),
            checking: RwSignal::new(false),
            owner: StoredValue::new(Owner::current().expect("session archive owner").downgrade()),
            notice: Notice::get(),
            live: api::live(),
        });
    }

    pub fn get() -> Self {
        expect_context()
    }

    pub fn start(self, id: String) {
        if self.checking.get_untracked() || self.request.with_untracked(Option::is_some) {
            return;
        }
        self.checking.set(true);
        spawn_local(async move {
            match api::session_archive_docs(id.clone()).await {
                Ok(docs) if needs_offer(&docs) => self.request.set(Some(ArchiveRequest { id, docs })),
                Ok(_) => match api::archive_with_docs(id.clone(), Vec::new()).await {
                    Ok(()) => {
                        self.remember(&id).await;
                        // Allocate under Main, not the row or menu that just
                        // disappeared. Unarchiving never restores documents.
                        let undo = self.owner.with_value(WeakOwner::upgrade).map(|owner| {
                            owner.with(|| {
                                Callback::new(move |()| {
                                    let id = id.clone();
                                    spawn_local(async move {
                                        match api::set_archived(id.clone(), false).await {
                                            Ok(()) => self.remember(&id).await,
                                            Err(e) => self.notice.show(format!("Couldn't bring it back: {e}"), None),
                                        }
                                    });
                                })
                            })
                        });
                        self.notice.show("Archived", undo);
                    }
                    Err(e) => self.notice.show(format!("Couldn't archive it: {e}"), None),
                },
                Err(e) => self.notice.show(format!("Couldn't check its docs: {e}"), None),
            }
            self.checking.set(false);
        });
    }

    /// Keep open tiles on the upstream archived-session view immediately,
    /// without waiting for the next sessions-list refresh to arrive.
    async fn remember(self, id: &str) {
        if let Ok(detail) = api::session(id.to_owned()).await {
            self.live.remember_session(detail.session);
        }
    }
}

fn needs_offer(docs: &[SessionArchiveDoc]) -> bool {
    !docs.is_empty()
}

/// The override is separate from individual choices. Turning it off must
/// restore even choices the user changed from the safe defaults.
#[derive(Clone, Debug, PartialEq)]
struct Selection {
    individual: BTreeSet<String>,
    all: bool,
}

impl Selection {
    fn new(docs: &[SessionArchiveDoc]) -> Self {
        Self { individual: docs.iter().filter(|d| d.reasons.is_empty()).map(|d| d.id.clone()).collect(), all: false }
    }

    fn checked(&self, id: &str) -> bool {
        self.all || self.individual.contains(id)
    }

    fn toggle(&mut self, id: &str, checked: bool) {
        if self.all {
            return;
        }
        if checked {
            self.individual.insert(id.to_owned());
        } else {
            self.individual.remove(id);
        }
    }

    fn ids(&self, docs: &[SessionArchiveDoc]) -> Vec<String> {
        docs.iter().filter(|d| self.checked(&d.id)).map(|d| d.id.clone()).collect()
    }
}

#[component]
pub(crate) fn ArchiveHost() -> impl IntoView {
    let archive = SessionArchive::get();
    move || archive.request.get().map(|request| view! { <ArchiveDialog request archive /> })
}

#[component]
fn ArchiveDialog(request: ArchiveRequest, archive: SessionArchive) -> impl IntoView {
    let selection = RwSignal::new(Selection::new(&request.docs));
    let request = StoredValue::new(request);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let panel = NodeRef::<leptos::html::Div>::new();
    let cancel = Callback::new(move |()| {
        if !busy.get_untracked() {
            archive.request.set(None);
        }
    });
    let pop = crate::util::escapable(|| true, move || cancel.run(()));
    Effect::new(move || {
        if let Some(el) = panel.get() {
            let _ = el.focus();
        }
    });
    let submit = move |_| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        error.set(None);
        let request = request.get_value();
        let ids = selection.with_untracked(|s| s.ids(&request.docs));
        spawn_local(async move {
            match api::archive_with_docs(request.id.clone(), ids).await {
                Ok(()) => {
                    archive.remember(&request.id).await;
                    archive.notice.show("Session archived. Unchecked docs were kept.", None);
                    archive.request.set(None);
                }
                Err(e) => {
                    error.set(Some(e));
                    busy.set(false);
                }
            }
        });
    };
    view! {
        <leptos::portal::Portal>
            <div class="fixed inset-0 z-50 flex items-end justify-center bg-black/40 p-4 sm:items-center"
                on:click=move |_| cancel.run(())>
                <div node_ref=panel data-pop=pop.clone() role="dialog" aria-modal="true" aria-label="Archive session" tabindex="-1"
                    class="flex max-h-[calc(100dvh-2rem)] w-full min-w-0 max-w-lg flex-col gap-3 overflow-hidden rounded-lg bg-white p-4 text-left text-zinc-900 shadow-xl outline-none dark:bg-zinc-900 dark:text-zinc-100"
                    on:click=|ev| ev.stop_propagation()
                    on:keydown=move |ev| {
                        if ev.key() == "Tab" && let Some(panel) = panel.get_untracked() {
                            keep_focus(&panel, &ev);
                        }
                    }>
                    <div class="shrink-0 space-y-2">
                        <h2 class="font-medium">"Archive session"</h2>
                        <p class="text-sm text-muted">"Archive docs and decks made by this session or its subagents too? Unchecked items stay available."</p>
                        <crate::components::Switch on=move || selection.with(|s| s.all)
                            on_change=move |on| selection.update(|s| s.all = on)
                            label="Archive all docs" hint="Include items with reasons to keep them."
                            disabled=move || busy.get() />
                    </div>
                    <div class="min-h-0 flex-1 overflow-y-auto overscroll-contain divide-y divide-zinc-200 dark:divide-zinc-700" aria-label="Docs to archive">
                        {request.get_value().docs.into_iter().map(|doc| view! { <ArchiveDocRow doc selection busy /> }).collect_view()}
                    </div>
                    <div class="shrink-0 space-y-2">
                        {move || error.get().map(|message| view! { <ErrorBox message /> })}
                        <div class="flex justify-end gap-2">
                            <button class=format!("{BUTTON_QUIET} min-h-11") disabled=move || busy.get() on:click=move |_| cancel.run(())>"Cancel"</button>
                            <button class=format!("{BUTTON} min-h-11") disabled=move || busy.get() on:click=submit>
                                {move || if busy.get() { "Archiving…" } else { "Archive session" }}
                            </button>
                        </div>
                    </div>
                </div>
            </div>
        </leptos::portal::Portal>
    }
}

fn keep_focus(panel: &web_sys::HtmlElement, ev: &web_sys::KeyboardEvent) {
    use wasm_bindgen::JsCast;
    let Ok(nodes) = panel.query_selector_all("button:not([disabled]), input:not([disabled])") else { return };
    let controls: Vec<web_sys::HtmlElement> = (0..nodes.length()).filter_map(|i| nodes.item(i)?.dyn_into().ok()).collect();
    let (Some(first), Some(last)) = (controls.first(), controls.last()) else { return };
    let active = document().active_element();
    let at_panel = active.as_ref() == Some(panel.as_ref());
    let target = if ev.shift_key() && (at_panel || active.as_ref() == Some(first.as_ref())) {
        Some(last)
    } else if !ev.shift_key() && (at_panel || active.as_ref() == Some(last.as_ref())) {
        Some(first)
    } else {
        None
    };
    if let Some(target) = target {
        ev.prevent_default();
        let _ = target.focus();
    }
}

#[component]
fn ArchiveDocRow(doc: SessionArchiveDoc, selection: RwSignal<Selection>, busy: RwSignal<bool>) -> impl IntoView {
    let description = format!("archive-doc-{}-details", doc.id);
    let id = StoredValue::new(doc.id);
    let kind = if doc.kind == "deck" { "Deck" } else { "Doc" };
    view! {
        <label class="flex min-h-11 min-w-0 cursor-pointer items-start gap-3 rounded-md px-2 py-3 text-sm hover:bg-zinc-100 dark:hover:bg-zinc-800">
            <input type="checkbox" class=format!("{} mt-0.5", crate::components::CHECKBOX) aria-label=format!("Archive {}", doc.title) aria-describedby=description.clone()
                prop:checked=move || selection.with(|s| s.checked(&id.get_value()))
                disabled=move || busy.get() || selection.with(|s| s.all)
                on:change=move |ev| selection.update(|s| s.toggle(&id.get_value(), event_target_checked(&ev))) />
            <span class="min-w-0 flex-1 break-words">
                <span class="font-medium">{doc.title}</span>
                <span id=description class="block">
                    <span class="mt-0.5 flex items-center gap-1 text-xs text-muted">{crate::icons::file()}{kind}</span>
                    {doc.reasons.into_iter().map(|reason| view! { <span class="mt-1 block text-xs text-muted">{reason}</span> }).collect_view()}
                </span>
            </span>
        </label>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn docs() -> Vec<SessionArchiveDoc> {
        vec![
            SessionArchiveDoc { id: "safe-doc".into(), title: "Draft".into(), kind: "doc".into(), reasons: vec![] },
            SessionArchiveDoc {
                id: "kept-deck".into(),
                title: "Review".into(),
                kind: "deck".into(),
                reasons: vec!["Attached to a project".into()],
            },
            SessionArchiveDoc { id: "safe-deck".into(), title: "Sketch".into(), kind: "deck".into(), reasons: vec![] },
        ]
    }

    #[test]
    fn no_docs_skip_the_offer() {
        assert!(!needs_offer(&[]));
        assert!(needs_offer(&docs()));
        assert!(Selection::new(&[]).ids(&[]).is_empty());
    }

    #[test]
    fn defaults_only_select_docs_and_decks_without_reasons() {
        let docs = docs();
        let selection = Selection::new(&docs);
        assert_eq!(selection.ids(&docs), ["safe-doc", "safe-deck"]);
    }

    #[test]
    fn all_override_locks_rows_and_restores_individual_choices() {
        let docs = docs();
        let mut selection = Selection::new(&docs);
        selection.toggle("safe-doc", false);
        selection.toggle("kept-deck", true);
        let saved = selection.individual.clone();
        for _ in 0..2 {
            selection.all = true;
            assert_eq!(selection.ids(&docs), ["safe-doc", "kept-deck", "safe-deck"]);
            selection.toggle("kept-deck", false);
            selection.toggle("safe-doc", true);
            assert_eq!(selection.individual, saved);
            selection.all = false;
            assert_eq!(selection.ids(&docs), ["kept-deck", "safe-deck"]);
        }
    }

    #[test]
    fn submitted_ids_are_exactly_the_checked_candidates() {
        let docs = docs();
        let mut selection = Selection::new(&docs);
        selection.toggle("safe-doc", false);
        selection.toggle("safe-deck", false);
        assert!(selection.ids(&docs).is_empty());
        selection.toggle("kept-deck", true);
        selection.toggle("not-a-candidate", true);
        assert_eq!(selection.ids(&docs), ["kept-deck"]);
    }

    #[test]
    fn rows_are_locked_while_archiving_or_archive_all_is_on() {
        Owner::new().with(|| {
            let docs = docs();
            let selection = RwSignal::new(Selection::new(&docs));
            let busy = RwSignal::new(false);
            for (all, saving) in [(false, false), (true, false), (false, true), (false, false)] {
                selection.update(|s| s.all = all);
                busy.set(saving);
                let doc = docs[1].clone();
                let html = untrack(|| view! { <ArchiveDocRow doc selection busy /> }.to_html());
                assert_eq!(html.contains(" disabled"), all || saving, "{html}");
                assert!(html.contains(crate::components::CHECKBOX), "{html}");
            }
            assert_eq!(selection.with(|s| s.ids(&docs)), ["safe-doc", "safe-deck"]);
        });
    }

    #[test]
    fn rows_show_titles_kinds_and_every_reason_with_touch_height() {
        Owner::new().with(|| {
            let docs = docs();
            let selection = RwSignal::new(Selection::new(&docs));
            let busy = RwSignal::new(false);
            for doc in docs {
                let title = doc.title.clone();
                let kind = if doc.kind == "deck" { "Deck" } else { "Doc" };
                let reasons = doc.reasons.clone();
                let html = untrack(|| view! { <ArchiveDocRow doc selection busy /> }.to_html());
                assert!(html.contains(&title), "{html}");
                assert!(html.contains(kind), "{html}");
                assert!(html.contains("min-h-11"), "{html}");
                assert!(html.contains("break-words"), "{html}");
                assert!(html.contains(crate::components::CHECKBOX), "{html}");
                assert!(html.contains("<svg"), "{html}");
                for reason in reasons {
                    assert!(html.contains(&reason), "{html}");
                }
            }
        });
    }
}
