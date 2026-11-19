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
* PDFs were first tested in a sandboxed native iframe backed by downloaded
  bytes. Chrome blocks that embed even when `pdfViewerEnabled` is true, so full
  mode instead lazy-loads self-hosted PDF.js and paints bounded pages to canvas.
  It never enables PDF scripting, annotation or link layers, or document URLs.
  Open/Download remains available; inline is a filename/Open card (no engine
  download merely to count its pages).
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
the fs/read preview's 2 MiB truncation. The bridge runs up to two downloads at
a time per page; queued work for disconnected viewers is dropped. This does not
change the backend's 40 MiB cap per download. Blob URLs have explicit allowlisted MIME
types and are revoked when their owner disappears. No content scripts run;
source HTML is escaped; Mermaid and SVG never enter innerHTML. Markdown file
images must not initiate network requests. Links remain user-initiated.

Views cap rendered rows/lines/nodes, then expose Load more within the byte cap.
There are also hard display limits (2 MiB of source/table text, 256 table
columns, 100,000 displayed cells, 8 KiB per displayed line/value and 64 JSON
levels). A display-cap notice points to the complete Download; large text is
streamed with a 40 MiB ceiling, not buffered before checking its size.
No library is fetched from a CDN at runtime. JS vendors are version-pinned,
licensed, served from the same origin and loaded only by their renderer.
`scripts/ui-size.sh` measures wasm and its Brotli sibling, not lazy JS; report
both wasm size and separately shipped vendor assets so lazy loading is not a
hidden base-bundle increase.

The shipped lazy vendors are highlight.js 11.12.0 (common grammars, BSD-3-Clause,
129,254 bytes) and Mermaid 11.17.2 (MIT, 3,572,661 bytes). Their licenses live
beside the pinned builds in `crates/c6-ui/vendor`. The small bridge loads at
startup; the vendor code is fetched only for visible highlighted code/diagrams.
Mermaid also runs in an opaque script-only iframe with a deny-network CSP;
its SVG passes an element/attribute/resource allowlist before becoming an image.
The 2 MiB inline byte cap is separate from the full viewer's 40 MiB cap; an
oversized preview keeps its Open/Download link rather than truncating silently.
PDF.js 6.5.438 is also lazy (Apache-2.0; 474,284-byte module and
1,271,915-byte worker). It initially paints five pages, then loads five more
per click, with hard limits of 2 million pixels per page, 18 million pixels per
preview and 60 pages. Canvas only: annotations, XFA, scripting, eval, document
URLs, external font/CMap fetches and worker network fetches are disabled.

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
