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
  // Resolves to a JSON array of {name, mime, data (base64)}.
  async read(ev) {
    const out = [];
    for (const f of this.list(ev)) {
      const buf = new Uint8Array(await f.arrayBuffer());
      let s = "";
      for (let i = 0; i < buf.length; i += 0x8000) s += String.fromCharCode(...buf.subarray(i, i + 0x8000));
      const name = f.name && f.name !== "image.png" ? f.name : `pasted-${Date.now()}.${(f.type.split("/")[1] || "png")}`;
      out.push({ name, mime: f.type || "text/plain", data: btoa(s) });
    }
    if (ev.target && ev.target.files) ev.target.value = "";
    return JSON.stringify(out);
  },
};
