# Read-only file viewers

Files opened from the browser or a transcript path use the same renderer as
inline previews. Renderers take content, a safe byte URL, a name, a source
location and a compact/full mode; they do not own upload or attachment delivery.
An attachment can supply those same inputs later. Unknown formats keep a card
with type, size, Download and Copy path. Access is still the session worker's
existing filesystem access, not browser-local files.

## Group 1

* Markdown (`.md`, `.markdown`) uses the existing sanitising Markdown renderer,
  with a Source switch. Inline mentions show a heading/first-lines card.
* Code and text show numbered, highlighted source. `path:line[:column]` keeps
  the location in the link and opens at a highlighted line. Pointer hover,
  keyboard focus or long press can show nearby source without navigating.
  Fenced code uses the same highlighter. The highlighter is a separately
  shipped, on-demand JS asset, not a Rust grammar database in the wasm.
* Images (PNG, JPEG, GIF, WebP, AVIF, SVG) fit the pane, show natural size and
  support zoom and pan. Inline mentions show clickable thumbnails. SVG bytes
  are only image resources, never inserted into the document as markup.
* PDFs use a sandboxed native PDF iframe backed by downloaded bytes, with an
  Open/Download fallback for browsers without an embedded viewer. Inline is a
  filename/Open card; page count is omitted unless available without parsing.
* CSV/TSV are parsed in Rust, including quoted fields and embedded newlines.
  Tables have a sticky header, sorting, filtering and horizontal scrolling.
  Full mode initially shows 10,000 rows and Load more; compact mode shows 20.
  CSV fences use this renderer too.
* JSON has a collapsible tree, Copy path (JSON Pointer), and Pretty/Raw switch.
  JSONL/NDJSON has one expandable row per record and bounded loading. Large JSON
  fences (over 30 lines) start collapsed; small JSON stays highlighted code.

## Group 2

* Unified diffs (`.diff`, `.patch`, diff fences and whole unified-diff replies)
  have collapsible file headers, word changes using the discussion-card LCS,
  Unified/Split full mode, and compact expansion after 40 lines.
* Logs (`.log` and recognised build/test output filenames) safely interpret
  ANSI SGR as spans, ignoring control sequences and hyperlinks. Error/warning
  lines are marked, with first/previous/next error controls. Large logs start
  at the tail with Load earlier.
* Mermaid fences and `.mmd`/`.mermaid` files load Mermaid only when visible.
  Use strict security, disable external content and display the generated SVG
  as an image, never model-provided markup. Source is always available; failed
  renders show the error and source rather than a blank diagram.

## Delivery, safety and limits

The head download route keeps its fixed 40 MiB worker-RPC cap, no-store,
attachment filename and nosniff. Its Content-Type is an allowlisted image/PDF
or text type; HTML and unknown formats remain octet-stream. A restrictive CSP
on byte responses prevents active content/network access if opened directly.
The viewer fetches bounded bytes for complete text rather than silently using
the fs/read preview's 2 MiB truncation. Blob URLs have explicit allowlisted MIME
types and are revoked when their owner disappears. No content scripts run;
source HTML is escaped; Mermaid and SVG never enter innerHTML. Markdown file
images must not initiate network requests. Links remain user-initiated.

Views cap rendered rows/lines/nodes, then expose Load more within the byte cap.
No library is fetched from a CDN at runtime. JS vendors are version-pinned,
licensed, served from the same origin and loaded only by their renderer.
`scripts/ui-size.sh` measures wasm and its Brotli sibling, not lazy JS; report
both wasm size and separately shipped vendor assets so lazy loading is not a
hidden base-bundle increase.

All controls are keyboard accessible and touch sized. At 390px, tables/source
scroll inside their pane rather than widening the page. Colour tokens support
light and dark modes; status also has text, not colour alone.

## Verification

Native UI tests cover source locations (including times, ports and IPv6
negatives), CSV/TSV, JSON pointers/JSONL, diff parsing/word changes, ANSI/log
severity, renderer dispatch and the SVG image-only invariant. Download tests
cover allowlisted MIME, nosniff and hostile filenames. Targeted tests run while
developing; after rebase run the full suite once. Check workspace and wasm
clippy, Docker release UI and size budget, then inspect a scratch head at
18190/18191 in desktop/mobile light/dark with mixed-format mock replies.
Never use the live head or its workers for verification.
