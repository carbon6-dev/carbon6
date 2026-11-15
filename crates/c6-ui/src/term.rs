//! Terminals in the browser: xterm.js bridged to a worker PTY over
//! `WS /api/v1/workers/{id}/terminals/{tid}/attach`.

use futures::channel::mpsc;
use futures::{SinkExt, StreamExt};
use gloo_net::websocket::Message;
use gloo_net::websocket::futures::WebSocket;
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::{Value, json};
use std::{cell::RefCell, rc::Rc};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    type TermHandle;

    #[wasm_bindgen(catch, js_namespace = c6Terminal, js_name = load)]
    async fn load_terminal() -> Result<JsValue, JsValue>;

    #[wasm_bindgen(catch, js_namespace = c6Terminal, js_name = create)]
    fn create_terminal(
        el: &web_sys::HtmlElement,
        on_data: &Closure<dyn FnMut(String)>,
        on_resize: &Closure<dyn FnMut(u32, u32)>,
    ) -> Result<TermHandle, JsValue>;

    #[wasm_bindgen(method)]
    fn write(this: &TermHandle, data: &[u8]);

    #[wasm_bindgen(method)]
    fn writeln(this: &TermHandle, line: &str);

    #[wasm_bindgen(method)]
    fn dispose(this: &TermHandle);

    #[wasm_bindgen(method)]
    fn focus(this: &TermHandle);

    #[wasm_bindgen(method, js_name = appCursor)]
    fn app_cursor(this: &TermHandle) -> bool;
}

/// Modifiers the touch key row holds for the next key (a phone's keyboard
/// has neither).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Mods {
    ctrl: bool,
    alt: bool,
}

/// What typing `text` sends with `mods` held: Ctrl turns a letter (or one of
/// @[\]^_ and space) into its control character, Alt puts Esc before it.
fn with_mods(text: &str, mods: Mods) -> String {
    if !mods.ctrl && !mods.alt {
        return text.to_owned();
    }
    let mut chars = text.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else { return text.to_owned() };
    let mut out = String::new();
    if mods.alt {
        out.push('\x1b');
    }
    match c {
        _ if mods.ctrl && c.is_ascii_alphabetic() => out.push(((c.to_ascii_uppercase() as u8) & 0x1f) as char),
        '@' | ' ' | '2' if mods.ctrl => out.push('\0'),
        '[' | '\\' | ']' | '^' | '_' if mods.ctrl => out.push(((c as u8) & 0x1f) as char),
        '?' if mods.ctrl => out.push('\x7f'),
        _ => out.push(c),
    }
    out
}

/// A key of the touch row.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Key {
    Esc,
    Tab,
    Up,
    Down,
    Right,
    Left,
    Home,
    End,
    PageUp,
    PageDown,
    Text(&'static str),
}

/// What a key of the row sends: arrows in the cursor mode the program asked
/// for, and with Ctrl or Alt as xterm encodes them (`ESC [1;5A`).
fn key_bytes(key: Key, mods: Mods, app_cursor: bool) -> String {
    let modifier = 1 + if mods.alt { 2 } else { 0 } + if mods.ctrl { 4 } else { 0 };
    let cursor = |c: char| {
        if modifier > 1 {
            format!("\x1b[1;{modifier}{c}")
        } else if app_cursor {
            format!("\x1bO{c}")
        } else {
            format!("\x1b[{c}")
        }
    };
    let tilde = |n: u8| if modifier > 1 { format!("\x1b[{n};{modifier}~") } else { format!("\x1b[{n}~") };
    match key {
        Key::Esc => "\x1b".into(),
        Key::Tab if mods.alt => "\x1b\t".into(),
        Key::Tab => "\t".into(),
        Key::Up => cursor('A'),
        Key::Down => cursor('B'),
        Key::Right => cursor('C'),
        Key::Left => cursor('D'),
        Key::Home => cursor('H'),
        Key::End => cursor('F'),
        Key::PageUp => tilde(5),
        Key::PageDown => tilde(6),
        Key::Text(t) => with_mods(t, mods),
    }
}

/// The touch key row's keys, in order: a label (or, for arrows, an icon
/// with the label as its name).
const KEYS: [(&str, Option<crate::icons::Icon>, Key); 15] = {
    use crate::icons::Icon;
    [
        ("Esc", None, Key::Esc),
        ("Tab", None, Key::Tab),
        ("Left", Some(Icon::ArrowLeft), Key::Left),
        ("Up", Some(Icon::ArrowUp), Key::Up),
        ("Down", Some(Icon::ArrowDown), Key::Down),
        ("Right", Some(Icon::ArrowRight), Key::Right),
        ("|", None, Key::Text("|")),
        ("~", None, Key::Text("~")),
        ("/", None, Key::Text("/")),
        ("-", None, Key::Text("-")),
        ("Home", None, Key::Home),
        ("End", None, Key::End),
        ("PgUp", None, Key::PageUp),
        ("PgDn", None, Key::PageDown),
        ("^C", None, Key::Text("\x03")),
    ]
};

enum Out {
    Bytes(Vec<u8>),
    Resize(u32, u32),
}

/// Dispose xterm/its observer before dropping callbacks it may still call.
struct MountedTerminal {
    term: Rc<TermHandle>,
    _on_data: Closure<dyn FnMut(String)>,
    _on_resize: Closure<dyn FnMut(u32, u32)>,
}

impl Drop for MountedTerminal {
    fn drop(&mut self) {
        self.term.dispose();
    }
}

/// One live terminal. Reattaching shows the scrollback, so leaving the page
/// and coming back loses nothing.
#[component]
pub fn TerminalView(worker_id: String, terminal_id: String) -> impl IntoView {
    let el = NodeRef::<leptos::html::Div>::new();
    let ws_path = format!("/api/v1/workers/{worker_id}/terminals/{terminal_id}/attach");
    let loading = RwSignal::new(true);
    let error = RwSignal::new(None::<String>);
    // The touch key row (a phone's keyboard has no Ctrl, Esc or arrows):
    // the held modifiers, and where its keys go.
    let mods = RwSignal::new(Mods::default());
    let input = StoredValue::new(None::<mpsc::UnboundedSender<Out>>);
    let handle = StoredValue::new_local(None::<Rc<TermHandle>>);

    Effect::new(move || {
        let Some(div) = el.get() else { return };
        let mounted = Rc::new(RefCell::new(None::<MountedTerminal>));
        let for_cleanup = send_wrapper::SendWrapper::new(mounted.clone());
        let url = crate::api::ws_url(&ws_path);

        let (fut, abort) = futures::future::abortable(async move {
            // The shared JS promise continues if this view closes. This future
            // does not: aborting prevents late DOM/PTY attachment and callbacks.
            if let Err(e) = load_terminal().await {
                loading.set(false);
                error.set(Some(format!("Cannot load terminal: {e:?}. Close and reopen to retry.")));
                return;
            }
            let (out_tx, mut out_rx) = mpsc::unbounded::<Out>();
            let data_tx = out_tx.clone();
            input.set_value(Some(data_tx.clone()));
            let on_data = Closure::<dyn FnMut(String)>::new(move |s: String| {
                // A held Ctrl or Alt applies to the next key typed, once.
                let held = mods.get_untracked();
                let s = with_mods(&s, held);
                if held != Mods::default() {
                    mods.set(Mods::default());
                }
                let _ = data_tx.unbounded_send(Out::Bytes(s.into_bytes()));
            });
            let on_resize = Closure::<dyn FnMut(u32, u32)>::new(move |c, r| {
                let _ = out_tx.unbounded_send(Out::Resize(c, r));
            });
            let t = match create_terminal(&div, &on_data, &on_resize) {
                Ok(term) => Rc::new(term),
                Err(e) => {
                    loading.set(false);
                    error.set(Some(format!("Cannot open terminal: {e:?}. Close and reopen to retry.")));
                    return;
                }
            };
            *mounted.borrow_mut() = Some(MountedTerminal { term: t.clone(), _on_data: on_data, _on_resize: on_resize });
            handle.set_value(Some(t.clone()));
            loading.set(false);
            let ws = match WebSocket::open(&url) {
                Ok(ws) => ws,
                Err(e) => return t.writeln(&format!("\r\n[cannot connect: {e}]")),
            };
            let (mut sink, mut stream) = ws.split();
            let forward = async move {
                while let Some(o) = out_rx.next().await {
                    let msg = match o {
                        Out::Bytes(b) => Message::Bytes(b),
                        Out::Resize(cols, rows) => Message::Text(json!({"type": "resize", "cols": cols, "rows": rows}).to_string()),
                    };
                    if sink.send(msg).await.is_err() {
                        break;
                    }
                }
            };
            let receive = async {
                while let Some(Ok(msg)) = stream.next().await {
                    match msg {
                        Message::Bytes(b) => t.write(&b),
                        Message::Text(s) => {
                            let v: Value = serde_json::from_str(&s).unwrap_or_default();
                            match v["type"].as_str() {
                                Some("exit") => t.writeln(&format!("\r\n[process exited with code {}]", v["code"])),
                                Some("error") => t.writeln(&format!("\r\n[{}]", v["message"].as_str().unwrap_or("error"))),
                                _ => {}
                            }
                        }
                    }
                }
                t.writeln("\r\n[disconnected]");
            };
            futures::pin_mut!(forward, receive);
            futures::future::select(forward, receive).await;
        });
        on_cleanup(move || {
            input.set_value(None);
            handle.set_value(None);
            abort.abort();
            for_cleanup.take().borrow_mut().take();
        });
        spawn_local(async move {
            let _ = fut.await;
        });
    });

    view! {
        <div class="relative">
            <Show when=move || loading.get()><p class="absolute left-3 top-3 text-sm text-muted">"Loading terminal…"</p></Show>
            {move || error.get().map(|e| view! { <crate::util::ErrorBox message=e /> })}
            <div node_ref=el class="h-[70vh] w-full overflow-hidden rounded-md border border-zinc-200 p-1 dark:border-zinc-800"></div>
            {crate::util::touch_first().then(|| view! { <KeyRow mods input handle /> })}
        </div>
    }
}

/// Keys a phone's keyboard lacks, under the terminal: Esc, Tab, Ctrl and
/// Alt (held for the next key; tap again to let go), arrows, Home, End, page
/// keys and awkward symbols. Pressing one never takes the focus (and the
/// keyboard) from the terminal.
#[component]
fn KeyRow(
    mods: RwSignal<Mods>,
    input: StoredValue<Option<mpsc::UnboundedSender<Out>>>,
    handle: StoredValue<Option<Rc<TermHandle>>, LocalStorage>,
) -> impl IntoView {
    let press = move |key: Key| {
        let app = handle.with_value(|h| h.as_ref().is_some_and(|t| t.app_cursor()));
        let bytes = key_bytes(key, mods.get_untracked(), app);
        mods.set(Mods::default());
        input.with_value(|tx| {
            if let Some(tx) = tx {
                let _ = tx.unbounded_send(Out::Bytes(bytes.into_bytes()));
            }
        });
        handle.with_value(|h| {
            if let Some(t) = h {
                t.focus();
            }
        });
    };
    const KEY: &str = "min-h-11 min-w-11 shrink-0 rounded-md border border-zinc-200 bg-white px-2 font-mono text-sm active:bg-zinc-200 dark:border-zinc-700 dark:bg-zinc-900 dark:active:bg-zinc-700";
    let held = |on: bool| if on { "border-sky-500 bg-sky-100 text-sky-900 dark:bg-sky-900 dark:text-sky-100" } else { "" };
    let keep = |ev: web_sys::PointerEvent| ev.prevent_default();
    view! {
        <div class="mt-1 flex gap-1 overflow-x-auto pb-1" data-term-keys="" role="toolbar" aria-label="Terminal keys">
            <button type="button" class=move || format!("{KEY} {}", held(mods.get().ctrl)) aria-pressed=move || mods.get().ctrl.to_string()
                on:pointerdown=keep on:click=move |_| mods.update(|m| m.ctrl = !m.ctrl)>"Ctrl"</button>
            <button type="button" class=move || format!("{KEY} {}", held(mods.get().alt)) aria-pressed=move || mods.get().alt.to_string()
                on:pointerdown=keep on:click=move |_| mods.update(|m| m.alt = !m.alt)>"Alt"</button>
            {KEYS.iter().map(|&(label, icon, key)| view! {
                <button type="button" class=format!("{KEY} inline-flex items-center justify-center") aria-label=label title=label
                    on:pointerdown=keep on:click=move |_| press(key)>
                    {match icon {
                        Some(i) => crate::icons::icon(i, crate::icons::Size::S16),
                        None => view! { {label} }.into_any(),
                    }}
                </button>
            }).collect_view()}
        </div>
    }
}

#[cfg(test)]
mod key_tests {
    use super::*;

    #[test]
    fn ctrl_and_alt_change_the_next_key() {
        let ctrl = Mods { ctrl: true, alt: false };
        let alt = Mods { ctrl: false, alt: true };
        assert_eq!(with_mods("c", ctrl), "\x03");
        assert_eq!(with_mods("D", ctrl), "\x04");
        assert_eq!(with_mods("[", ctrl), "\x1b");
        assert_eq!(with_mods(" ", ctrl), "\0");
        assert_eq!(with_mods("b", alt), "\x1bb");
        assert_eq!(with_mods("x", Mods { ctrl: true, alt: true }), "\x1b\x18");
        assert_eq!(with_mods("ab", ctrl), "ab", "pasted text is left alone");
        assert_eq!(with_mods("c", Mods::default()), "c");
    }

    #[test]
    fn keys_send_what_a_terminal_expects() {
        let none = Mods::default();
        assert_eq!(key_bytes(Key::Up, none, false), "\x1b[A");
        assert_eq!(key_bytes(Key::Up, none, true), "\x1bOA", "application cursor mode (vim, less)");
        assert_eq!(key_bytes(Key::Left, Mods { ctrl: true, alt: false }, false), "\x1b[1;5D");
        assert_eq!(key_bytes(Key::Right, Mods { ctrl: false, alt: true }, true), "\x1b[1;3C");
        assert_eq!(key_bytes(Key::PageUp, none, false), "\x1b[5~");
        assert_eq!(key_bytes(Key::Esc, none, false), "\x1b");
        assert_eq!(key_bytes(Key::Tab, none, false), "\t");
        assert_eq!(key_bytes(Key::Text("|"), none, false), "|");
        assert_eq!(key_bytes(Key::Text("c"), Mods { ctrl: true, alt: false }, false), "\x03");
    }
}

/// A worker's terminals: open new ones, switch, close.
#[component]
pub fn Terminals(
    worker_id: String,
    #[prop(optional)] default_cwd: Option<String>,
    #[prop(default = None)] ssh: Option<String>,
) -> impl IntoView {
    let ssh = StoredValue::new(ssh);
    let wid = StoredValue::new(worker_id);
    let list = LocalResource::new(move || crate::api::terminals(wid.get_value()));
    let active = RwSignal::new(None::<String>);
    let home = StoredValue::new(default_cwd.unwrap_or_default());
    let cwd = RwSignal::new(home.get_value());
    // Another folder than the session's: asked for only behind "…".
    let picking = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);

    let open = move || {
        spawn_local(async move {
            match crate::api::open_terminal(wid.get_value(), cwd.get_untracked(), ssh.get_value()).await {
                Ok(t) => {
                    active.set(Some(t.id));
                    list.refetch();
                    error.set(None);
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };
    // A session's pane shows a terminal at once: one open in its folder,
    // or a new one there.
    let started = StoredValue::new(false);
    Effect::new(move || {
        if started.get_value() || active.get_untracked().is_some() {
            return;
        }
        let Some(Ok(ts)) = list.get() else { return };
        started.set_value(true);
        let live = || ts.iter().rev().filter(|t| t.exited.is_none());
        let here = home.with_value(|h| live().find(|t| !h.is_empty() && t.cwd.trim_end_matches('/') == h.trim_end_matches('/')).cloned());
        // A worker's page (no folder of its own) only shows the newest.
        let newest = home.with_value(String::is_empty).then(|| live().next().cloned()).flatten();
        match here.or(newest) {
            Some(t) => active.set(Some(t.id)),
            None if !home.with_value(String::is_empty) => open(),
            None => {}
        }
    });
    let close = move |id: String| {
        spawn_local(async move {
            let _ = crate::api::close_terminal(wid.get_value(), id.clone()).await;
            if active.get_untracked().as_deref() == Some(&id) {
                active.set(None);
            }
            list.refetch();
        });
    };

    view! {
        <div class="space-y-3">
            <div class="flex flex-wrap items-center gap-2">
                {move || list.get().and_then(Result::ok).unwrap_or_default().into_iter().map(|t| {
                    let (id, id2) = (t.id.clone(), t.id.clone());
                    let label = t.cwd.rsplit('/').next().unwrap_or("/").to_owned();
                    view! {
                        <span class=move || if active.get().as_deref() == Some(id.as_str()) { "flex items-center rounded-md bg-zinc-200 text-sm dark:bg-zinc-800" } else { "flex items-center rounded-md text-sm hover:bg-zinc-100 dark:hover:bg-zinc-900" }>
                            <button class="px-2 py-1 font-mono text-xs" title=t.cwd.clone() on:click=move |_| active.set(Some(id2.clone()))>
                                {label}{t.exited.map(|c| format!(" (exited {c})"))}
                            </button>
                            <button class="px-1.5 text-muted hover:text-red-600" title="Close" aria-label="Close terminal" on:click={let id = t.id.clone(); move |_| close(id.clone())}>{crate::icons::icon(crate::icons::Icon::X, crate::icons::Size::S14)}</button>
                        </span>
                    }
                }).collect_view()}
                // Another terminal: + in the session's folder, … for another.
                <button class="flex size-6 items-center justify-center rounded-md text-muted hover:bg-zinc-100 hover:text-zinc-900 dark:hover:bg-zinc-800 dark:hover:text-zinc-100 pointer-coarse:size-9"
                    title="New terminal in the session's folder" aria-label="New terminal"
                    on:click=move |_| { cwd.set(home.get_value()); picking.set(false); open(); }>
                    {crate::icons::icon(crate::icons::Icon::Plus, crate::icons::Size::S14)}
                </button>
                <button class="flex h-6 items-center rounded-md px-1 text-muted hover:bg-zinc-100 hover:text-zinc-900 dark:hover:bg-zinc-800 dark:hover:text-zinc-100 pointer-coarse:h-9"
                    title="New terminal in another folder" aria-label="New terminal in another folder" on:click=move |_| picking.update(|p| *p = !*p)>{crate::icons::icon(crate::icons::Icon::Ellipsis, crate::icons::Size::S14)}</button>
                {move || picking.get().then(|| view! {
                    <form class="flex min-w-0 max-w-full items-center gap-1" on:submit=move |ev| { ev.prevent_default(); picking.set(false); open(); }>
                        <input class=format!("{} min-w-0 w-64 font-mono text-xs", crate::util::INPUT) placeholder="folder (empty: home)" autofocus=true
                            prop:value=move || cwd.get() on:input=move |ev| cwd.set(event_target_value(&ev))
                            on:keydown=move |ev| if ev.key() == "Escape" { ev.stop_propagation(); picking.set(false) } />
                        <button type="submit" class=crate::util::BUTTON_QUIET>"Open"</button>
                    </form>
                })}
            </div>
            {move || error.get().map(|e| view! { <crate::util::ErrorBox message=e /> })}
            {move || match active.get() {
                Some(tid) => view! { <TerminalView worker_id=wid.get_value() terminal_id=tid /> }.into_any(),
                None if !started.get_value() || error.get().is_none() && list.get().is_none() => view! { <p class="py-10 text-center text-sm text-muted">"Opening a terminal…"</p> }.into_any(),
                None => view! { <p class="py-10 text-center text-sm text-muted">"No terminal open: + opens one."</p> }.into_any(),
            }}
        </div>
    }
}
