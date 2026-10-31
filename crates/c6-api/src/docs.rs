//! Documents (docs/docs.md): docs and decks that people and agents write
//! together. A document is tabs of blocks, each with a stable id; edits are
//! batches of small guarded operations; every batch is a version.
//! Markdown is the exchange format, with chips and pending sections as
//! `<?c6 block k?>` tokens and their values in a `blocks` map beside it.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::UnixMs;

/// The kinds of document.
pub const KINDS: [&str; 2] = ["doc", "deck"];

/// A slide's layouts, each with its regions in order (docs/docs.md §5).
/// A heading at the top of a slide goes to its `title` region, the rest
/// to the first other region unless `<!-- region: x -->` says otherwise.
pub const LAYOUTS: &[(&str, &[&str])] = &[
    ("title", &["title", "subtitle"]),
    ("section", &["title", "subtitle"]),
    ("body", &["title", "body"]),
    ("two-columns", &["title", "left", "right"]),
    ("big-number", &["title", "number", "caption"]),
    ("quote", &["quote", "attribution"]),
    ("image", &["title", "image", "caption"]),
    ("chart", &["title", "chart", "caption"]),
    ("diagram", &["title", "diagram", "caption"]),
    ("html", &["html"]),
];

/// A layout's regions (unknown layouts are `body`'s).
pub fn regions(layout: &str) -> &'static [&'static str] {
    LAYOUTS.iter().find(|(l, _)| *l == layout).map(|(_, r)| *r).unwrap_or(&["title", "body"])
}

/// Limits (docs/docs.md §9).
pub const MAX_TAB_TEXT: usize = 2 * 1024 * 1024;
pub const MAX_TABS: usize = 20;
pub const MAX_SLIDES: usize = 200;
pub const MAX_OPS: usize = 500;

/// A document as listed: no content.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocSummary {
    pub id: String,
    pub realm: String,
    /// doc or deck.
    pub kind: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub theme: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub size: String,
    pub labels: Vec<String>,
    /// The latest version.
    pub rev: i64,
    /// you, session:<id>, assistant, or token:<name>.
    pub created_by: String,
    pub created_at: UnixMs,
    pub updated_by: String,
    pub updated_at: UnixMs,
    /// Blocks still to be written (pending), and who is writing them.
    #[serde(default)]
    pub pending: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub writers: Vec<String>,
}

/// A document with all its content, block by block.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Doc {
    #[serde(flatten)]
    pub summary: DocSummary,
    pub tabs: Vec<DocTab>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocTab {
    pub id: String,
    pub title: String,
    /// In order; a deck's are its slides (and pending slides).
    pub blocks: Vec<DocBlock>,
}

/// A block. Text blocks keep their Markdown; a slide keeps its layout,
/// notes and hidden flag, and its regions' blocks as `children`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocBlock {
    /// `<writer>.<clock>`, issued when it was written and never reused.
    pub id: String,
    /// heading, paragraph, item, table, code, quote, html, rule, pending,
    /// embed, or slide.
    pub kind: String,
    /// Its Markdown (inline chips as `<?c6 block k?>`).
    #[serde(default)]
    pub text: String,
    /// pending: {intent, by}; slide: {layout, notes, hidden}; embed: its
    /// value; chips: {"chips": {k: value}}.
    #[serde(default, skip_serializing_if = "is_empty_object")]
    pub attrs: Value,
    /// For a block on a slide: its region.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub region: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<DocBlock>,
    /// What a guard compares: changes whenever the block (or a slide's
    /// content) does.
    pub hash: String,
    /// Who last wrote it, and in which version.
    pub by: String,
    pub rev: i64,
}

fn is_empty_object(v: &Value) -> bool {
    v.is_null() || v.as_object().is_some_and(Map::is_empty)
}

/// `POST /api/v1/docs`: a new document (its birth: a title, then Markdown,
/// typically with a pending block per section or slide).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocNew {
    /// doc (default) or deck.
    #[serde(default)]
    pub kind: String,
    pub title: String,
    /// The first tab's content. A deck's slides are separated by `---`.
    #[serde(default)]
    pub markdown: String,
    /// The values of the `<?c6 block k?>` tokens in it: {k: {type:
    /// pending, intent}} and chips.
    #[serde(default)]
    pub blocks: Map<String, Value>,
    /// A deck's theme and size (16:9, the default, or 4:3).
    #[serde(default)]
    pub theme: String,
    #[serde(default)]
    pub size: String,
    #[serde(default)]
    pub labels: Vec<String>,
    /// The first tab's title (default: the document's).
    #[serde(default)]
    pub tab: String,
}

/// `PATCH /api/v1/docs/{id}`: its title and labels.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocPatch {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub labels: Option<Vec<String>>,
}

/// `POST /api/v1/docs/{id}/ops`: a batch, applied atomically or not at all.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocBatch {
    pub ops: Vec<DocOp>,
    /// The comment thread this edit answers (docs/docs.md §8).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub answering: String,
}

/// One operation. Replacing or deleting a whole block needs `ifRev` (the
/// version you read) and `ifHash` (the block's hash then): if it changed
/// since, the batch is refused (`guard_mismatch`) and nothing lands.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum DocOp {
    /// Markdown (one or more blocks; a deck's: slides) before or after a
    /// block, at the end of a heading's section, at the end of a slide's
    /// region, or at the end of a tab (the first, unless named).
    Insert {
        markdown: String,
        #[serde(default)]
        blocks: Map<String, Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        before: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after: Option<String>,
        /// At the end of this heading's section.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        section: Option<String>,
        /// Into this slide, at the end of `region`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        slide: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        region: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tab: Option<String>,
    },
    /// A block by new Markdown (guarded; it keeps its id, and further
    /// blocks get new ones), a run of words in it (`find`, the `nth` when
    /// they repeat, 1-based), a table cell, or a slide's layout, notes or
    /// hidden flag.
    Replace {
        /// The block (or slide); `block` is the same.
        #[serde(alias = "block")]
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        markdown: Option<String>,
        #[serde(default)]
        blocks: Map<String, Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        find: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none", rename = "with")]
        with_text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        nth: Option<u32>,
        /// [row, column], 0-based, the header row being 0.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cell: Option<[usize; 2]>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        layout: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        notes: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hidden: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none", rename = "ifRev", alias = "if_rev")]
        if_rev: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none", rename = "ifHash", alias = "if_hash")]
        if_hash: Option<String>,
    },
    /// A block or slide (guarded).
    Delete {
        /// The block (or slide); `block` is the same.
        #[serde(alias = "block")]
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none", rename = "ifRev", alias = "if_rev")]
        if_rev: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none", rename = "ifHash", alias = "if_hash")]
        if_hash: Option<String>,
    },
    /// A block or slide, before or after another, or to the end of a tab.
    Move {
        /// The block (or slide); `block` is the same.
        #[serde(alias = "block")]
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        before: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tab: Option<String>,
    },
    /// A table's rows and columns: add_row, delete_row, add_column or
    /// delete_column at a 0-based index (adding: before it; past the end:
    /// at the end).
    Table {
        /// The block (or slide); `block` is the same.
        #[serde(alias = "block")]
        id: String,
        action: String,
        at: usize,
        #[serde(default)]
        cells: Vec<String>,
    },
    /// The document's title, a deck's theme or size, a tab's title, the
    /// order of tabs, or a chip's value (`block` and `chip`).
    Set {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        theme: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tab: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tab_title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tabs: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        block: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        chip: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<Value>,
    },
    /// A new tab (docs only), after another or at the end.
    AddTab {
        title: String,
        #[serde(default)]
        markdown: String,
        #[serde(default)]
        blocks: Map<String, Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after: Option<String>,
    },
    /// A tab and what's in it.
    DeleteTab { tab: String },
}

/// What a batch did.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocApplied {
    pub id: String,
    /// The version it made (unchanged when it changed nothing).
    pub rev: i64,
    /// The ids its Markdown's blocks got, in order, per operation.
    #[serde(default)]
    pub ids: Vec<Vec<String>>,
    /// The ids `<?c6 block k?>` keys became: {k: id}.
    #[serde(default)]
    pub keys: Map<String, Value>,
    /// The blocks it changed and their new hashes (for the next guard).
    #[serde(default)]
    pub hashes: Map<String, Value>,
}

/// `GET /api/v1/docs/{id}/read`: what to read (small reads, docs/docs.md §7).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocRead {
    /// The realm (default: yours, else home).
    #[serde(default)]
    pub realm: Option<String>,
    /// outline (default), block, slide, search, since, markdown or full.
    #[serde(default)]
    pub view: String,
    /// The block or slide (block, slide).
    #[serde(default)]
    pub id: Option<String>,
    /// What to find (search).
    #[serde(default)]
    pub q: Option<String>,
    /// The version read before (since).
    #[serde(default)]
    pub rev: Option<i64>,
    /// A tab (outline, markdown), else all.
    #[serde(default)]
    pub tab: Option<String>,
    /// The document as it was at this version (outline, block, markdown, full).
    #[serde(default)]
    pub at: Option<i64>,
}

/// A version in a document's history.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocVersion {
    pub rev: i64,
    pub by: String,
    /// The session turn that made it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<i64>,
    pub at: UnixMs,
    /// Operations in the batch.
    pub ops: u32,
    /// The blocks it added, changed and removed.
    pub added: Vec<String>,
    pub changed: Vec<String>,
    pub removed: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub answering: String,
    /// A line saying what it did ("created", "renamed", "3 blocks changed").
    pub summary: String,
}

/// `POST /api/v1/docs/{id}/restore`: a version, or one block or slide of
/// it, back as a new change.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocRestore {
    pub rev: i64,
    #[serde(default)]
    pub block: Option<String>,
}

/// `GET /api/v1/docs`: which documents.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocQuery {
    /// The realm (default: yours, else home).
    #[serde(default)]
    pub realm: Option<String>,
    /// doc or deck.
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    /// Who made or last changed it (you, session:<id>, …).
    #[serde(default)]
    pub by: Option<String>,
    /// Words in the title.
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
}

/// A change to a document, over the live feed: what to read again.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocChange {
    pub id: String,
    pub realm: String,
    pub rev: i64,
    pub by: String,
    /// created, changed or deleted.
    pub what: String,
}

/// How documents behave in a realm (`/api/v1/settings/docs`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocsSettings {
    /// Which documents a session may write without asking: `work` (those
    /// it made and its work's; others once you allow each), `realm` (any in
    /// the realm) or `own` (only those it made; never others).
    #[serde(default = "work")]
    pub write: String,
}

fn work() -> String {
    "work".into()
}

impl Default for DocsSettings {
    fn default() -> Self {
        Self { write: work() }
    }
}

/// An export of a document, kept as a file with the version it was made
/// from (docs/docs.md §10).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocExport {
    pub id: String,
    pub doc: String,
    /// md, html, pdf, docx, odt, pptx, odp or png (a zip of pictures).
    pub format: String,
    /// The document's version it was made from.
    pub rev: i64,
    /// Its file name.
    pub name: String,
    pub size: u64,
    pub by: String,
    pub at: UnixMs,
    /// Where to download it.
    #[serde(default)]
    pub url: String,
}

/// `POST /api/v1/docs/{id}/export`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocExportIn {
    /// md, html, pdf, docx or odt for a doc; md, html, pdf, pptx, odp or
    /// png for a deck.
    pub format: String,
}

/// A document's Google file (docs/docs.md §10): where it was exported to
/// last, as a Google Doc or Google Slides.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GoogleFile {
    /// Its id in Google Drive.
    pub id: String,
    /// Where it opens in Google.
    pub url: String,
    /// The Google account it is in.
    pub account: String,
    /// The document's version last exported to it.
    pub rev: i64,
    /// When Google says it was last changed, as of that export (a later
    /// time means it was edited in Google since).
    #[serde(default)]
    pub modified: String,
    pub by: String,
    pub at: UnixMs,
}

/// `GET /api/v1/docs/{id}/google`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocGoogle {
    /// The realm's Google accounts for documents (Settings → Docs).
    pub accounts: Vec<String>,
    /// Its Google file, once exported.
    #[serde(default)]
    pub file: Option<GoogleFile>,
    /// Google Docs or Google Slides.
    pub app: String,
}

/// `POST /api/v1/docs/{id}/google`: to Google Docs (a doc) or Google
/// Slides (a deck). By default its Google file is updated (a new revision
/// in Google), or made the first time.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GoogleExportIn {
    /// Make a new Google file, not update the one it has.
    #[serde(default)]
    pub new: bool,
    /// Update even though the Google file changed in Google since the last
    /// export (those changes are replaced).
    #[serde(default)]
    pub anyway: bool,
    /// Which account a new file goes in (default: the one before, else the
    /// first).
    #[serde(default)]
    pub account: Option<String>,
}

/// `POST /api/v1/docs/google/connect`: a Google sign-in for documents.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GoogleConnect {
    /// The page to come back to (default: Settings).
    #[serde(default)]
    pub back: String,
}

/// `POST /api/v1/docs/import`: a file becomes a document.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocImport {
    /// The file's name (its extension says what it is: .docx, .odt, .pptx,
    /// .html, .md).
    pub name: String,
    /// The file, in base64.
    pub content: String,
    /// doc or deck (default: deck for PowerPoint, else doc).
    #[serde(default)]
    pub kind: String,
    /// Its title (default: the file's name).
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub labels: Vec<String>,
}

/// What a comment thread is about (docs/docs.md §8): quoted words in a
/// block, a block or slide, or a whole tab.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Anchor {
    /// The block or slide (none: the tab).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub block: String,
    /// A few words of it, quoted (none: the whole block).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub quote: String,
    /// The tab (default: the first).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tab: String,
}

/// A comment in a thread.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocComment {
    pub id: String,
    /// you, session:<id>, assistant, or token:<name>.
    pub by: String,
    pub text: String,
    pub at: UnixMs,
}

/// A comment thread on a document.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocThread {
    pub id: String,
    pub doc: String,
    pub anchor: Anchor,
    /// open or resolved.
    pub state: String,
    /// What it was anchored to is gone (the block deleted, the words
    /// changed): it stays, showing what it had.
    #[serde(default)]
    pub detached: bool,
    /// What it's about, as it read when the thread began.
    #[serde(default)]
    pub about: String,
    /// It was sent to an agent, and the session it went to.
    #[serde(default)]
    pub to_agent: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent: String,
    pub comments: Vec<DocComment>,
    /// The versions made answering it (edits with `answering`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edits: Vec<i64>,
    /// An agent answered and you haven't replied or resolved it yet.
    #[serde(default)]
    pub needs_you: bool,
    pub created_by: String,
    pub created_at: UnixMs,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<UnixMs>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub resolved_by: String,
}

/// `POST /api/v1/docs/{id}/threads`: a new thread with its first comment.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ThreadNew {
    #[serde(default)]
    pub anchor: Anchor,
    pub text: String,
    /// `agent`: send it to an agent (the session that last wrote that part,
    /// if it's idle, else a new one in the document's work or project).
    #[serde(default)]
    pub to: String,
}

/// `POST /api/v1/docs/{id}/threads/{thread}/comments`: a reply.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CommentNew {
    pub text: String,
    /// `agent`: send the thread (again) to an agent.
    #[serde(default)]
    pub to: String,
}
