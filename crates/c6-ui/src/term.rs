//! Terminals in the browser: xterm.js bridged to a worker PTY over
//! `WS /api/v1/workers/{id}/terminals/{tid}/attach`.

use futures::channel::mpsc;
use futures::{SinkExt, StreamExt};
use gloo_net::websocket::Message;
use gloo_net::websocket::futures::WebSocket;
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::{Value, json};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    type TermHandle;

    #[wasm_bindgen(js_namespace = c6Terminal, js_name = create)]
    fn create_terminal(
        el: &web_sys::HtmlElement,
        on_data: &Closure<dyn FnMut(String)>,
        on_resize: &Closure<dyn FnMut(u32, u32)>,
    ) -> TermHandle;

    #[wasm_bindgen(method)]
    fn write(this: &TermHandle, data: &[u8]);

    #[wasm_bindgen(method)]
    fn writeln(this: &TermHandle, line: &str);

    #[wasm_bindgen(method)]
    fn dispose(this: &TermHandle);
}

enum Out {
    Bytes(Vec<u8>),
    Resize(u32, u32),
}

/// One live terminal. Reattaching shows the scrollback, so leaving the page
/// and coming back loses nothing.
#[component]
pub fn TerminalView(worker_id: String, terminal_id: String) -> impl IntoView {
    let el = NodeRef::<leptos::html::Div>::new();
    let ws_path = format!("/api/v1/workers/{worker_id}/terminals/{terminal_id}/attach");

    Effect::new(move || {
        let Some(div) = el.get() else { return };
        let (out_tx, mut out_rx) = mpsc::unbounded::<Out>();
        let data_tx = out_tx.clone();
        let on_data = Closure::<dyn FnMut(String)>::new(move |s: String| {
            let _ = data_tx.unbounded_send(Out::Bytes(s.into_bytes()));
        });
        let on_resize = Closure::<dyn FnMut(u32, u32)>::new(move |c, r| {
            let _ = out_tx.unbounded_send(Out::Resize(c, r));
        });
        let term = std::rc::Rc::new(create_terminal(&div, &on_data, &on_resize));
        let url = crate::api::ws_url(&ws_path);

        let t = term.clone();
        let (fut, abort) = futures::future::abortable(async move {
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
        spawn_local(async move {
            let _ = fut.await;
        });
        let term_for_cleanup = send_wrapper::SendWrapper::new((term, on_data, on_resize));
        on_cleanup(move || {
            abort.abort();
            let (term, on_data, on_resize) = term_for_cleanup.take();
            term.dispose();
            drop((on_data, on_resize));
        });
    });

    view! { <div node_ref=el class="h-[70vh] w-full overflow-hidden rounded-md border border-zinc-200 p-1 dark:border-zinc-800"></div> }
}

/// A worker's terminals: open new ones, switch, close.
#[component]
pub fn Terminals(worker_id: String, #[prop(optional)] default_cwd: Option<String>) -> impl IntoView {
    let wid = StoredValue::new(worker_id);
    let list = LocalResource::new(move || crate::api::terminals(wid.get_value()));
    let active = RwSignal::new(None::<String>);
    let cwd = RwSignal::new(default_cwd.unwrap_or_default());
    let error = RwSignal::new(None::<String>);

    // Show the newest open terminal by default.
    Effect::new(move || {
        if active.get_untracked().is_none()
            && let Some(Ok(ts)) = list.get()
            && let Some(t) = ts.iter().rev().find(|t| t.exited.is_none())
        {
            active.set(Some(t.id.clone()));
        }
    });

    let open = move |_| {
        spawn_local(async move {
            match crate::api::open_terminal(wid.get_value(), cwd.get_untracked()).await {
                Ok(t) => {
                    active.set(Some(t.id));
                    list.refetch();
                    error.set(None);
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };
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
                            <button class="px-1.5 text-zinc-400 hover:text-red-600" title="Close" on:click={let id = t.id.clone(); move |_| close(id.clone())}>"×"</button>
                        </span>
                    }
                }).collect_view()}
                <span class="flex-1"></span>
                <input class=format!("{} w-64 font-mono text-xs", crate::util::INPUT) placeholder="directory (default: home)"
                    prop:value=move || cwd.get() on:input=move |ev| cwd.set(event_target_value(&ev)) />
                <button class=crate::util::BUTTON on:click=open>"New terminal"</button>
            </div>
            {move || error.get().map(|e| view! { <crate::util::ErrorBox message=e /> })}
            {move || match active.get() {
                Some(tid) => view! { <TerminalView worker_id=wid.get_value() terminal_id=tid /> }.into_any(),
                None => view! { <p class="py-10 text-center text-sm text-zinc-500">"No terminal open."</p> }.into_any(),
            }}
        </div>
    }
}
