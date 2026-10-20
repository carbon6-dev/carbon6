//! browser: a headless Chromium of the session's own (its own profile, so
//! sign-ins last across turns), driven over the DevTools protocol. The
//! agent opens pages, reads them as text with numbered interactive
//! elements, clicks, types (secrets by name, never seen by the model),
//! and saves screenshots to the session's Outputs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine;
use c6_llm::ToolSpec;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

use crate::{Concurrency, SessionTools, Tool, ToolCtx, ToolOutput};

/// A browser unused this long is closed (it comes back on the next call,
/// with the same profile).
const IDLE: Duration = Duration::from_secs(10 * 60);
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_TEXT: usize = 30_000;

/// A Chromium-family browser on this machine: `C6_CHROME`, else the usual
/// places.
pub fn find_chrome() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("C6_CHROME") {
        return Some(PathBuf::from(p)).filter(|p| p.exists());
    }
    let mac = [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
    ];
    if let Some(p) = mac.iter().map(PathBuf::from).find(|p| p.exists()) {
        return Some(p);
    }
    let path = std::env::var_os("PATH")?;
    for name in ["google-chrome", "google-chrome-stable", "chromium", "chromium-browser", "brave-browser", "microsoft-edge"] {
        if let Some(p) = std::env::split_paths(&path).map(|d| d.join(name)).find(|p| p.is_file()) {
            return Some(p);
        }
    }
    None
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

/// A DevTools protocol connection.
struct Cdp {
    out: mpsc::UnboundedSender<String>,
    pending: Pending,
    next: AtomicU64,
}

impl Cdp {
    async fn connect(url: &str) -> Result<Self, String> {
        let (ws, _) = tokio_tungstenite::connect_async(url).await.map_err(|e| format!("connecting to the browser: {e}"))?;
        let (mut sink, mut stream) = ws.split();
        let (out, mut rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            while let Some(m) = rx.recv().await {
                if sink.send(Message::text(m)).await.is_err() {
                    break;
                }
            }
        });
        let pending: Pending = Arc::default();
        let p = pending.clone();
        tokio::spawn(async move {
            while let Some(Ok(m)) = stream.next().await {
                let Ok(text) = m.to_text() else { continue };
                let Ok(v) = serde_json::from_str::<Value>(text) else { continue };
                // Events have no id; only replies are of interest.
                let Some(id) = v["id"].as_u64() else { continue };
                if let Some(tx) = p.lock().unwrap().remove(&id) {
                    let r = match v.get("error") {
                        Some(e) => Err(e["message"].as_str().unwrap_or("browser error").to_owned()),
                        None => Ok(v["result"].clone()),
                    };
                    let _ = tx.send(r);
                }
            }
            // Closed: fail whatever waits.
            p.lock().unwrap().clear();
        });
        Ok(Self { out, pending, next: AtomicU64::new(1) })
    }

    async fn call(&self, session: Option<&str>, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let mut msg = json!({"id": id, "method": method, "params": params});
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        self.out.send(msg.to_string()).map_err(|_| "the browser has closed".to_owned())?;
        match tokio::time::timeout(CALL_TIMEOUT, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err("the browser has closed".into()),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(format!("the browser did not answer {method} in {}s", CALL_TIMEOUT.as_secs()))
            }
        }
    }
}

/// A running browser with one page.
pub struct Browser {
    _child: tokio::process::Child,
    cdp: Cdp,
    page: String,
    last_used: Instant,
    screenshots: u32,
    /// The last screenshot, for the model to see.
    shot: Option<Vec<u8>>,
}

/// How long a browser may take to start.
const START_SECS: u64 = 60;

impl Browser {
    async fn launch(chrome: &Path, profile: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(profile).map_err(|e| format!("creating the browser profile: {e}"))?;
        let port_file = profile.join("DevToolsActivePort");
        let _ = std::fs::remove_file(&port_file);
        // What it says on the way (why it didn't start, if it doesn't).
        let log_path = profile.join("c6-browser.log");
        let log = std::fs::File::create(&log_path).map_err(|e| format!("creating {}: {e}", log_path.display()))?;
        let mut child = tokio::process::Command::new(chrome)
            .args([
                "--headless=new",
                "--remote-debugging-port=0",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-features=Translate,MediaRouter",
                "--window-size=1280,900",
                "about:blank",
            ])
            .arg(format!("--user-data-dir={}", profile.display()))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(log)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("starting {}: {e}", chrome.display()))?;
        // The browser writes its port and path when it is ready. A first
        // start on a busy machine can be slow; one that exits says why.
        let said = || {
            let s = std::fs::read_to_string(&log_path).unwrap_or_default();
            let tail: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
            tail[tail.len().saturating_sub(5)..].join("\n")
        };
        let deadline = Instant::now() + Duration::from_secs(START_SECS);
        let url = loop {
            if let Ok(s) = std::fs::read_to_string(&port_file) {
                let mut l = s.lines();
                if let (Some(port), Some(path)) = (l.next(), l.next()) {
                    break format!("ws://127.0.0.1:{}{}", port.trim(), path.trim());
                }
            }
            if let Ok(Some(status)) = child.try_wait() {
                return Err(format!("the browser exited as it started ({status}): {}", said()));
            }
            if Instant::now() > deadline {
                return Err(format!("the browser did not start in {START_SECS} seconds: {}", said()));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        let cdp = Cdp::connect(&url).await?;
        let target = cdp.call(None, "Target.createTarget", json!({"url": "about:blank"})).await?;
        let target_id = target["targetId"].as_str().ok_or("no page")?.to_owned();
        let attached = cdp.call(None, "Target.attachToTarget", json!({"targetId": target_id, "flatten": true})).await?;
        let page = attached["sessionId"].as_str().ok_or("no page session")?.to_owned();
        cdp.call(Some(&page), "Page.enable", json!({})).await?;
        Ok(Self { _child: child, cdp, page, last_used: Instant::now(), screenshots: 0, shot: None })
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        self.cdp.call(Some(&self.page), method, params).await
    }

    /// Runs JavaScript in the page; its (JSON) value.
    async fn eval(&self, expression: &str) -> Result<Value, String> {
        let r = self
            .call("Runtime.evaluate", json!({"expression": expression, "returnByValue": true, "awaitPromise": true, "userGesture": true}))
            .await?;
        if let Some(ex) = r.get("exceptionDetails") {
            let msg = ex["exception"]["description"].as_str().or(ex["text"].as_str()).unwrap_or("script error");
            return Err(msg.lines().next().unwrap_or(msg).to_owned());
        }
        Ok(r["result"]["value"].clone())
    }

    /// Waits for the page to finish loading (up to 15 s), after a moment
    /// for a navigation to begin.
    async fn settle(&self) {
        tokio::time::sleep(Duration::from_millis(400)).await;
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if self.eval("document.readyState").await.ok().as_ref().and_then(Value::as_str) == Some("complete") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    async fn location(&self) -> (String, String) {
        let v = self.eval("({title: document.title, url: location.href})").await.unwrap_or_default();
        (v["title"].as_str().unwrap_or_default().to_owned(), v["url"].as_str().unwrap_or_default().to_owned())
    }

    /// The page as text, with its interactive elements numbered.
    async fn read(&self) -> Result<String, String> {
        let v = self.eval(READ_JS).await?;
        let mut out = format!("Page: {}\nURL: {}\n\n", v["title"].as_str().unwrap_or(""), v["url"].as_str().unwrap_or(""));
        out.push_str(v["text"].as_str().unwrap_or("").trim());
        if v["more"].as_bool() == Some(true) {
            out.push_str(&format!("\n\n(page text cut at {MAX_TEXT} characters; scroll, or use evaluate to pick out what you need)"));
        }
        let els = v["elements"].as_array().cloned().unwrap_or_default();
        if !els.is_empty() {
            out.push_str("\n\nInteractive elements (pass the number as ref to click, type or select):\n");
            for e in &els {
                let kind = match (e["tag"].as_str().unwrap_or(""), e["type"].as_str().unwrap_or(""), e["role"].as_str().unwrap_or("")) {
                    ("a", _, _) => "link".to_owned(),
                    ("input", t, _) => format!("input[{}]", if t.is_empty() { "text" } else { t }),
                    (_, _, r) if !r.is_empty() => r.to_owned(),
                    (t, _, _) => t.to_owned(),
                };
                out.push_str(&format!("[{}] {kind} {:?}", e["ref"], e["label"].as_str().unwrap_or("")));
                if let Some(v) = e["value"].as_str().filter(|v| !v.is_empty()) {
                    out.push_str(&format!(" value={v:?}"));
                }
                if let Some(h) = e["href"].as_str().filter(|h| !h.is_empty()) {
                    out.push_str(&format!(" → {h}"));
                }
                out.push('\n');
            }
        }
        Ok(out)
    }

    /// Scrolls an element into view; its center.
    async fn center(&self, r: u64) -> Result<(f64, f64), String> {
        let v = self
            .eval(&format!(
                "(() => {{ const el = document.querySelector('[data-c6-ref=\"{r}\"]'); if (!el) return null; \
                 el.scrollIntoView({{block: 'center', inline: 'center'}}); const b = el.getBoundingClientRect(); \
                 return {{x: b.left + b.width / 2, y: b.top + b.height / 2}}; }})()"
            ))
            .await?;
        match (v["x"].as_f64(), v["y"].as_f64()) {
            (Some(x), Some(y)) => Ok((x, y)),
            _ => Err(format!("no element [{r}] on this page now; read the page again for current numbers")),
        }
    }

    async fn click(&self, r: u64) -> Result<(), String> {
        let (x, y) = self.center(r).await?;
        self.call("Input.dispatchMouseEvent", json!({"type": "mouseMoved", "x": x, "y": y})).await?;
        for kind in ["mousePressed", "mouseReleased"] {
            self.call("Input.dispatchMouseEvent", json!({"type": kind, "x": x, "y": y, "button": "left", "clickCount": 1})).await?;
        }
        Ok(())
    }

    async fn key(&self, name: &str) -> Result<(), String> {
        let (code, text) = match name {
            "Enter" => (13, "\r"),
            "Tab" => (9, ""),
            "Escape" => (27, ""),
            "Backspace" => (8, ""),
            "Delete" => (46, ""),
            "Space" => (32, " "),
            "ArrowUp" => (38, ""),
            "ArrowDown" => (40, ""),
            "ArrowLeft" => (37, ""),
            "ArrowRight" => (39, ""),
            "PageUp" => (33, ""),
            "PageDown" => (34, ""),
            "Home" => (36, ""),
            "End" => (35, ""),
            other => {
                return Err(format!(
                    "unknown key {other:?} (Enter, Tab, Escape, Backspace, Delete, Space, Arrow…, PageUp/Down, Home, End)"
                ));
            }
        };
        let key = if name == "Space" { " " } else { name };
        let mut down = json!({"type": "keyDown", "key": key, "code": name, "windowsVirtualKeyCode": code});
        if !text.is_empty() {
            down["text"] = json!(text);
        }
        self.call("Input.dispatchKeyEvent", down).await?;
        self.call("Input.dispatchKeyEvent", json!({"type": "keyUp", "key": key, "code": name, "windowsVirtualKeyCode": code})).await?;
        Ok(())
    }
}

/// Numbers the page's visible interactive elements (data-c6-ref) and
/// returns them with its text.
const READ_JS: &str = r#"(() => {
  const visible = el => { const r = el.getBoundingClientRect(); const s = getComputedStyle(el);
    return r.width > 0 && r.height > 0 && s.visibility !== 'hidden' && s.display !== 'none'; };
  const sel = 'a[href], button, input:not([type=hidden]), textarea, select, summary, [role=button], [role=link], [role=checkbox], '
    + '[role=radio], [role=tab], [role=menuitem], [role=option], [role=switch], [contenteditable=""], [contenteditable=true]';
  document.querySelectorAll('[data-c6-ref]').forEach(e => e.removeAttribute('data-c6-ref'));
  const els = []; let n = 0;
  for (const el of document.querySelectorAll(sel)) {
    if (els.length >= 400 || !visible(el)) continue;
    n++; el.setAttribute('data-c6-ref', String(n));
    const tag = el.tagName.toLowerCase();
    const label = (el.getAttribute('aria-label') || el.innerText || el.placeholder || el.title || el.name || el.alt || (tag === 'input' && ['submit','button'].includes(el.type) ? el.value : '') || '')
      .trim().replace(/\s+/g, ' ').slice(0, 100);
    const showValue = (tag === 'input' && !['password','submit','button','checkbox','radio'].includes(el.type)) || tag === 'textarea' || tag === 'select';
    const checked = (el.type === 'checkbox' || el.type === 'radio') ? (el.checked ? 'checked' : 'unchecked') : '';
    els.push({ref: n, tag, type: el.type || '', role: el.getAttribute('role') || '', label,
      value: showValue ? String(el.value).slice(0, 100) : checked, href: tag === 'a' ? el.href : ''});
  }
  const text = (document.body ? document.body.innerText : '').replace(/\n{3,}/g, '\n\n');
  return {title: document.title, url: location.href, text: text.slice(0, 30000), more: text.length > 30000, elements: els};
})()"#;

/// The session's browser slot: launched on first use, closed when idle.
#[derive(Default)]
pub struct BrowserSlot {
    browser: tokio::sync::Mutex<Option<Browser>>,
    profile: Mutex<Option<PathBuf>>,
}

impl BrowserSlot {
    /// Where the browser keeps its profile (cookies, sign-ins).
    pub fn set_profile(&self, dir: PathBuf) {
        *self.profile.lock().unwrap() = Some(dir);
    }

    fn profile(&self) -> PathBuf {
        self.profile.lock().unwrap().clone().unwrap_or_else(|| {
            let n: u64 = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0);
            let p = std::env::temp_dir().join(format!("c6-browser-{}-{n}", std::process::id()));
            *self.profile.lock().unwrap() = Some(p.clone());
            p
        })
    }
}

/// Closes a session's browser once it has been idle a while.
fn watch_idle(session: Weak<SessionTools>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            let Some(s) = session.upgrade() else { return };
            let mut b = s.browser.browser.lock().await;
            match b.as_ref() {
                None => return,
                Some(open) if open.last_used.elapsed() > IDLE => {
                    *b = None;
                    return;
                }
                Some(_) => {}
            }
        }
    });
}

pub struct BrowserTool {
    claude: bool,
    chrome: PathBuf,
}

impl BrowserTool {
    pub fn new(claude: bool, chrome: PathBuf) -> Self {
        Self { claude, chrome }
    }
}

#[async_trait]
impl Tool for BrowserTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: if self.claude { "Browser" } else { "browser" }.into(),
            description: "A web browser of this session's own (headless Chromium; sign-ins last for the session). Actions: \
                          open {url}: go to a page and read it. read: the page's text and its interactive elements, numbered. \
                          click {ref}. type {ref, text} (replaces what is there; submit: true presses Enter after), or \
                          {ref, secret} to type a secret by name without seeing it. select {ref, value}: pick an option. \
                          press {key}: Enter, Tab, Escape, ArrowDown, …. scroll {direction: down|up}. back. \
                          screenshot: see the page as an image (selector: just that element, e.g. one slide; full_page: \
                          the whole page; width and height: set the window size first, e.g. 1280×720; save: true also \
                          puts it in the session's Outputs for the user). Use it to check how something you made looks. \
                          pdf {path}: print the page to a PDF file (page size from the page's CSS @page, backgrounds \
                          included; save: true also puts it in Outputs). evaluate {script}: run JavaScript \
                          in the page and get its JSON value. close. Element numbers change when the page does: read again \
                          after a page changes. Prefer web search and fetching for plain lookups; use the browser for pages \
                          that need JavaScript, signing in, or filling forms."
                .into(),
            parameters: json!({"type": "object", "properties": {
                "action": {"type": "string", "enum": ["open", "read", "click", "type", "select", "press", "scroll", "back", "screenshot", "pdf", "evaluate", "close"]},
                "url": {"type": "string"},
                "ref": {"type": "integer", "description": "An element's number from read"},
                "text": {"type": "string"},
                "secret": {"type": "string", "description": "type: a secret's name, typed in its place"},
                "submit": {"type": "boolean"},
                "value": {"type": "string", "description": "select: the option's value or label"},
                "key": {"type": "string"},
                "direction": {"type": "string", "enum": ["down", "up"]},
                "script": {"type": "string"},
                "selector": {"type": "string", "description": "screenshot: a CSS selector; only that element"},
                "full_page": {"type": "boolean"},
                "width": {"type": "integer"},
                "height": {"type": "integer"},
                "save": {"type": "boolean", "description": "screenshot, pdf: also put it in the session's Outputs"},
                "path": {"type": "string", "description": "pdf: the file to write"}
            }, "required": ["action"], "additionalProperties": false}),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Exclusive
    }

    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput {
        let action = input["action"].as_str().unwrap_or("");
        let secrets = ctx.session.secrets().clone();
        let mut slot = ctx.session.browser.browser.lock().await;
        if action == "close" {
            return ToolOutput::ok(if slot.take().is_some() { "Closed the browser." } else { "The browser was not open." });
        }
        if slot.is_none() {
            match Browser::launch(&self.chrome, &ctx.session.browser.profile()).await {
                Ok(b) => {
                    *slot = Some(b);
                    watch_idle(Arc::downgrade(&ctx.session));
                }
                Err(e) => return ToolOutput::err(e),
            }
        }
        let b = slot.as_mut().expect("launched");
        b.last_used = Instant::now();
        let r = tokio::select! {
            r = act(b, ctx, action, &input) => r,
            _ = ctx.cancel.cancelled() => Err("interrupted".into()),
        };
        // A browser that stopped answering is replaced on the next call.
        if let Err(e) = &r
            && e.contains("has closed")
        {
            *slot = None;
        }
        let shot = b_shot(&mut slot);
        match r {
            Ok(text) => match shot {
                Some(png) => ToolOutput::limited(secrets.redact(&text), false)
                    .with_image(if png.starts_with(b"\x89PNG") { "image/png" } else { "image/jpeg" }, png),
                None => ToolOutput::limited(secrets.redact(&text), false),
            },
            Err(e) => ToolOutput::err(secrets.redact(&e)),
        }
    }
}

/// The screenshot the last action took, if any.
fn b_shot(slot: &mut Option<Browser>) -> Option<Vec<u8>> {
    slot.as_mut().and_then(|b| b.shot.take())
}

fn want_ref(input: &Value) -> Result<u64, String> {
    input["ref"].as_u64().ok_or_else(|| "ref (an element's number from read) is required".into())
}

async fn act(b: &mut Browser, ctx: &ToolCtx, action: &str, input: &Value) -> Result<String, String> {
    match action {
        "open" => {
            let url = input["url"].as_str().map(str::trim).filter(|u| !u.is_empty()).ok_or("url is required")?;
            let url = if url.contains("://") || url.starts_with("about:") { url.to_owned() } else { format!("https://{url}") };
            let r = b.call("Page.navigate", json!({"url": url})).await?;
            if let Some(e) = r["errorText"].as_str().filter(|e| !e.is_empty()) {
                return Err(format!("could not open {url}: {e}"));
            }
            b.settle().await;
            b.read().await
        }
        "read" => b.read().await,
        "back" => {
            b.eval("history.back()").await?;
            b.settle().await;
            b.read().await
        }
        "click" => {
            let r = want_ref(input)?;
            let (_, before) = b.location().await;
            b.click(r).await?;
            b.settle().await;
            let (title, after) = b.location().await;
            if after != before {
                Ok(format!("Clicked [{r}]; the page changed.\n\n{}", b.read().await?))
            } else {
                Ok(format!("Clicked [{r}] on {title} ({after}). Read the page to see what changed."))
            }
        }
        "type" => {
            let r = want_ref(input)?;
            let (text, what) = match (input["secret"].as_str().filter(|s| !s.is_empty()), input["text"].as_str()) {
                (Some(name), _) => match ctx.session.secrets().get(name) {
                    Some(v) => (v, format!("the secret {name}")),
                    None => return Err(format!("no secret named {name} applies to this session")),
                },
                (None, Some(t)) => (t.to_owned(), format!("{} characters", t.chars().count())),
                (None, None) => return Err("text (or secret) is required".into()),
            };
            b.center(r).await?;
            // Focus it and select what is there, so typing replaces it.
            let focused = b
                .eval(&format!(
                    "(() => {{ const el = document.querySelector('[data-c6-ref=\"{r}\"]'); if (!el) return false; el.focus(); \
                     if (el.select) el.select(); else if (el.isContentEditable) document.execCommand('selectAll'); return true; }})()"
                ))
                .await?;
            if focused != json!(true) {
                return Err(format!("no element [{r}] on this page now; read the page again"));
            }
            b.call("Input.insertText", json!({"text": text})).await?;
            if input["submit"].as_bool() == Some(true) {
                b.key("Enter").await?;
                b.settle().await;
                return Ok(format!("Typed {what} into [{r}] and pressed Enter.\n\n{}", b.read().await?));
            }
            Ok(format!("Typed {what} into [{r}]."))
        }
        "select" => {
            let r = want_ref(input)?;
            let want = input["value"].as_str().ok_or("value is required")?;
            let picked = b
                .eval(&format!(
                    "(() => {{ const el = document.querySelector('[data-c6-ref=\"{r}\"]'); if (!el || !el.options) return null; \
                     const w = {w}; const o = [...el.options].find(o => o.value === w || o.text.trim() === w); if (!o) return [...el.options].map(o => o.text.trim()); \
                     el.value = o.value; el.dispatchEvent(new Event('input', {{bubbles: true}})); el.dispatchEvent(new Event('change', {{bubbles: true}})); return o.text.trim(); }})()",
                    w = serde_json::to_string(want).unwrap_or_default()
                ))
                .await?;
            match picked {
                Value::String(s) => Ok(format!("Selected {s:?} in [{r}].")),
                Value::Array(opts) => Err(format!("no option {want:?} in [{r}]; its options: {}", Value::Array(opts))),
                _ => Err(format!("[{r}] is not a list to select from (read the page again)")),
            }
        }
        "press" => {
            let key = input["key"].as_str().ok_or("key is required")?;
            b.key(key).await?;
            b.settle().await;
            let (title, url) = b.location().await;
            Ok(format!("Pressed {key} on {title} ({url})."))
        }
        "scroll" => {
            let dy = if input["direction"].as_str() == Some("up") { -0.8 } else { 0.8 };
            let v = b.eval(&format!("(() => {{ scrollBy(0, innerHeight * {dy}); return {{y: scrollY, h: document.body ? document.body.scrollHeight : 0}}; }})()")).await?;
            Ok(format!("Scrolled {}; now at {} of {} pixels.", if dy < 0.0 { "up" } else { "down" }, v["y"], v["h"]))
        }
        "screenshot" => {
            if let (Some(w), Some(h)) = (input["width"].as_u64(), input["height"].as_u64()) {
                b.call("Emulation.setDeviceMetricsOverride", json!({"width": w, "height": h, "deviceScaleFactor": 1, "mobile": false}))
                    .await?;
            }
            // What to capture: an element, the whole page, or the window.
            let clip = if let Some(sel) = input["selector"].as_str().filter(|s| !s.trim().is_empty()) {
                let r = b
                    .eval(&format!(
                        "(() => {{ const e = document.querySelector({}); if (!e) return null; e.scrollIntoView(); const r = e.getBoundingClientRect(); \
                         return {{x: r.left + scrollX, y: r.top + scrollY, width: r.width, height: r.height}}; }})()",
                        serde_json::to_string(sel).unwrap_or_default()
                    ))
                    .await?;
                if r.is_null() {
                    return Err(format!("nothing on the page matches {sel}"));
                }
                Some(r)
            } else if input["full_page"].as_bool() == Some(true) {
                Some(
                    b.eval("({x: 0, y: 0, width: document.documentElement.scrollWidth, height: document.documentElement.scrollHeight})")
                        .await?,
                )
            } else {
                None
            };
            let capture = |format: &str| {
                let mut p = json!({"format": format, "captureBeyondViewport": clip.is_some()});
                if format == "jpeg" {
                    p["quality"] = json!(75);
                }
                if let Some(c) = &clip {
                    p["clip"] = json!({"x": c["x"], "y": c["y"], "width": c["width"], "height": c["height"], "scale": 1});
                }
                p
            };
            let decode = |v: &Value| {
                base64::engine::general_purpose::STANDARD
                    .decode(v["data"].as_str().unwrap_or(""))
                    .map_err(|e| format!("reading the screenshot: {e}"))
            };
            let mut data = decode(&b.call("Page.captureScreenshot", capture("png")).await?)?;
            let mut mime = "image/png";
            // Too big to show: the same as JPEG.
            if data.len() > crate::MAX_IMAGE_BYTES {
                data = decode(&b.call("Page.captureScreenshot", capture("jpeg")).await?)?;
                mime = "image/jpeg";
            }
            let (title, url) = b.location().await;
            let mut said = format!(
                "Screenshot of {}{}.",
                if title.is_empty() { &url } else { &title },
                input["selector"].as_str().map(|s| format!(" ({s})")).unwrap_or_default()
            );
            if input["save"].as_bool() == Some(true) {
                let sink = ctx.session.publisher().ok_or("Outputs are not available here")?;
                b.screenshots += 1;
                let name = format!("screenshot-{}.{}", b.screenshots, if mime == "image/png" { "png" } else { "jpg" });
                sink.send(crate::Published {
                    name: name.clone(),
                    title: if title.is_empty() { url.clone() } else { title.clone() },
                    mime: mime.into(),
                    data: data.clone(),
                })
                .map_err(|_| "Outputs are not available here".to_owned())?;
                said.push_str(&format!(" Saved as {name} in the session's Outputs."));
            }
            b.shot = Some(data);
            Ok(said)
        }
        "pdf" => {
            let path = input["path"].as_str().filter(|p| !p.trim().is_empty()).ok_or("path (the PDF file to write) is required")?;
            let v = b.call("Page.printToPDF", json!({"printBackground": true, "preferCSSPageSize": true})).await?;
            let data = base64::engine::general_purpose::STANDARD
                .decode(v["data"].as_str().unwrap_or(""))
                .map_err(|e| format!("reading the PDF: {e}"))?;
            let full = ctx.session.resolve(path);
            ctx.session.fs_write(&full, &data).await?;
            let mut said = format!("Wrote {} ({} KB).", full.display(), data.len() / 1024);
            if input["save"].as_bool() == Some(true) {
                let sink = ctx.session.publisher().ok_or("Outputs are not available here")?;
                let (title, _) = b.location().await;
                let name = full.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "page.pdf".into());
                sink.send(crate::Published { name: name.clone(), title, mime: "application/pdf".into(), data })
                    .map_err(|_| "Outputs are not available here".to_owned())?;
                said.push_str(&format!(" Saved as {name} in the session's Outputs."));
            }
            Ok(said)
        }
        "evaluate" => {
            let script = input["script"].as_str().filter(|s| !s.trim().is_empty()).ok_or("script is required")?;
            let v = b.eval(script).await?;
            Ok(if v.is_null() { "(no value)".into() } else { serde_json::to_string_pretty(&v).unwrap_or_default() })
        }
        "" => Err("action is required".into()),
        other => Err(format!("unknown action {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    const HOME: &str = r#"<html><head><title>Sign in</title></head><body><h1>Welcome</h1>
<form onsubmit="event.preventDefault(); document.getElementById('out').textContent = 'Hello ' + document.getElementById('user').value + ' / ' + document.getElementById('pw').value.length + ' / ' + document.getElementById('plan').value;">
<input id="user" placeholder="User name"> <input id="pw" type="password" placeholder="Password">
<select id="plan"><option value="f">Free</option><option value="p">Pro</option></select>
<button type="submit">Sign in</button></form><p id="out"></p><a href="/second">Next page</a></body></html>"#;
    const SECOND: &str = "<html><head><title>Second</title></head><body><p>The second page</p></body></html>";

    /// Serves the two pages.
    async fn site() -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let body = if req.starts_with("GET /second") { SECOND } else { HOME };
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = s.write_all(resp.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}/")
    }

    fn ref_of(page: &str, label: &str) -> u64 {
        let line = page.lines().find(|l| l.contains(&format!("{label:?}"))).unwrap_or_else(|| panic!("no {label:?} in {page}"));
        line[1..line.find(']').unwrap()].parse().unwrap()
    }

    #[tokio::test]
    async fn read_click_type_and_screenshot() {
        let Some(chrome) = find_chrome() else {
            eprintln!("no Chromium-family browser here; skipped");
            return;
        };
        let url = site().await;
        let dir = tempfile::tempdir().unwrap();
        let session = Arc::new(SessionTools::new(dir.path().to_owned()));
        session.browser.set_profile(dir.path().join("profile"));
        session.set_secrets(vec![("PW".into(), "hunter22".into())]);
        let (pub_tx, mut pub_rx) = mpsc::unbounded_channel();
        session.set_publisher(Some(pub_tx));
        let ctx = ToolCtx { session: session.clone(), cancel: Default::default(), output: Arc::new(|_: &str| {}), call_id: "c".into() };
        let tool = BrowserTool::new(false, chrome);
        let call = |v: Value| {
            let (tool, ctx) = (&tool, &ctx);
            async move {
                let out = tool.call(ctx, v.clone()).await;
                assert!(!out.is_error, "{v}: {}", out.content);
                out.content
            }
        };

        let page = call(json!({"action": "open", "url": url})).await;
        assert!(page.contains("Page: Sign in") && page.contains("Welcome"), "{page}");
        let (user, pw, plan, button) =
            (ref_of(&page, "User name"), ref_of(&page, "Password"), ref_of(&page, "Free Pro"), ref_of(&page, "Sign in"));
        assert!(page.contains(&format!("[{button}] button")), "{page}");

        call(json!({"action": "type", "ref": user, "text": "ada"})).await;
        let typed = call(json!({"action": "type", "ref": pw, "secret": "PW"})).await;
        assert!(typed.contains("the secret PW") && !typed.contains("hunter22"), "{typed}");
        assert!(call(json!({"action": "select", "ref": plan, "value": "Pro"})).await.contains("Selected \"Pro\""));
        call(json!({"action": "click", "ref": button})).await;
        let after = call(json!({"action": "read"})).await;
        assert!(after.contains("Hello ada / 8 / p"), "{after}");
        assert_eq!(call(json!({"action": "evaluate", "script": "document.getElementById('user').value"})).await, "\"ada\"");

        // A stale number is refused plainly; a secret's value never shows.
        let bad = tool.call(&ctx, json!({"action": "click", "ref": 999})).await;
        assert!(bad.is_error && bad.content.contains("read the page again"), "{}", bad.content);
        let leaked = call(json!({"action": "evaluate", "script": "'pw is ' + document.getElementById('pw').value"})).await;
        assert!(leaked.contains("[secret:PW]") && !leaked.contains("hunter22"), "{leaked}");

        let next = call(json!({"action": "click", "ref": ref_of(&after, "Next page")})).await;
        assert!(next.contains("the page changed") && next.contains("The second page"), "{next}");
        assert!(call(json!({"action": "back"})).await.contains("Welcome"));

        // A screenshot is shown to the model; saved to Outputs only when asked.
        let seen = tool.call(&ctx, json!({"action": "screenshot", "width": 800, "height": 450})).await;
        assert!(!seen.is_error && seen.images.len() == 1, "{}", seen.content);
        assert_eq!((seen.images[0].0.as_str(), &seen.images[0].1[1..4]), ("image/png", b"PNG".as_slice()));
        assert!(pub_rx.try_recv().is_err(), "not saved unless asked");
        let el = tool.call(&ctx, json!({"action": "screenshot", "selector": "h1"})).await;
        assert_eq!(el.images.len(), 1, "{}", el.content);
        let missing = tool.call(&ctx, json!({"action": "screenshot", "selector": "#nope"})).await;
        assert!(missing.is_error && missing.content.contains("nothing on the page matches"));
        let pdf = call(json!({"action": "pdf", "path": "page.pdf"})).await;
        assert!(pdf.contains("page.pdf"), "{pdf}");
        assert!(std::fs::read(dir.path().join("page.pdf")).unwrap().starts_with(b"%PDF"));
        assert!(call(json!({"action": "screenshot", "save": true})).await.contains("screenshot-1.png"));
        let shot = pub_rx.try_recv().unwrap();
        assert_eq!((shot.mime.as_str(), &shot.data[1..4]), ("image/png", b"PNG".as_slice()));

        assert_eq!(call(json!({"action": "close"})).await, "Closed the browser.");
        assert!(dir.path().join("profile").exists(), "its own profile");
    }
}
