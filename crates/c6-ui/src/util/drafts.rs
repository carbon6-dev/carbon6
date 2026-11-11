//! Browser-local attachment drafts. Text stays in localStorage; large payloads
//! live in IndexedDB. A count in localStorage makes eviction/failure visible.

use c6_api::AttachmentUpload;
use leptos::prelude::*;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use wasm_bindgen::prelude::*;

const LIMIT: usize = 20 * 1024 * 1024;
const SAVE_DELAY: std::time::Duration = std::time::Duration::from_millis(250);

#[derive(Default)]
struct TextSave {
    last: String,
    pending: Option<String>,
    timer: Option<leptos::leptos_dom::helpers::TimeoutHandle>,
    forgotten: bool,
}

impl TextSave {
    fn change(&mut self, text: &str) -> bool {
        if self.last == text {
            return false;
        }
        self.last = text.to_owned();
        self.pending = Some(text.to_owned());
        self.forgotten = false;
        true
    }

    fn cancel(&mut self) {
        if let Some(timer) = self.timer.take() {
            timer.clear();
        }
    }

    fn forget(&mut self) {
        self.cancel();
        self.pending = None;
        self.forgotten = true;
    }

    fn flush(&mut self, key: &str) {
        self.cancel();
        if let Some(text) = self.pending.take() {
            if text.is_empty() {
                super::storage_remove_global(key);
            } else {
                super::storage_set_global(key, &text);
            }
        }
    }
}

thread_local! {
    // Forget must cancel a queued write, not let teardown resurrect a sent
    // draft. Weak references don't keep departed composers alive.
    static TEXT_SAVES: RefCell<HashMap<String, Vec<Weak<RefCell<TextSave>>>>> = RefCell::new(HashMap::new());
}

pub(super) fn text(key: &str) -> RwSignal<String> {
    let key = super::placed_key(&format!("c6.draft.{key}"));
    let initial = super::storage_get_global(&key).unwrap_or_default();
    let text = RwSignal::new(initial.clone());
    let save = Rc::new(RefCell::new(TextSave { last: initial, ..Default::default() }));
    TEXT_SAVES.with_borrow_mut(|saves| {
        saves.retain(|_, saves| {
            saves.retain(|save| save.strong_count() > 0);
            !saves.is_empty()
        });
        saves.entry(key.clone()).or_default().push(Rc::downgrade(&save));
    });
    let save = StoredValue::new_local(save);
    let save_key = key.clone();
    Effect::new(move || {
        text.with(|t| {
            let save = save.get_value();
            if !save.borrow_mut().change(t) {
                return;
            }
            save.borrow_mut().cancel();
            let pending = save.clone();
            let key = save_key.clone();
            save.borrow_mut().timer = set_timeout_with_handle(move || pending.borrow_mut().flush(&key), SAVE_DELAY).ok();
        })
    });
    let flush_key = StoredValue::new(key);
    let flush = move || {
        save.with_value(|s| {
            let mut s = s.borrow_mut();
            // Include an edit made in this task, before the reactive save
            // effect ran. Forget must still win over teardown/pagehide.
            if !s.forgotten {
                text.try_with_untracked(|t| s.change(t));
            }
            s.flush(&flush_key.get_value());
        });
    };
    let page = window_event_listener_untyped("pagehide", move |_| flush());
    let hidden = window_event_listener_untyped("visibilitychange", move |_| {
        if window().document().is_some_and(|d| d.hidden()) {
            flush();
        }
    });
    on_cleanup(move || {
        page.remove();
        hidden.remove();
        flush();
    });
    text
}

// Keep IDB event/transaction lifetimes in JS. Updates run the Rust policy
// synchronously inside getAll's success handler, while the transaction is
// active. Resolve only at commit, not at the first successful request. The
// queue preserves save/clear order even while the database is opening.
#[wasm_bindgen(inline_js = r#"
let draftQueue = Promise.resolve();
function queuedDraft(work) {
    const result = draftQueue.then(work);
    draftQueue = result.catch(() => {});
    return result;
}
function draftDatabase() {
    return new Promise((resolve, reject) => {
        const request = indexedDB.open('c6.draft.attachments', 1);
        let blocked = false;
        request.onupgradeneeded = () => request.result.createObjectStore('drafts');
        request.onerror = () => reject(request.error);
        request.onblocked = () => { blocked = true; reject(new Error('Draft storage is blocked')); };
        request.onsuccess = () => {
            const db = request.result;
            if (blocked) { db.close(); return; }
            db.onversionchange = () => db.close();
            resolve(db);
        };
    });
}
export function loadDraftAttachments(key) {
    return queuedDraft(async () => {
        const db = await draftDatabase();
        try {
            return await new Promise((resolve, reject) => {
                const tx = db.transaction('drafts', 'readonly');
                const request = tx.objectStore('drafts').get(key);
                tx.oncomplete = () => resolve(request.result ?? '');
                tx.onabort = () => reject(tx.error ?? new Error('Draft read aborted'));
                tx.onerror = () => {};
            });
        } finally { db.close(); }
    });
}
export function updateDraftAttachments(update) {
    return queuedDraft(async () => {
        const db = await draftDatabase();
        try {
            return await new Promise((resolve, reject) => {
                const tx = db.transaction('drafts', 'readwrite');
                const store = tx.objectStore('drafts');
                const request = store.getAll();
                let failure;
                request.onsuccess = () => {
                    try {
                        const records = JSON.parse(update(JSON.stringify(request.result)));
                        store.clear();
                        for (const record of records) store.put(JSON.stringify(record), record.key);
                    } catch (error) { failure = error; tx.abort(); }
                };
                tx.oncomplete = () => resolve();
                tx.onabort = () => reject(failure ?? tx.error ?? new Error('Draft write aborted'));
                tx.onerror = () => {};
            });
        } finally { db.close(); }
    });
}
"#)]
extern "C" {
    #[wasm_bindgen(catch, js_name = loadDraftAttachments)]
    async fn load(key: &str) -> Result<JsValue, JsValue>;
    #[wasm_bindgen(catch, js_name = updateDraftAttachments)]
    async fn update(policy: &js_sys::Function) -> Result<JsValue, JsValue>;
}

#[derive(Clone, Serialize, Deserialize)]
struct File {
    order: usize,
    #[serde(flatten)]
    upload: AttachmentUpload,
}

#[derive(Clone, Serialize, Deserialize)]
struct Draft {
    key: String,
    updated: i64,
    files: Vec<File>,
}

fn record(key: String, uploads: Vec<AttachmentUpload>, updated: i64) -> Draft {
    Draft { key, updated, files: uploads.into_iter().enumerate().map(|(order, upload)| File { order, upload }).collect() }
}

/// Bound the actual serialized payload (including base64 and metadata), not
/// just decoded image sizes. Oldest modified drafts go first, as whole drafts.
fn replace(records: &mut Vec<Draft>, draft: Draft, limit: usize) -> usize {
    records.retain(|d| d.key != draft.key);
    let key = draft.key.clone();
    if !draft.files.is_empty() {
        records.push(draft);
    }
    records.sort_by(|a, b| (a.updated, &a.key).cmp(&(b.updated, &b.key)));
    let sizes: Vec<_> = records.iter().map(|d| serde_json::to_vec(d).expect("draft serializes").len()).collect();
    let mut total: usize = sizes.iter().sum();
    let mut drop = 0;
    while total > limit {
        total -= sizes[drop];
        drop += 1;
    }
    records.drain(..drop);
    records.iter().find(|d| d.key == key).map_or(0, |d| d.files.len())
}

fn base64(data: &str) -> bool {
    let bytes = data.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return false;
    }
    let end = bytes.iter().position(|b| *b == b'=').unwrap_or(bytes.len());
    bytes.len() - end <= 2
        && bytes[..end].iter().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/'))
        && bytes[end..].iter().all(|b| *b == b'=')
}

fn restore(mut files: Vec<File>) -> Vec<AttachmentUpload> {
    files.sort_by_key(|f| f.order);
    let mut last = None;
    files
        .into_iter()
        .filter_map(|f| {
            let duplicate = last == Some(f.order);
            last = Some(f.order);
            (!duplicate && !f.upload.mime.is_empty() && base64(&f.upload.data)).then_some(f.upload)
        })
        .collect()
}

fn lost(count: usize) -> Option<String> {
    (count > 0).then(|| format!("{count} attachment{} from your draft couldn't be kept", if count == 1 { "" } else { "s" }))
}

fn restored(json: &str, key: &str) -> (Vec<AttachmentUpload>, usize) {
    let value = serde_json::from_str::<serde_json::Value>(json).ok();
    let files = value.as_ref().filter(|v| v["key"].as_str() == Some(key)).and_then(|v| v["files"].as_array());
    let Some(files) = files else { return (Vec::new(), usize::from(!json.is_empty())) };
    let count = files.len();
    (restore(files.iter().filter_map(|f| serde_json::from_value(f.clone()).ok()).collect()), count)
}

async fn write(draft: Draft) -> Result<usize, JsValue> {
    let kept = std::rc::Rc::new(std::cell::Cell::new(0));
    let result = kept.clone();
    let policy = Closure::<dyn Fn(String) -> String>::new(move |json: String| {
        // A malformed record must not prevent other drafts from being saved.
        let values: Vec<String> = serde_json::from_str(&json).unwrap_or_default();
        let mut records = values.into_iter().filter_map(|v| serde_json::from_str(&v).ok()).collect();
        result.set(replace(&mut records, draft.clone(), LIMIT));
        serde_json::to_string(&records).expect("drafts serialize")
    });
    update(policy.as_ref().unchecked_ref()).await?;
    Ok(kept.get())
}

/// The same per-place key as text drafts, captured before asynchronous work
/// can run in a different page/realm. `None` is an explicitly unsaved form.
pub fn attachments(key: Option<&str>) -> (RwSignal<Vec<AttachmentUpload>>, RwSignal<Option<String>>, RwSignal<bool>) {
    let uploads = RwSignal::new(Vec::<AttachmentUpload>::new());
    let note = RwSignal::new(None);
    let ready = RwSignal::new(key.is_none());
    let Some(key) = key else { return (uploads, note, ready) };
    let key = super::placed_key(&format!("c6.draft.{key}"));
    let marker = format!("{key}.attachments");
    let expected = super::storage_get_global(&marker).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
    let baseline = StoredValue::new(Vec::<AttachmentUpload>::new());
    let revision = StoredValue::new(0_u64);
    let restore_key = key.clone();
    leptos::task::spawn_local(async move {
        let loaded = load(&restore_key).await;
        if uploads.is_disposed() {
            return;
        }
        match loaded {
            Ok(value) => {
                let (mut files, count) = restored(&value.as_string().unwrap_or_default(), &restore_key);
                note.set(lost(expected.max(count).saturating_sub(files.len())));
                baseline.set_value(files.clone());
                // Pasting while the database opens must not be overwritten.
                files.extend(uploads.get_untracked());
                uploads.set(files);
            }
            Err(_) => note.set(lost(expected).or_else(|| Some("Draft attachments couldn't be restored in this browser".into()))),
        }
        ready.set(true);
    });
    Effect::new(move || {
        if !ready.get() {
            return;
        }
        let files = uploads.get();
        if baseline.with_value(|previous| previous == &files) {
            return;
        }
        baseline.set_value(files.clone());
        revision.update_value(|r| *r += 1);
        let saving = revision.get_value();
        let count = files.len();
        // Write the count before IDB: if quota/security blocks it, the next
        // visit can still explain what went missing. No payload in localStorage.
        if count == 0 {
            super::storage_remove_global(&marker);
            note.set(None);
        } else {
            super::storage_set_global(&marker, &count.to_string());
        }
        let draft = record(key.clone(), files, super::now_ms());
        leptos::task::spawn_local(async move {
            let missing = match write(draft).await {
                Ok(kept) => count.saturating_sub(kept),
                Err(_) => count,
            };
            if missing > 0 && revision.try_get_value() == Some(saving) {
                note.try_set(lost(missing));
            }
        });
    });
    (uploads, note, ready)
}

pub(super) fn forget(key: &str) {
    let key = super::placed_key(&format!("c6.draft.{key}"));
    TEXT_SAVES.with_borrow(|saves| {
        if let Some(saves) = saves.get(&key) {
            for save in saves.iter().filter_map(Weak::upgrade) {
                save.borrow_mut().forget();
            }
        }
    });
    super::storage_remove_global(&key);
    super::storage_remove_global(&format!("{key}.attachments"));
    leptos::task::spawn_local(async move {
        let _ = write(record(key, Vec::new(), 0)).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_save_coalesces_edits_and_forgetting_prevents_teardown_resurrection() {
        let mut save = TextSave { last: "restored".into(), ..Default::default() };
        assert!(!save.change("restored"), "restoring isn't an edit or a write");
        assert!(save.pending.is_none());
        assert!(save.change("a"));
        assert!(save.change("ab"));
        assert!(!save.change("ab"), "unchanged text doesn't restart the timer");
        assert_eq!(save.pending.as_deref(), Some("ab"), "only the latest edit is flushed");
        save.forget();
        assert!(save.pending.is_none() && save.forgotten, "sending cancels pending persistence");
        assert!(!save.change("ab"), "teardown of an unchanged form must not resurrect a sent draft");
        assert!(save.forgotten);
        assert!(save.change("new draft"));
        assert!(!save.forgotten, "a subsequent actual edit can be saved");
        assert!(save.change(""));
        assert_eq!(save.pending.as_deref(), Some(""), "clearing removes rather than restores text");
    }

    fn file(name: &str) -> AttachmentUpload {
        AttachmentUpload { name: name.into(), mime: "image/png".into(), data: "aGVsbG8=".into() }
    }

    #[test]
    fn attachment_order_and_validation() {
        let mut d = record("a".into(), vec![file("first"), file("second"), file("bad"), file("duplicate")], 1);
        d.files[2].upload.data = "not base64".into();
        d.files[3].order = 0;
        d.files.swap(0, 1);
        let files = restore(d.files);
        assert_eq!(files.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), ["first", "second"]);
        assert!(base64(""));
        assert!(!base64("===="));
        assert!(!base64("a=== or bad"));
    }

    #[test]
    fn round_trip_keeps_metadata_and_restores_around_a_broken_file() {
        let d = record("c6.home.draft.session".into(), vec![file("a.png"), file("b.png")], 1);
        let json = serde_json::to_string(&d).unwrap();
        let (files, count) = restored(&json, &d.key);
        assert_eq!(count, 2);
        assert_eq!(files, vec![file("a.png"), file("b.png")]);
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        value["files"][0]["mime"] = serde_json::Value::Null;
        let (files, count) = restored(&value.to_string(), &d.key);
        assert_eq!(count, 2);
        assert_eq!(files, vec![file("b.png")]);
        assert!(restored(&json, "c6.other.draft.session").0.is_empty());
        assert_eq!(restored("", &d.key), (Vec::new(), 0));
        assert_eq!(restored("broken", &d.key), (Vec::new(), 1));
    }

    #[test]
    fn cap_evicts_oldest_drafts_and_counts_encoded_bytes() {
        let old = record("old".into(), vec![file("one")], 1);
        let new = record("new".into(), vec![file("two")], 2);
        let limit = serde_json::to_vec(&new).unwrap().len();
        let mut records = vec![old];
        assert_eq!(replace(&mut records, new.clone(), limit), 1);
        assert_eq!(records.iter().map(|d| d.key.as_str()).collect::<Vec<_>>(), ["new"]);
        assert_eq!(replace(&mut records, new, limit - 1), 0);
        assert!(records.is_empty());
    }

    #[test]
    fn updating_and_clearing_a_draft_does_not_duplicate_or_touch_others() {
        let mut records = vec![record("a".into(), vec![file("old")], 1), record("b".into(), vec![file("other")], 2)];
        replace(&mut records, record("a".into(), vec![file("updated"), file("last")], 3), LIMIT);
        assert_eq!(records.len(), 2);
        assert_eq!(records[1].files[0].upload.name, "updated");
        replace(&mut records, record("a".into(), vec![], 4), LIMIT);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].key, "b");
        assert_eq!(lost(1).as_deref(), Some("1 attachment from your draft couldn't be kept"));
        assert_eq!(lost(0), None);
    }

    #[test]
    fn cap_ties_are_deterministic_and_empty_saves_delete() {
        let mut records = vec![record("a".into(), vec![file("one")], 1)];
        let b = record("b".into(), vec![file("two")], 1);
        let limit = serde_json::to_vec(&b).unwrap().len();
        replace(&mut records, b, limit);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].key, "b");
        assert_eq!(replace(&mut records, record("b".into(), vec![], 2), 0), 0);
        assert!(records.is_empty());
    }
}
