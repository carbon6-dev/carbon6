//! Shared textarea mention search, insertion and keyboard navigation.

use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;

/// The slash command being typed: the text up to the caret, while it is
/// still the first word.
fn command_at(text: &str, caret: usize) -> Option<String> {
    let before: String = text.chars().take(caret).collect();
    let q = before.strip_prefix('/')?;
    (!q.contains(char::is_whitespace) && !q.contains('/')).then(|| q.to_owned())
}

#[derive(Clone, PartialEq)]
enum Suggestion {
    File(String, bool),
    /// A thing in the memex: (what to insert after @memex:, kind, name).
    Thing(String, String, String),
    /// For Ask (docs/ask.md §4): (title, kind, reference): a project, doc,
    /// work, or any memex item, read when the message reaches it.
    Ref(String, String, String),
    Command(c6_api::CommandInfo),
}

/// The `@` mention being typed at the caret: (where the @ is, the query).
/// A space ends it, except in a quoted one, `@"words with spaces`, which
/// goes on until its closing quote.
fn mention_at(text: &str, caret: usize) -> Option<(usize, String)> {
    let before: String = text.chars().take(caret).collect();
    if let Some(at) = before.rfind("@\"") {
        let query = &before[at + 2..];
        let starts_word = before[..at].chars().last().is_none_or(char::is_whitespace);
        if starts_word && !query.contains('"') {
            return Some((before[..at].chars().count(), query.to_owned()));
        }
    }
    let at = before.rfind('@')?;
    let query = &before[at + 1..];
    let starts_word = before[..at].chars().last().is_none_or(char::is_whitespace);
    (starts_word && !query.contains(char::is_whitespace)).then(|| (before[..at].chars().count(), query.to_owned()))
}

/// Where a mention being typed ends: its @, its query, and the quote a
/// quoted one opens with.
fn mention_end(chars: &[char], at: usize, query: &str) -> usize {
    let quoted = chars.get(at + 1) == Some(&'"');
    (at + 1 + usize::from(quoted) + query.chars().count()).min(chars.len())
}

fn replace_mention(text: &str, at: usize, query: &str, insert: &str) -> Option<(String, u32)> {
    let chars: Vec<char> = text.chars().collect();
    let end = mention_end(&chars, at, query);
    if mention_at(text, end).as_ref() != Some(&(at, query.to_owned())) {
        return None;
    }
    let new = chars[..at].iter().collect::<String>() + insert + &chars[end..].iter().collect::<String>();
    Some((new, (at + insert.chars().count()) as u32))
}

#[derive(Clone, Copy)]
pub struct Completion {
    pub mention: RwSignal<Option<(usize, String)>>,
    pub command: RwSignal<Option<String>>,
    pub pop: StoredValue<String>,
    picked: RwSignal<usize>,
    suggestions: Signal<Vec<Suggestion>>,
    pub update: Callback<()>,
    complete: Callback<(String, bool)>,
    complete_ref: Callback<(String, String)>,
    complete_command: Callback<String>,
}

impl Completion {
    /// True when navigation or completion consumed the key, never a form submit.
    pub fn keydown(self, ev: &web_sys::KeyboardEvent, send: impl Fn()) -> bool {
        let Self { picked, mention, command, .. } = self;
        let complete = |path, dir| self.complete.run((path, dir));
        let complete_ref = |title, reference| self.complete_ref.run((title, reference));
        let complete_command = |name| self.complete_command.run(name);
        let list = self.suggestions.get();
        if !list.is_empty() {
            match ev.key().as_str() {
                "ArrowDown" => {
                    ev.prevent_default();
                    picked.update(|p| *p = (*p + 1) % list.len());
                    return true;
                }
                "ArrowUp" => {
                    ev.prevent_default();
                    picked.update(|p| *p = (*p + list.len() - 1) % list.len());
                    return true;
                }
                "Enter" | "Tab" => {
                    let s = list[picked.get_untracked().min(list.len() - 1)].clone();
                    match s {
                        Suggestion::File(path, dir) => complete(path, dir),
                        Suggestion::Thing(key, _, _) => complete(format!("memex:{key}"), false),
                        Suggestion::Ref(title, _, reference) => complete_ref(title, reference),
                        // Enter on a command already typed in full sends it.
                        Suggestion::Command(c) if crate::util::sends(ev) && command.get_untracked().as_deref() == Some(c.name.as_str()) => {
                            ev.prevent_default();
                            command.set(None);
                            send();
                            return true;
                        }
                        Suggestion::Command(c) => complete_command(c.name),
                    }
                    ev.prevent_default();
                    return true;
                }
                "Escape" => {
                    ev.prevent_default();
                    ev.stop_propagation();
                    mention.set(None);
                    command.set(None);
                    return true;
                }
                _ => {}
            }
        }
        false
    }
}

/// Commands are opt-in; a new session has neither commands nor a session ID.
/// Its destination realm may differ from the currently open one.
pub fn use_completion(
    text: RwSignal<String>,
    area: NodeRef<leptos::html::Textarea>,
    files: Signal<Option<(String, String)>>,
    session: Option<String>,
    commands: Option<Signal<Vec<c6_api::CommandInfo>>>,
    realm: Option<Signal<String>>,
) -> Completion {
    let session = StoredValue::new(session);
    let mention = RwSignal::new(None::<(usize, String)>);
    let picked = RwSignal::new(0usize);
    let matches = RwSignal::new(Vec::<(String, bool)>::new());
    let things = RwSignal::new(Vec::<c6_api::MemexThingRef>::new());
    let refs = RwSignal::new(Vec::<c6_api::Mentionable>::new());
    // Caret movement within the same query doesn't restart the debounce.
    let query = Memo::new(move |_| (mention.with(|m| m.as_ref().map(|(_, q)| q.clone())), files.get(), realm.map(|r| r.get())));
    let search = StoredValue::new_local(None::<(leptos::leptos_dom::helpers::TimeoutHandle, web_sys::AbortController)>);
    let cancel_search = move || {
        search.update_value(|s| {
            if let Some((timer, controller)) = s.take() {
                timer.clear();
                // Dropping a Rust fetch future doesn't cancel the browser request.
                // Abort all three actual fetches, including response-body reads.
                controller.abort();
            }
        })
    };
    Effect::new(move || {
        let (q, place, realm) = query.get();
        cancel_search();
        matches.set(Vec::new());
        things.set(Vec::new());
        refs.set(Vec::new());
        let Some(q) = q else { return };
        let Ok(controller) = web_sys::AbortController::new() else { return };
        let signal = controller.signal();
        let timer = set_timeout_with_handle(
            move || {
                if let Some((worker, root)) = place.filter(|(worker, root)| !worker.is_empty() && !root.is_empty()) {
                    let (q, signal) = (q.clone(), signal.clone());
                    spawn_local(async move {
                        let found = api::fs_find(worker, root, q, &signal).await.unwrap_or_default();
                        if !signal.aborted() {
                            matches.try_set(found);
                        }
                    });
                }
                if q.chars().count() >= 2 {
                    let (q, signal) = (q.clone(), signal.clone());
                    spawn_local(async move {
                        let found = api::memex_mentions(q, &signal).await.unwrap_or_default();
                        if !signal.aborted() {
                            things.try_set(found);
                        }
                    });
                }
                if q.chars().count() >= 2 {
                    let session = session.get_value();
                    spawn_local(async move {
                        let found = api::ask_mentionables(q, session, realm, &signal).await.unwrap_or_default();
                        if !signal.aborted() {
                            refs.try_set(found);
                        }
                    });
                }
            },
            std::time::Duration::from_millis(120),
        );
        if let Ok(timer) = timer {
            search.set_value(Some((timer, controller)));
        }
    });
    on_cleanup(cancel_search);
    let command = RwSignal::new(None::<String>);
    let suggestions = move || -> Vec<Suggestion> {
        if let Some(q) = command.get() {
            let q = q.to_lowercase();
            let all = commands.map(|c| c.get()).unwrap_or_default();
            let (mut starts, contains): (Vec<_>, Vec<_>) =
                all.into_iter().filter(|c| c.name.to_lowercase().contains(&q)).partition(|c| c.name.to_lowercase().starts_with(&q));
            starts.extend(contains);
            return starts.into_iter().map(Suggestion::Command).collect();
        }
        if mention.with(Option::is_some) {
            let mut list: Vec<Suggestion> = matches.get().into_iter().map(|(p, d)| Suggestion::File(p, d)).collect();
            list.extend(things.get().into_iter().take(8).map(|t| {
                let key = if t.label.is_empty() { t.id.clone() } else { t.label.clone() };
                Suggestion::Thing(key, t.kind, t.name)
            }));
            list.extend(refs.get().into_iter().take(10).map(|m| Suggestion::Ref(m.title, m.kind, m.reference)));
            list
        } else {
            Vec::new()
        }
    };
    let update_mention = move || {
        if let Some(el) = area.get_untracked() {
            let caret = el.selection_start().ok().flatten().unwrap_or(0) as usize;
            let t = text.get_untracked();
            let m = mention_at(&t, caret);
            let c = commands.and_then(|_| command_at(&t, caret));
            if m.as_ref().map(|x| &x.1) != mention.get_untracked().as_ref().map(|x| &x.1) || c != command.get_untracked() {
                picked.set(0);
            }
            if mention.get_untracked() != m {
                mention.set(m);
            }
            if command.get_untracked() != c {
                command.set(c);
            }
        }
    };
    // The suggestions go on a press outside them and the box, or Escape.
    let suggesting = Memo::new(move |_| !suggestions().is_empty());
    let suggest_pop = StoredValue::new(crate::util::dismissable(
        move || suggesting.get(),
        move || {
            mention.try_set(None);
            command.try_set(None);
        },
    ));
    let complete_command = move |name: String| {
        let t = text.get_untracked();
        let rest = t.split_once(char::is_whitespace).map(|(_, r)| r.trim_start().to_owned()).unwrap_or_default();
        let insert = format!("/{name} ");
        let caret = insert.chars().count() as u32;
        text.set(insert + &rest);
        command.set(None);
        request_animation_frame(move || {
            if let Some(el) = area.try_get_untracked().flatten() {
                let _ = el.focus();
                let _ = el.set_selection_range(caret, caret);
            }
        });
    };
    let complete = move |path: String, dir: bool| {
        let Some((at, query)) = mention.get_untracked() else { return };
        let t = text.get_untracked();
        let insert = format!("@{path}{} ", if dir { "/" } else { "" });
        let Some((new, caret)) = replace_mention(&t, at, &query, &insert) else {
            mention.set(None);
            return;
        };
        text.set(new);
        mention.set(None);
        request_animation_frame(move || {
            if let Some(el) = area.try_get_untracked().flatten() {
                let _ = el.focus();
                let _ = el.set_selection_range(caret, caret);
            }
        });
    };
    // A reference for Ask, as a link the head reads when it arrives.
    let complete_ref = move |title: String, reference: String| {
        let Some((at, query)) = mention.get_untracked() else { return };
        let t = text.get_untracked();
        let insert = format!("[@{}](c6:{reference}) ", title.replace(['[', ']'], ""));
        let Some((new, caret)) = replace_mention(&t, at, &query, &insert) else {
            mention.set(None);
            return;
        };
        text.set(new);
        mention.set(None);
        request_animation_frame(move || {
            if let Some(el) = area.get_untracked() {
                let _ = el.focus();
                let _ = el.set_selection_range(caret, caret);
            }
        });
    };
    Completion {
        mention,
        command,
        pop: suggest_pop,
        picked,
        suggestions: Signal::derive(suggestions),
        update: Callback::new(move |()| update_mention()),
        complete: Callback::new(move |(path, dir)| complete(path, dir)),
        complete_ref: Callback::new(move |(title, reference)| complete_ref(title, reference)),
        complete_command: Callback::new(complete_command),
    }
}

#[component]
pub fn CompletionList(completion: Completion, #[prop(optional)] below: bool) -> impl IntoView {
    let Completion { picked, pop: suggest_pop, .. } = completion;
    let suggestions = move || completion.suggestions.get();
    let complete = move |path, dir| completion.complete.run((path, dir));
    let complete_ref = move |title, reference| completion.complete_ref.run((title, reference));
    let complete_command = move |name| completion.complete_command.run(name);
    view! {
        // Form lists flow below the textarea so a dialog cannot clip them above.
        {move || {
            let list = suggestions();
            (!list.is_empty()).then(|| view! {
                <div data-pop=suggest_pop.get_value() class=move || format!("{} z-30 max-h-64 w-full max-w-lg overflow-y-auto rounded-md border border-zinc-200 bg-white py-1 text-xs shadow-lg dark:border-zinc-700 dark:bg-zinc-900", if below { "relative my-1" } else { "absolute bottom-full left-0 mb-1" })>
                    {list.into_iter().enumerate().map(|(i, s)| {
                        let row = move || format!("block w-full truncate px-3 py-1 text-left {}", if picked.get() == i { "bg-zinc-100 dark:bg-zinc-800" } else { "" });
                        match s {
                            Suggestion::File(path, dir) => {
                                let p2 = path.clone();
                                view! {
                                    <button type="button" class=move || format!("{} font-mono", row()) on:mousedown=move |ev| { ev.prevent_default(); complete(p2.clone(), dir); }>
                                        {if dir { format!("{path}/") } else { path }}
                                    </button>
                                }.into_any()
                            }
                            Suggestion::Thing(key, kind, name) => {
                                let k2 = key.clone();
                                view! {
                                    <button type="button" class=row on:mousedown=move |ev| { ev.prevent_default(); complete(format!("memex:{k2}"), false); }>
                                        <span class="rounded bg-violet-100 px-1 text-[10px] text-violet-800 dark:bg-violet-950 dark:text-violet-300">{kind}</span>
                                        <span class="ml-1">{name}</span>
                                        <span class="ml-2 font-mono text-muted">{key}</span>
                                    </button>
                                }.into_any()
                            }
                            Suggestion::Ref(title, kind, reference) => {
                                let (t2, r2) = (title.clone(), reference.clone());
                                view! {
                                    <button type="button" class=row on:mousedown=move |ev| { ev.prevent_default(); complete_ref(t2.clone(), r2.clone()); }>
                                        <span class="rounded bg-sky-100 px-1 text-[10px] text-sky-800 dark:bg-sky-950 dark:text-sky-300">{kind}</span>
                                        <span class="ml-1">{title}</span>
                                    </button>
                                }.into_any()
                            }
                            Suggestion::Command(c) => {
                                let n2 = c.name.clone();
                                view! {
                                    <button type="button" class=row on:mousedown=move |ev| { ev.prevent_default(); complete_command(n2.clone()); }>
                                        <span class="font-mono">{format!("/{}", c.name)}</span>
                                        {(!c.argument_hint.is_empty()).then(|| view! { <span class="ml-1 font-mono text-muted">{c.argument_hint.clone()}</span> })}
                                        <span class="ml-2 text-muted">{c.description.clone()}</span>
                                        {(c.source != "built-in").then(|| view! { <span class="ml-2 rounded bg-zinc-100 px-1 text-[10px] text-muted dark:bg-zinc-800">{c.source.clone()}</span> })}
                                    </button>
                                }.into_any()
                            }
                        }
                    }).collect_view()}
                </div>
            })
        }}
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn completion_replaces_only_the_query_at_the_caret() {
        let text = "é @\"deploy broke then";
        let caret = "é @\"deploy broke".chars().count();
        let (at, query) = super::mention_at(text, caret).unwrap();
        for insert in ["@src/ ", "@memex:topic:deploy ", "[@Deploy](c6:work:123) "] {
            let (new, caret) = super::replace_mention(text, at, &query, insert).unwrap();
            assert_eq!(new, format!("é {insert} then"));
            assert_eq!(caret as usize, 2 + insert.chars().count());
        }
        assert_eq!(super::mention_at("mail@example.com", 16), None);
        assert_eq!(super::mention_at("é @文", 4), Some((2, "文".into())));
        assert_eq!(super::replace_mention("", at, &query, "@file "), None, "a cleared template has no query to replace");
        assert_eq!(super::replace_mention("é @changed", at, &query, "@file "), None, "a stale query cannot overwrite new input");
    }

    #[test]
    fn both_textareas_use_shared_completion_and_only_composer_offers_commands() {
        let composer = include_str!("session.rs").split("fn Composer(").nth(1).unwrap().split("/// Shared by").next().unwrap();
        let form = include_str!("sessions.rs").split("fn SessionForm(").nth(1).unwrap().split("\n#[component]").next().unwrap();
        for source in [composer, form] {
            assert!(source.contains("crate::completion::use_completion("));
            assert!(source.contains("<crate::completion::CompletionList completion=completion"));
            assert!(source.contains("completion.keydown(&ev,"));
            assert!(source.contains("data-pop="));
            assert!(!source.contains("api::fs_find("));
            assert!(!source.contains("api::memex_mentions("));
            assert!(!source.contains("api::ask_mentionables("));
        }
        assert!(composer.contains("Some(Signal::derive(move || commands.get().unwrap_or_default()))"));
        assert!(form.contains("None,\n        None,"), "no session ID or slash commands before creation");
        assert!(form.contains("!remote.get() && !worker.is_empty() && !folder.is_empty()"));
        assert!(form.contains("template.with(Option::is_none)"), "templates choose their own execution context");
        assert!(form.contains("template.with(|t| t.as_ref().map(|t| t.realm.clone()))"), "references use the destination realm");
        assert!(form.contains("if prompt.with(String::is_empty)"), "cleared template input dismisses pending completion");
        assert!(form.contains("template.track();\n        completion.mention.set(None);"));
        assert!(form.contains("below=true"));
        assert!(form.find("if ev.is_composing()").unwrap() < form.find("completion.keydown(&ev,").unwrap());
        assert!(form.find("completion.keydown(&ev,").unwrap() < form.find("if crate::util::sends(&ev)").unwrap());
    }

    #[test]
    fn a_quoted_mention_takes_spaces_until_its_closing_quote() {
        let at = |t: &str| super::mention_at(t, t.chars().count());
        assert_eq!(at("see @deploy"), Some((4, "deploy".into())));
        assert_eq!(at("see @deploy broke"), None, "a space ends a plain mention");
        assert_eq!(at("see @\"deploy broke"), Some((4, "deploy broke".into())));
        assert_eq!(at("see @\"deploy broke\""), None, "its closing quote ends it");
        assert_eq!(at("see @\"deploy broke\" and"), None);
        assert_eq!(at("me@\"x y"), None, "only at the start of a word");
        // Picking one replaces what was typed, quote and all.
        let t: Vec<char> = "see @\"deploy broke".chars().collect();
        assert_eq!(super::mention_end(&t, 4, "deploy broke"), t.len());
        let t: Vec<char> = "see @deploy now".chars().collect();
        assert_eq!(super::mention_end(&t, 4, "deploy"), 11);
    }
}
