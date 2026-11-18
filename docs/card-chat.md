# Card discussions

Status: built and verified (2026-11-18).

## What it does

**Discuss** opens a small conversation inside any decision card, in both the
session panel and Decisions. It explains the proposal and can revise its text.
The existing answer buttons alone settle the card. Answered cards keep a
read-only discussion and revision history.

## How it works

The head keeps one thread per decision, not a top-level session. It seeds each
turn with the latest card, its source context and a bounded digest around its
creation. The model uses `task:card` routing, falling back to the source
session's head-callable connection, then Ask's. Its editable instructions are
`decision.discuss` in Settings → Prompts.

The only tools read bounded source events (at most 200 per read) and revise the card. Task revisions
may change title, why, prompt, worktree and labels; other decisions may change
wording and explanatory `context.discussion`, not options or action context. Every revision saves the previous and next
text. The card shows **revised in discussion** with before/after details.
Actions always read the current card. A secondary **Ask the session that raised
it** opens an ordinary side chat with the unarchived proposer and its context.

The card's realm permissions protect every discussion route and model choice. Discussion never
answers or dismisses cards, starts work, or sends notifications. Closure is
checked again before saving model results. Per-card guards serialize revisions
with single and batch answers, so an action cannot execute an older snapshot
while a revision commits. No guard is held during model calls. A durable,
expiring lease admits one reply at a time; turns have at most eight model calls
and a two-minute limit. The model sees the latest card and last 32 discussion
messages; the complete history remains stored.
API connections use native tool calls. Claude Code's one-off head client uses
strict JSON requests for the same two tools, with the protocol instructions in
the same editable prompt; it never receives CLI filesystem tools.

## As built

- **Storage:** `0068_card_chat.sql` keeps messages, revision snapshots and reply
  leases beside the decision. No hidden or top-level session is needed.
- **Head:** `card_chat.rs`, shared `watchers::ask_message`/`digest`, existing
  model routing and side-chat creation. The editable `decision.discuss` prompt
  is in the prompts registry.
- **API:** GET/POST `/api/v1/decisions/{id}/discussion`, and POST
  `…/discussion/session`. Catalog ops are `decisions.discussion` (read),
  `decisions.discuss` and `decisions.discussion_session` (writes). The agent
  operation classifier explicitly excludes both writes from arbitrary agent
  calls.
- **UI:** `decisions.rs` renders Discuss, history, changed-field comparisons and
  the source-chat choice in the shared card renderer. Revision details show
  short fields as old → new and long text as a word-level LCS diff (plain
  before/after paragraphs over 4,000 characters or for whitespace-only edits).
  History is newest first;
  older revisions and the outer marker start collapsed. Revisions refresh the
  proposal without collapsing its thread. Failed turns reload durable changes
  while retaining their error.

## Tests

Five mock-model worker integration tests check seeding and editable instructions,
native tool calls, latest prompt/title on Start, old text retained, continuation,
non-task context protection, unavailable settlement tools, closure (including
a gated late revision), cross-realm denial and source side chats/archival.
Head tests cover routing/fallback permissions, durable snapshots, stale and
closed writes, source pagination bounds, lease contention, malformed contexts
and shared action guards. A fake Claude executable covers the complete JSON
tool/reply turn without invoking an installed CLI. HTTP/catalog/classification
tests cover new routes.
UI tests cover shared controls, rendering, changed-field selection, word diffs
and their long-text fallback, revision ordering, closure and failed-turn
recovery. See `docs/testing.md` for the condition-based test rules.

Verification: workspace clippy and wasm UI clippy passed with warnings denied;
`make test` passed 1,440 tests with 2 skipped. The Linux Docker production UI
build passed at 10,975,674 raw bytes and 2,735,561 Brotli bytes, within the
existing budgets. Touched-file rustfmt checks, test lint and diff checks passed.
A scratch-head visual check used a Docker release UI build and a local mock
model, capturing Chrome CDP screenshots at 1,440px desktop and 390px phone widths.
The Discuss control, question/reply and read-only answered thread rendered
correctly. A follow-up replaced raw revision JSON with readable per-field
comparisons and rechecked the revision view at both widths.
The follow-up UI tests passed all 350 tests; native all-target and wasm UI
clippy passed with warnings denied.

## Not done

Reading arbitrary worker files from the discussion is intentionally omitted;
bounded source events and the full card prompt are available without adding a
new remote filesystem permission surface.
