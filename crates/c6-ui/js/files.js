// Files for the composer: from a paste, a drop or a file picker, read as
// base64 for upload.
window.c6Files = {
  list(ev) {
    if (ev.clipboardData) return [...ev.clipboardData.files];
    if (ev.dataTransfer) return [...ev.dataTransfer.files];
    if (ev.target && ev.target.files) return [...ev.target.files];
    return [];
  },
  // The event carries files (a pasted screenshot, dropped files).
  has(ev) {
    return this.list(ev).length > 0;
  },
  // Keep the existing inline-image behavior: Anthropic accepts at most 5 MB
  // per image. Other file types always retain their original bytes.
  async shrink(f) {
    const LIMIT = 4.5 * 1024 * 1024;
    if (f.size <= LIMIT || !/^image\/(png|jpeg|webp)$/.test(f.type)) return f;
    let bmp;
    try {
      bmp = await createImageBitmap(f);
    } catch (_) {
      return f;
    }
    let edge = Math.min(Math.max(bmp.width, bmp.height), 2560);
    let out = f;
    for (let i = 0; i < 4 && out.size > LIMIT; i++, edge = Math.round(edge * 0.75)) {
      const k = edge / Math.max(bmp.width, bmp.height);
      const c = document.createElement("canvas");
      c.width = Math.round(bmp.width * k);
      c.height = Math.round(bmp.height * k);
      c.getContext("2d").drawImage(bmp, 0, 0, c.width, c.height);
      out = await new Promise((r) => c.toBlob(r, "image/png"));
      if (out.size > LIMIT) out = await new Promise((r) => c.toBlob(r, "image/jpeg", 0.9));
    }
    bmp.close();
    if (out.size >= f.size) return f;
    const name = out.type === f.type ? f.name : f.name.replace(/\.[^.]*$/, "") + ".jpg";
    return new File([out], name, { type: out.type });
  },
  // Resolves to a JSON array of {name, mime, data (base64)}.
  async read(ev) {
    // Snapshot the files before awaiting: paste and drop data are only
    // accessible while their event is being dispatched.
    const files = this.list(ev);
    if (ev.target && ev.target.files) ev.target.value = "";
    const out = [];
    for (const raw of files) {
      const f = await this.shrink(raw);
      const buf = new Uint8Array(await f.arrayBuffer());
      let s = "";
      for (let i = 0; i < buf.length; i += 0x8000) s += String.fromCharCode(...buf.subarray(i, i + 0x8000));
      out.push({ name: f.name, mime: f.type || "application/octet-stream", data: btoa(s) });
    }
    return JSON.stringify(out);
  },
};
