# Slide decks — Design

carbon6 makes presentation decks the way Claude does: from a prompt to an outline you approve, a look you choose, then slides built and checked by eye until they look professional. It is an app (docs/os.md §5), configuration only, on two general capabilities.

## 1. Decisions

| Question | Choice | Why |
|---|---|---|
| Format | HTML master; PDF always; PPTX as an export later | Models write good HTML and CSS, Chromium renders it exactly, and the review loop works on it. PPTX keeps text and simple shapes editable, with complex visuals as images |
| Images | Charts, diagrams and icons drawn as SVG, and your own assets; generated images later, opt-in per deck; no stock | Data visuals make business decks; nothing needs a new account |
| Brand | Three themes ship as records; a brand kit becomes a fourth theme | Good-looking out of the box, configurable later |
| Workers | No installs for HTML and PDF (Chromium is already used by sessions); PPTX's Python packages are asked for once, as a decision, when that export comes | Downloads are yours to approve |

## 2. General capabilities (built first)

- **Tools show images to the model.** `read` on an image file returns the image, and the browser's `screenshot` returns what it captures. It can capture one element (`selector`, e.g. one slide), the whole page, or the window at a set size (`width`, `height`). Tool images are kept in the conversation (the latest 12) and shown in the transcript. Every provider gets them (Anthropic, OpenAI Responses, Chat Completions).
- **The browser prints PDFs:** `pdf {path}` uses the page's CSS `@page` size and includes backgrounds.

- **The decks-render kit** (`examples/packages/decks-render.json`, docs/packages.md): headless Chromium, poppler, python-pptx and open fonts in an image every worker builds for itself, run in a sandbox with no network. Its tools: `pdf`, `slides` (a PNG per slide), `slide` (one) and `pptx` (a picture per slide). The decks app requires it (version 1.1), so a worker needs only Docker or Podman; the app's skills use `kit_run` instead of the browser.

## 3. The decks app (`examples/apps/decks.json`)

- **Collections:**
  - `decks`: title, brief, audience, length, theme, outline, stage (`outlining → choosing → building → reviewing → delivered`), folder and files.
  - `themes`: name, mood, and CSS custom properties: colours, type, accents. Three ship: **Paper** (light, editorial serif headings), **Night** (dark, high contrast), **Signal** (clean light, one strong accent).
- **Skills:**
  - `deck-storyline`: one message per slide, an arc, titles that state the point.
  - `deck-design`: grid, type scale, spacing, colour use, one idea per slide, data-ink.
  - `deck-html`: the slide markup and base CSS (1280×720 sections, layouts, print CSS for the PDF).
  - `deck-visuals`: SVG charts and diagrams by hand, icons, images.
  - `deck-review`: the checklist the agent applies to each screenshot.
- **Templates:**
  - `deck`: the whole flow, from a brief:
    1. record the deck;
    2. write the outline, then ask you as a decision (approve, or say what to change);
    3. ask which theme;
    4. build;
    5. look at every slide (rendered by the kit) and fix it;
    6. print the PDF (and PowerPoint, when asked);
    7. publish the files;
    8. mark the deck delivered.

    The agent waits for each answer, which arrives as a message.
  - `deck-revise`: changes to a delivered deck, from your notes.
- **Views:** a board of decks by stage, and the themes.

## 4. Later

1. **PPTX export:** HTML slides map to editable shapes and text, with charts as images. It needs python-pptx on the worker, asked for once.
2. **Comments per page** on Outputs (decks, PDFs, reports), sent to the session as revision requests.
3. **Generated images** for title and section slides, through a connection that makes images; opt-in per deck.
4. **Brand kits:** from your logo, colours and fonts in the memex, the agent drafts a theme record for you to approve.
