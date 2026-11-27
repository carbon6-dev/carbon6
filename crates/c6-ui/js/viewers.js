/* Lazy enhancements for the HTML emitted by the shared Rust file renderer. */
(() => {
  "use strict";
  const assets = new URL("../vendor/", document.currentScript?.src || location.href);
  const limit = 40 * 1024 * 1024;
  const previewLimit = 2 * 1024 * 1024;
  const images = { png: "image/png", jpg: "image/jpeg", jpeg: "image/jpeg", gif: "image/gif", webp: "image/webp", avif: "image/avif", svg: "image/svg+xml" };
  const ext = path => path.split(".").pop().toLowerCase();
  const media = path => ext(path) === "pdf" ? "application/pdf" : Object.hasOwn(images, ext(path)) ? images[ext(path)] : "";
  const mediaType = (path, mime) => media(path) || ([...Object.values(images), "application/pdf"].includes(mime) ? mime : "");
  const candidates = ".fv, pre > code[class*='language-'], .c6-file-link[data-fv-path][data-fv-url]";
  const states = new WeakMap();
  const own = (root, sel) => Array.from(root.querySelectorAll(sel)).find(el => el.closest(".fv") === root);
  let renderer;
  let highlighting;

  window.c6Viewers = {
    init(callback) {
      renderer = callback;
      document.querySelectorAll(".c6-file-link[data-fv-path][data-fv-url]").forEach(scanNode);
    },
    render(input) { return renderer ? renderer(JSON.stringify(input)) : ""; },
    // Explicit user-requested copy may opt out of the preview read ceiling;
    // read() still enforces same-origin worker downloads and cancellation.
    async text(url, owner, max = limit) {
      if (owner && !owner.isConnected) throw new DOMException("Viewer disconnected", "AbortError");
      return asText(await read(url, max, owner && stateFor(owner).abort.signal));
    },
  };

  function downloadURL(value) {
    if (!value) throw new Error("No download URL available");
    const url = new URL(value, location.href);
    if (url.origin !== location.origin || !/^https?:$/.test(url.protocol) || url.username || url.password || url.hash ||
        !/^\/api\/v1\/workers\/[a-z\d-]+\/fs\/download$/i.test(url.pathname) || !url.searchParams.get("path")) {
      throw new Error("Invalid file download URL");
    }
    return url;
  }

  const waitingReads = new Set();
  let activeReads = 0;

  function releaseReadSlot() {
    activeReads--;
    waitingReads.values().next().value?.();
  }

  function acquireReadSlot(signal) {
    return new Promise((resolve, reject) => {
      signal?.throwIfAborted();
      const abort = () => { waitingReads.delete(start); reject(signal.reason); };
      const start = () => {
        waitingReads.delete(start);
        signal?.removeEventListener("abort", abort);
        activeReads++;
        resolve(releaseReadSlot);
      };
      if (activeReads < 2) start();
      else {
        waitingReads.add(start);
        signal?.addEventListener("abort", abort, { once: true });
      }
    });
  }

  async function read(url, max, signal) {
    const target = downloadURL(url);
    const release = await acquireReadSlot(signal);
    try {
      signal?.throwIfAborted();
      return await readBytes(target, max, signal);
    } finally {
      release();
    }
  }

  async function readBytes(url, max, signal) {
    const response = await fetch(url, { signal, redirect: "error", credentials: "same-origin" });
    if (!response.ok) {
      await response.body?.cancel();
      throw new Error(`Download unavailable (${response.status})`);
    }
    if (Number(response.headers.get("Content-Length")) > max) {
      await response.body?.cancel();
      throw new Error(`File exceeds ${max / 1024 / 1024} MiB`);
    }
    const reader = response.body?.getReader();
    if (!reader) throw new Error("File streaming unavailable");
    const parts = [];
    let total = 0;
    try {
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        total += value.byteLength;
        if (total > max) {
          await reader.cancel();
          throw new Error(`File exceeds ${max / 1024 / 1024} MiB`);
        }
        parts.push(value);
      }
    } finally {
      reader.releaseLock();
    }
    return parts;
  }

  function asText(parts) {
    const decoder = new TextDecoder("utf-8", { fatal: true });
    const text = parts.map(part => decoder.decode(part, { stream: true })).join("") + decoder.decode();
    if (text.includes("\0")) throw new Error("Not a text file");
    return text;
  }

  function blobURL(state, parts, type) {
    const url = URL.createObjectURL(new Blob(parts, { type }));
    state.urls.add(url);
    return url;
  }

  function highlighter() {
    if (!highlighting) highlighting = new Promise((resolve, reject) => {
      const script = document.createElement("script");
      script.src = new URL("highlight-11.12.0.min.js", assets).href;
      script.onload = () => window.hljs ? resolve(window.hljs) : reject(new Error("Highlighter unavailable"));
      script.onerror = () => reject(new Error("Highlighter unavailable"));
      document.head.append(script);
    });
    return highlighting;
  }

  function language(code) {
    const lang = [...code.classList].find(cls => cls.startsWith("language-"))?.slice(9);
    return lang && !["none", "plaintext", "text", "nohighlight"].includes(lang) && lang;
  }

  async function highlightFence(code) {
    const lang = language(code);
    if (!lang || code.textContent.length > 100000) return;
    const hl = await highlighter();
    if (!code.isConnected || !hl.getLanguage(lang)) return;
    code.innerHTML = hl.highlight(code.textContent, { language: lang, ignoreIllegals: true }).value;
    code.classList.add("hljs");
  }

  // One pass per contiguous set of numbered rows, preserving multiline tokens
  // without ever treating the source itself as markup. The inert template is
  // populated solely from highlight.js' escaped, generated token spans.
  async function highlightRows(root) {
    const rows = [...root.querySelectorAll(".fv-code code")].filter(code => code.closest(".fv") === root);
    const lang = rows.length && language(rows[0]);
    if (!lang || rows.length > 10000) return;
    const text = rows.map(code => code.textContent).join("\n");
    if (text.length > 250000 || rows.some(code => language(code) !== lang || code.textContent.includes("\n"))) return;
    const hl = await highlighter();
    if (!root.isConnected || !hl.getLanguage(lang) || rows.some(code => !code.isConnected)) return;
    const tokens = document.createElement("template");
    tokens.innerHTML = hl.highlight(text, { language: lang, ignoreIllegals: true }).value;
    const fragments = [document.createDocumentFragment()];
    const stack = [];
    let parent = fragments[0];
    function walk(node) {
      if (node.nodeType === Node.TEXT_NODE) {
        node.data.split("\n").forEach((piece, i) => {
          if (i) {
            parent = document.createDocumentFragment();
            fragments.push(parent);
            for (const entry of stack) {
              entry.parent = parent;
              const span = entry.node.cloneNode(false);
              parent.append(span);
              parent = span;
            }
          }
          if (piece) parent.append(piece);
        });
      } else if (node instanceof HTMLSpanElement) {
        stack.push({ node, parent });
        const span = node.cloneNode(false);
        parent.append(span);
        parent = span;
        node.childNodes.forEach(walk);
        parent = stack.pop().parent;
      }
    }
    tokens.content.childNodes.forEach(walk);
    if (fragments.length !== rows.length) return;
    rows.forEach((code, i) => { code.replaceChildren(fragments[i]); code.classList.add("hljs"); });
  }

  function sourceView(root, show) {
    const source = own(root, ".fv-source-view");
    const rendered = own(root, ".fv-rendered");
    if (!source || !rendered) return;
    source.hidden = !show;
    rendered.hidden = show;
    for (const button of root.querySelectorAll('[data-fv-action="source"]')) {
      if (button.closest(".fv") !== root) continue;
      button.setAttribute("aria-pressed", String(button.dataset.fvView === "pretty" ? !show : show));
    }
    if (show) centerLine(root);
  }

  function centerLine(root) {
    if (root.dataset.mode !== "full") return;
    const line = own(root, ".fv-selected");
    const box = line?.closest(".fv-code");
    if (!box?.clientHeight) return;
    box.scrollTop += line.getBoundingClientRect().top - box.getBoundingClientRect().top - (box.clientHeight - line.offsetHeight) / 2;
  }

  function error(root, message) {
    const rendered = own(root, ".fv-rendered") || root;
    let status = own(root, ".fv-js-error");
    if (!status) {
      status = document.createElement("p");
      status.className = "fv-js-error";
      status.setAttribute("role", "status");
      if (rendered === root) root.append(status);
      else rendered.before(status);
    }
    status.textContent = message;
  }

  // Only local SVG fragment references and non-resource CSS functions survive.
  const localRef = /^#[\w.:-]+$/;
  const svgNS = "http://www.w3.org/2000/svg";
  const svgTags = new Set(("svg g defs title desc path rect circle ellipse line polyline polygon text tspan textPath marker " +
    "clipPath mask pattern linearGradient radialGradient stop symbol use style filter feDropShadow feGaussianBlur " +
    "feOffset feFlood feComposite feBlend feMerge feMergeNode feColorMatrix feComponentTransfer feFuncR feFuncG feFuncB feFuncA").split(" "));
  const cssFunctions = new Set(("matrix matrix3d translate translatex translatey translatez translate3d scale scalex scaley scalez scale3d " +
    "rotate rotatex rotatey rotatez rotate3d skew skewx skewy rgb rgba hsl hsla hwb lab lch oklab oklch color calc min max clamp").split(" "));
  function localValue(value) {
    const compact = value.replace(/\/\*[\s\S]*?\*\//g, "");
    if (/[\\@\u0000-\u0008]/.test(compact)) return false;
    const withoutUrls = compact.replace(/\burl\(\s*(["']?)(#[\w.:-]+)\1\s*\)/gi, "");
    for (const [, fn] of withoutUrls.matchAll(/([\w-]+)\(/g)) if (!cssFunctions.has(fn.toLowerCase())) return false;
    return true;
  }

  function filterStyle(style) {
    return Array.from(style).filter(name => !name.startsWith("--") && localValue(style.getPropertyValue(name)))
      .map(name => `${name}:${style.getPropertyValue(name)}${style.getPropertyPriority(name) ? " !important" : ""};`).join("");
  }

  function safeSVG(svg) {
    if (!svg || /<!DOCTYPE|<!ENTITY|<\?/i.test(svg.replace(/^\s*<\?xml\s[^?]*\?>/i, ""))) throw new Error("Unsupported SVG declaration");
    const tree = new DOMParser().parseFromString(svg, "image/svg+xml");
    const root = tree.documentElement;
    if (root.localName !== "svg" || root.namespaceURI !== svgNS || tree.querySelector("parsererror")) throw new Error("Invalid SVG image");
    for (const el of [root, ...root.querySelectorAll("*")]) {
      if (el.namespaceURI !== svgNS || !svgTags.has(el.localName)) throw new Error(`Unsupported SVG element: ${el.localName}`);
      if (el.localName === "style") {
        // Constructed sheets are never added to a document; @import is ignored
        // by replaceSync. Copy normal style rules with safe declarations only.
        const sheet = new CSSStyleSheet();
        sheet.replaceSync(el.textContent);
        el.textContent = [...sheet.cssRules].filter(rule => rule.type === CSSRule.STYLE_RULE)
          .map(rule => `${rule.selectorText}{${filterStyle(rule.style)}}`).join("\n");
      }
      for (const attr of [...el.attributes]) {
        const name = attr.localName.toLowerCase();
        if (name.startsWith("on") || name === "base" || name === "src" ||
            (name === "href" && !localRef.test(attr.value))) throw new Error(`Unsupported SVG attribute: ${attr.name}`);
        if (name === "style") {
          const detached = document.createElement("span");
          detached.style.cssText = attr.value;
          el.setAttribute("style", filterStyle(detached.style));
        } else if (!localValue(attr.value) && /url\s*\(|\\/i.test(attr.value)) {
          throw new Error(`Unsupported SVG reference: ${attr.name}`);
        }
      }
    }
    return new XMLSerializer().serializeToString(root);
  }

  // Mermaid itself builds temporary SVG DOM. Run that work in an opaque,
  // script-only frame with network denied, not in the application DOM.
  function frameProgram() {
    const send = data => parent.postMessage(data, "*");
    const engine = globalThis.mermaid;
    if (!engine) { send({ failure: "Diagram engine unavailable" }); return; }
    let serial = 0;
    addEventListener("message", async event => {
      if (event.source !== parent || typeof event.data?.source !== "string") return;
      const { id, source, dark } = event.data;
      const config = {
        startOnLoad: false, securityLevel: "strict", suppressErrorRendering: true,
        maxTextSize: 32768, maxEdges: 200, theme: dark ? "dark" : "default",
        arrowMarkerAbsolute: false, htmlLabels: false, flowchart: { htmlLabels: false },
      };
      config.secure = [...Object.keys(config), "secure"];
      try {
        engine.initialize(config);
        const result = await engine.render(`c6diagram${++serial}`, source);
        send({ id, svg: result.svg });
      } catch (e) {
        send({ id, failure: String(e?.message || e).slice(0, 800) });
      } finally {
        document.body.replaceChildren();
      }
    });
    send({ ready: true });
  }

  let diagramFrame;
  let diagramQueue = Promise.resolve();
  let diagramID = 0;
  function framedEngine() {
    if (diagramFrame) return diagramFrame;
    const frame = document.createElement("iframe");
    frame.setAttribute("sandbox", "allow-scripts");
    frame.setAttribute("aria-hidden", "true");
    frame.tabIndex = -1;
    frame.style.cssText = "position:fixed;left:-10000px;top:0;width:1024px;height:768px;visibility:hidden;pointer-events:none";
    const nonce = Array.from(crypto.getRandomValues(new Uint8Array(16)), b => b.toString(16).padStart(2, "0")).join("");
    const vendor = new URL("mermaid-11.17.2.min.js", assets);
    const wait = readyMessage => new Promise((resolve, reject) => {
      const finish = (fn, value) => {
        clearTimeout(timer);
        window.removeEventListener("message", receive);
        fn(value);
      };
      const receive = event => {
        if (event.source !== frame.contentWindow || event.origin !== "null" || !readyMessage(event.data)) return;
        if (event.data.failure) finish(reject, new Error(event.data.failure));
        else finish(resolve, event.data);
      };
      const timer = setTimeout(() => {
        drop();
        finish(reject, new Error("Diagram render timed out"));
      }, 12000);
      window.addEventListener("message", receive);
    });
    const drop = () => { frame.remove(); if (diagramFrame?.frame === frame) diagramFrame = undefined; };
    const ready = wait(data => data?.ready || data?.failure && data.id == null);
    frame.srcdoc = `<!doctype html><meta charset="utf-8"><meta http-equiv="Content-Security-Policy" content="default-src 'none'; script-src 'nonce-${nonce}' ${vendor.href}; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'"><body><script src="${vendor.href}"></script><script nonce="${nonce}">(${frameProgram.toString()})();</script>`;
    document.body.append(frame);
    diagramFrame = { frame, ready, wait, drop };
    return diagramFrame;
  }

  function trustedDiagram(source) {
    if (source.length > 32768) throw new Error("Diagram exceeds 32 KiB");
    // Reject config blocks and user-directed references before the parser. A
    // separate CSP and SVG allowlist cover syntaxes this filter doesn't know.
    if (/^\s*---|%%\s*\{|\b(?:click|links?|href|callback)\s+[\w"']|\b(?:links?|href|img|image|icon|url)\s*:|\burl\s*\(|\]\s*\(|(?:https?|ftp|data|javascript|blob):|<\s*(?:a|img|svg|iframe|foreignobject)\b|\.\s*addLink\b/i.test(source)) {
      throw new Error("Diagram configuration, links and external resources are disabled");
    }
    return source;
  }

  async function renderDiagram(root, state) {
    const encoded = own(root, ".fv-source");
    const source = trustedDiagram(encoded?.dataset?.encoding === "json" ? JSON.parse(encoded.textContent) : encoded?.textContent || "");
    const task = diagramQueue.then(async () => {
      if (state.abort.signal.aborted || !root.isConnected) return;
      const box = own(root, ".fv-rendered");
      if (!box) return;
      const engine = framedEngine();
      try { await engine.ready; } catch (e) { engine.drop(); throw e; }
      const id = ++diagramID;
      const result = engine.wait(data => data?.id === id);
      const dark = document.documentElement.classList.contains("dark");
      engine.frame.contentWindow.postMessage({ id, source, dark }, "*");
      const { svg } = await result;
      if (state.abort.signal.aborted || !root.isConnected) return;
      const img = document.createElement("img");
      img.alt = "Mermaid diagram";
      img.src = blobURL(state, [safeSVG(svg)], "image/svg+xml");
      box.replaceChildren(img);
    });
    diagramQueue = task.catch(() => {});
    return task;
  }

  let pdfEngine;
  function pdfLibrary() {
    if (!pdfEngine) pdfEngine = import(new URL("pdf-6.5.438.min.mjs", assets).href).then(engine => {
      engine.GlobalWorkerOptions.workerSrc = new URL("pdf.worker-6.5.438.min.mjs", assets).href;
      return engine;
    });
    return pdfEngine;
  }

  async function renderPDF(root, state, target, parts) {
    const status = document.createElement("p");
    status.className = "fv-pdf-status";
    status.setAttribute("role", "status");
    status.textContent = "Loading PDF preview…";
    target.append(status);
    const active = () => root.isConnected && !state.abort.signal.aborted;
    let engine;
    try { engine = await pdfLibrary(); }
    catch (_) { if (active()) status.textContent = "PDF preview engine unavailable. Open PDF to view the file."; return; }
    if (!active()) return;
    const data = new Uint8Array(parts.reduce((bytes, part) => bytes + part.byteLength, 0));
    let offset = 0;
    for (const part of parts) { data.set(part, offset); offset += part.byteLength; }
    const loading = engine.getDocument({
      data, isEvalSupported: false, disableAutoFetch: true, disableRange: true, disableStream: true,
      useWorkerFetch: false, useWasm: false, disableFontFace: true, useSystemFonts: true,
      maxImageSize: 8e6, canvasMaxAreaInBytes: 32e6, disableWorkerRendering: true, enableXfa: false,
    });
    let inFlight;
    const canvases = new Set();
    state.cleanup.push(() => {
      inFlight?.cancel();
      void loading.destroy().catch(() => {}); // also destroys the document, if already loaded
      for (const canvas of canvases) canvas.width = canvas.height = 0;
    });
    let passwordProtected = false;
    loading.onPassword = () => {
      passwordProtected = true;
      void loading.destroy().catch(() => {});
    };
    let pdf;
    try { pdf = await loading.promise; }
    catch (e) {
      if (active()) status.textContent = passwordProtected ? "This PDF requires a password. Open PDF to unlock it." :
        `PDF preview unavailable: ${e.message || "Cannot open document"}`;
      return;
    }
    if (!active()) return;
    const pages = document.createElement("div");
    pages.className = "fv-pdf-pages";
    const more = document.createElement("button");
    more.type = "button";
    more.className = "fv-pdf-more";
    more.textContent = "Load more pages";
    target.append(pages, more);
    const documentLimit = 18e6;
    const pageLimit = 2e6;
    let pixels = 0, next = 1, busy = false, capped = false, warning = "";
    async function batch() {
      if (busy || !active()) return;
      busy = true;
      more.disabled = true;
      more.hidden = next + 4 >= pdf.numPages;
      const end = Math.min(next + 4, pdf.numPages, 60);
      for (; next <= end && active(); next++) {
        let page, canvas;
        try {
          page = await pdf.getPage(next);
          if (!active()) break;
          const size = page.getViewport({ scale: 1 });
          if (!Number.isFinite(size.width) || !Number.isFinite(size.height) || size.width <= 0 || size.height <= 0) {
            throw new Error("Invalid page size");
          }
          const scale = Math.min(2, 1200 / size.width, 4096 / size.height, Math.sqrt(pageLimit / size.width / size.height));
          const viewport = page.getViewport({ scale });
          const width = Math.max(1, Math.floor(viewport.width));
          const height = Math.max(1, Math.floor(viewport.height));
          if (width * height > pageLimit || pixels + width * height > documentLimit) { capped = true; break; }
          canvas = document.createElement("canvas");
          canvas.width = width;
          canvas.height = height;
          canvas.setAttribute("role", "img");
          canvas.setAttribute("aria-label", `PDF page ${next} of ${pdf.numPages}`);
          canvas.style.cssText = "display:block;max-width:100%;height:auto;margin:0.5rem auto;background:white";
          canvases.add(canvas);
          inFlight = page.render({ canvas, viewport, annotationMode: engine.AnnotationMode.DISABLE });
          await inFlight.promise;
          if (!active()) break;
          pages.append(canvas);
          pixels += width * height;
        } catch (e) {
          if (!active()) break;
          if (canvas) { canvases.delete(canvas); canvas.width = canvas.height = 0; }
          warning = `Page ${next} could not be rendered: ${e.message || "Unknown error"}`;
          break;
        } finally {
          inFlight = undefined;
          page?.cleanup();
        }
      }
      if (!active()) return;
      capped ||= next > 60 && next <= pdf.numPages;
      status.textContent = `${pdf.numPages} ${pdf.numPages === 1 ? "page" : "pages"}` +
        (warning ? ` · ${warning}` : capped ? ` · Preview limit reached after ${next - 1} pages. Open PDF to see the rest.` : "");
      more.hidden = capped || !!warning || next > pdf.numPages;
      more.disabled = false;
      busy = false;
    }
    more.addEventListener("click", () => { void batch(); }, { signal: state.abort.signal });
    await batch();
  }

  async function loadMedia(root, state) {
    const img = own(root, ".fv-image img, img.fv-image") || (root.dataset.kind === "image" ? own(root, "img") : null);
    const pdf = own(root, ".fv-pdf");
    if (!img && !pdf) return;
    if (img) setupImage(root, state, img);
    const display = (blob, parts) => {
      if (img) { img.src = blob; return; }
      const open = document.createElement("a");
      open.textContent = "Open PDF";
      open.href = blob;
      open.target = "_blank";
      open.rel = "noopener noreferrer";
      pdf.prepend(open);
      state.cleanup.push(() => open.remove());
      if (root.dataset.mode === "full" && parts) return renderPDF(root, state, pdf, parts);
    };
    const url = root.dataset.url || root.dataset.fvUrl;
    // Inline cards already have a bridge-owned binary URL; do not fetch twice.
    if (url?.startsWith("blob:")) {
      if (trustedBlob(root, url)) display(url);
      return;
    }
    const type = mediaType(root.dataset.path || "", root.dataset.mime);
    if (!url || !type || (root.dataset.kind === "image") !== type.startsWith("image/")) return;
    const parts = await read(url, limit, state.abort.signal);
    if (!root.isConnected || state.abort.signal.aborted) return;
    return display(blobURL(state, type === "image/svg+xml" ? [safeSVG(asText(parts))] : parts, type), parts);
  }

  function setupImage(root, state, img) {
    const box = img.closest(".fv-image-scroll, .fv-image-viewport") || (img.parentElement.tagName === "A" ? img.parentElement.parentElement : img.parentElement);
    if (!box || box === root) return;
    const signal = state.abort.signal;
    const dimensions = own(root, ".fv-dimensions");
    let scale = null, drag, suppressClick = false;
    box.style.overflow = "auto";
    img.draggable = false;
    const fit = () => {
      const cap = getComputedStyle(box).maxHeight;
      const height = root.dataset.mode === "inline" ? 180 : cap.endsWith("px") ? parseFloat(cap) : Infinity;
      return Math.min(1, (box.clientWidth || img.naturalWidth) / img.naturalWidth, height / img.naturalHeight);
    };
    function size(next, preserve) {
      if (!img.naturalWidth) return;
      const old = img.getBoundingClientRect();
      const x = old.width ? (box.scrollLeft + box.clientWidth / 2) / old.width : 0.5;
      const y = old.height ? (box.scrollTop + box.clientHeight / 2) / old.height : 0.5;
      scale = next;
      const factor = scale ?? fit();
      img.style.maxWidth = img.style.maxHeight = "none";
      img.style.width = `${Math.max(1, Math.round(img.naturalWidth * factor))}px`;
      img.style.height = "auto";
      box.scrollLeft = preserve ? x * img.clientWidth - box.clientWidth / 2 : 0;
      box.scrollTop = preserve ? y * img.clientHeight - box.clientHeight / 2 : 0;
      if (dimensions) dimensions.textContent = `${img.naturalWidth} × ${img.naturalHeight} · ${Math.round(factor * 100)}%`;
    }
    state.zoom = action => {
      if (!img.naturalWidth) return;
      if (action === "fit") size(null, false);
      else size(Math.max(0.01, Math.min(8, (scale ?? fit()) * (action === "zoom-in" ? 1.25 : 0.8))), true);
    };
    function loaded() { size(null, false); }
    img.addEventListener("load", loaded, { signal });
    img.addEventListener("error", () => error(root, "Image could not be displayed"), { signal });
    if (img.complete && img.naturalWidth) loaded();
    if (typeof ResizeObserver === "function") {
      let last = box.clientWidth;
      const resize = new ResizeObserver(() => {
        if (last !== box.clientWidth) { last = box.clientWidth; if (scale == null) loaded(); }
      });
      resize.observe(box);
      state.cleanup.push(() => resize.disconnect());
    }
    box.addEventListener("pointerdown", event => {
      if (event.pointerType === "touch" || event.button !== 0 || (box.scrollWidth <= box.clientWidth && box.scrollHeight <= box.clientHeight)) return;
      drag = { id: event.pointerId, x: event.clientX, y: event.clientY, left: box.scrollLeft, top: box.scrollTop };
      suppressClick = false;
    }, { signal });
    box.addEventListener("pointermove", event => {
      if (!drag || drag.id !== event.pointerId || !(event.buttons & 1)) return;
      const dx = event.clientX - drag.x, dy = event.clientY - drag.y;
      if (!suppressClick && Math.hypot(dx, dy) < 4) return;
      if (!box.hasPointerCapture(event.pointerId)) box.setPointerCapture(event.pointerId);
      suppressClick = true;
      box.scrollLeft = drag.left - dx;
      box.scrollTop = drag.top - dy;
      event.preventDefault();
    }, { signal });
    for (const name of ["pointerup", "pointercancel", "lostpointercapture"]) box.addEventListener(name, () => { drag = null; }, { signal });
    box.addEventListener("click", event => {
      if (suppressClick) { suppressClick = false; event.preventDefault(); event.stopPropagation(); }
    }, { signal, capture: true });
  }

  const ownedCards = new WeakMap();
  function trustedBlob(root, url) {
    const owner = root.closest(".fv-inline-card");
    return !!ownedCards.get(owner)?.urls.has(url);
  }

  function cardHTML(container, input) {
    const html = window.c6Viewers.render(input);
    if (typeof html !== "string" || !html.trim()) throw new Error("Preview renderer unavailable");
    const template = document.createElement("template");
    template.innerHTML = html; // shared Rust renderer, not file-supplied markup
    container.replaceChildren(template.content);
  }

  async function inlineCard(link, state) {
    if (!renderer) return;
    const path = link.dataset.fvPath;
    const head = link.dataset.fvUrl;
    const mime = media(path);
    let parts;
    try { parts = await read(head, previewLimit, state.abort.signal); }
    catch (e) { if (mime !== "application/pdf" || state.abort.signal.aborted) throw e; }
    if (!link.isConnected || state.abort.signal.aborted) return;
    const url = mime && parts ? blobURL(state, mime === "image/svg+xml" ? [safeSVG(asText(parts))] : parts, mime) : mime ? "" : head;
    const card = document.createElement("span");
    card.className = "fv-inline-card";
    ownedCards.set(card, state);
    cardHTML(card, { path, text: mime ? "" : asText(parts), url, mode: "inline", line: 0, mime: mime || "text/plain" });
    // Images are clickable without navigating to the SVG blob. Blob URLs are
    // only media sources; use the existing file-browser link for navigation.
    for (const anchor of card.querySelectorAll('a[href^="blob:"]')) anchor.href = anchor.hasAttribute("download") ? head : link.href;
    if (mime === "application/pdf" && !parts) {
      const fallback = document.createElement("a");
      fallback.href = link.href;
      fallback.textContent = "Open file viewer";
      card.querySelector(".fv-pdf")?.append(fallback);
    }
    if (mime.startsWith("image/")) {
      const img = card.querySelector("img");
      if (img && !img.closest("a")) {
        const anchor = document.createElement("a");
        anchor.href = link.href;
        img.before(anchor);
        anchor.append(img);
      }
    }
    state.cleanup.push(() => card.remove());
    link.after(card);
  }

  function setupContext(link, state, line) {
    const signal = state.abort.signal;
    let popup, pointer, timer, hideTimer, request, text, suppressClick = false;
    const hide = () => {
      clearTimeout(timer);
      clearTimeout(hideTimer);
      request?.abort();
      request = undefined;
      popup?.remove();
      popup = undefined;
      link.removeAttribute("aria-describedby");
    };
    const position = () => {
      if (!popup) return;
      const rect = link.getBoundingClientRect();
      popup.style.left = `${Math.max(8, Math.min(rect.left, innerWidth - popup.offsetWidth - 8))}px`;
      popup.style.top = `${Math.max(8, rect.bottom + popup.offsetHeight + 6 <= innerHeight ? rect.bottom + 6 : rect.top - popup.offsetHeight - 6)}px`;
    };
    const show = async () => {
      clearTimeout(hideTimer);
      if (popup || signal.aborted || !renderer) return;
      popup = document.createElement("div");
      popup.className = "fv-line-preview";
      popup.id = `c6-fv-context-${++contextID}`;
      popup.setAttribute("role", "tooltip");
      popup.style.cssText = "position:fixed;z-index:50;min-width:min(18rem,calc(100vw - 16px));max-width:min(42rem,calc(100vw - 16px));max-height:45vh;overflow:auto;padding:.5rem;border:1px solid GrayText;border-radius:.5rem;background:Canvas;color:CanvasText;box-shadow:0 4px 16px #0004";
      popup.textContent = "Loading source…";
      link.setAttribute("aria-describedby", popup.id);
      popup.addEventListener("pointerenter", () => clearTimeout(hideTimer));
      popup.addEventListener("pointerleave", () => { if (document.activeElement !== link) hide(); });
      document.body.append(popup);
      position();
      const current = popup;
      request = new AbortController();
      try {
        text ??= asText(await read(link.dataset.fvUrl, previewLimit, request.signal));
        if (popup !== current || signal.aborted) return;
        const parts = text.split(/\r?\n/);
        if (line > parts.length) throw new Error("Source line not found");
        // Render only nearby lines rather than rebuilding a full-file view.
        const start = Math.max(0, line - 4);
        const snippet = parts.slice(start, line + 3);
        const pre = document.createElement("pre");
        pre.className = "fv-context-code";
        pre.style.cssText = "margin:0;overflow:auto";
        snippet.forEach((value, idx) => {
          const row = document.createElement("span");
          row.style.display = "block";
          row.textContent = `${String(start + idx + 1).padStart(String(line + 3).length)}  ${value.slice(0, 320)}`;
          if (start + idx + 1 === line) {
            row.setAttribute("aria-current", "location");
            row.style.fontWeight = "bold";
          }
          pre.append(row);
        });
        popup.replaceChildren(pre);
        position();
      } catch (e) {
        if (popup !== current || signal.aborted) return;
        popup.textContent = `Source preview unavailable: ${e.message}`;
        position();
      }
    };
    link.addEventListener("pointerenter", e => {
      if (e.pointerType === "touch") return;
      clearTimeout(hideTimer);
      timer = setTimeout(show, 240);
    }, { signal });
    link.addEventListener("pointerleave", e => {
      if (e.pointerType === "touch") return;
      clearTimeout(timer);
      if (document.activeElement !== link) hideTimer = setTimeout(hide, 180);
    }, { signal });
    link.addEventListener("focus", () => { if (!pointer && !suppressClick) void show(); }, { signal });
    link.addEventListener("blur", hide, { signal });
    link.addEventListener("pointerdown", e => {
      suppressClick = false;
      if (e.pointerType !== "touch") return;
      pointer = { x: e.clientX, y: e.clientY };
      timer = setTimeout(() => { suppressClick = true; void show(); }, 550);
    }, { signal });
    link.addEventListener("pointermove", e => {
      if (pointer && Math.hypot(e.clientX - pointer.x, e.clientY - pointer.y) > 10) clearTimeout(timer);
    }, { signal });
    for (const name of ["pointerup", "pointercancel"]) link.addEventListener(name, () => { pointer = undefined; clearTimeout(timer); }, { signal });
    link.addEventListener("contextmenu", e => { if (suppressClick || pointer) e.preventDefault(); }, { signal });
    link.addEventListener("click", e => { if (suppressClick) { e.preventDefault(); suppressClick = false; } }, { signal });
    document.addEventListener("pointerdown", e => { if (popup && !link.contains(e.target) && !popup.contains(e.target)) hide(); }, { signal });
    document.addEventListener("keydown", e => { if (e.key === "Escape") hide(); }, { signal });
    state.cleanup.push(hide);
  }

  let contextID = 0;
  function stateFor(el) {
    let state = states.get(el);
    if (!state) {
      state = { abort: new AbortController(), urls: new Set(), cleanup: [], observed: false };
      states.set(el, state);
    }
    return state;
  }

  async function enhance(el) {
    const state = states.get(el);
    if (!state || !el.isConnected || state.abort.signal.aborted) return;
    try {
      if (el.matches(".c6-file-link")) await inlineCard(el, state);
      else if (el.matches("code")) await highlightFence(el);
      else switch (el.dataset.kind) {
        case "mermaid": await renderDiagram(el, state); break;
        case "image": case "pdf": await loadMedia(el, state); break;
        default: centerLine(el); await highlightRows(el);
      }
    } catch (e) {
      if (state.abort.signal.aborted || !el.isConnected || !el.matches(".fv")) return;
      if (el.dataset.kind === "mermaid") {
        error(el, `Mermaid: ${e.message || "Invalid diagram"}`);
        sourceView(el, true);
      } else if (["image", "pdf"].includes(el.dataset.kind)) error(el, e.message || "Preview unavailable");
    }
  }

  const visible = typeof IntersectionObserver === "function" ? new IntersectionObserver(entries => {
    for (const { target, isIntersecting } of entries) if (isIntersecting) {
      visible.unobserve(target);
      void enhance(target);
    }
  }) : null;

  function scanNode(el) {
    if (el.closest(".fv-source, .fv-source-view") && !el.closest(".fv-code")) return;
    if (el.matches(".c6-file-link")) {
      if (!renderer || el.closest(".fv-inline-card, .fv-line-preview")) return;
      const path = el.dataset.fvPath;
      try { downloadURL(el.dataset.fvUrl); } catch (_) { return; }
      const state = stateFor(el);
      const line = Number(el.dataset.fvLine);
      if (Number.isSafeInteger(line) && line > 0 && !media(path)) {
        if (!state.context) { state.context = true; setupContext(el, state, line); }
        return;
      }
      if (!media(path) && !["md", "markdown", "csv", "tsv", "json", "jsonl", "ndjson", "diff", "patch", "log", "mmd", "mermaid"].includes(ext(path))) return;
    } else if (el.matches("code")) {
      if (el.closest(".fv-code") || !language(el)) return;
    } else if (!["image", "pdf", "mermaid"].includes(el.dataset.kind) && !el.querySelector(".fv-code code")) return;
    const state = stateFor(el);
    if (state.observed) return;
    state.observed = true;
    if (visible) visible.observe(el);
    else {
      // Older engines may still highlight and display media; never eagerly
      // start Mermaid or automatic worker previews without visibility support.
      if (el.dataset.kind !== "mermaid" && !el.matches(".c6-file-link")) void enhance(el);
    }
  }

  function scan(node, visit) {
    if (node.nodeType !== Node.ELEMENT_NODE) return;
    if (node.matches(candidates)) visit(node);
    node.querySelectorAll(candidates).forEach(visit);
  }

  function cleanup(el) {
    if (el.isConnected) return; // the same batch may have reparented it
    const state = states.get(el);
    if (!state) return;
    visible?.unobserve(el);
    state.abort.abort();
    for (const dispose of state.cleanup) dispose();
    for (const url of state.urls) URL.revokeObjectURL(url);
    states.delete(el);
  }

  document.addEventListener("click", event => {
    const button = event.target.closest?.("[data-fv-action]");
    const root = button?.closest(".fv");
    if (!root) return;
    const action = button.dataset.fvAction;
    if (action === "source") sourceView(root, button.dataset.fvView ? button.dataset.fvView === "raw" : !!own(root, ".fv-source-view")?.hidden);
    else if (["zoom-in", "zoom-out", "fit"].includes(action)) states.get(root)?.zoom?.(action);
    else if (action === "expand") {
      const details = own(root, "details:not([open])");
      if (details) details.open = true;
    }
  });

  new MutationObserver(records => {
    for (const record of records) for (const node of record.removedNodes) scan(node, cleanup);
    for (const record of records) for (const node of record.addedNodes) scan(node, scanNode);
  }).observe(document.body, { childList: true, subtree: true });
  scan(document.body, scanNode);
})();
