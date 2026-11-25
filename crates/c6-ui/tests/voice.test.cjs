const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");
const source = fs.readFileSync(`${__dirname}/../js/voice.js`, "utf8");

function fixture(overrides = {}) {
  const states = [], texts = [], requests = [], tracks = [];
  const waiters = [];
  let recorder;
  class Recorder {
    static isTypeSupported(type) { return type === "audio/webm;codecs=opus"; }
    constructor(stream, options) { recorder = this; this.mimeType = options.mimeType; this.state = "inactive"; }
    start() { this.state = "recording"; }
    stop() {
      this.state = "inactive";
      this.ondataavailable?.({ data: new Blob(["speech"], { type: this.mimeType }) });
      this.onstop?.();
    }
  }
  const window = { isSecureContext: true, MediaRecorder: Recorder };
  const context = {
    window, MediaRecorder: Recorder, Blob, AbortController, performance,
    setInterval, clearInterval,
    navigator: { mediaDevices: { getUserMedia: async () => {
      const track = { stopped: false, stop() { this.stopped = true; } };
      tracks.push(track);
      return { getTracks: () => [track] };
    } } },
    fetch: async (url, options) => {
      requests.push({ url, ...options });
      return { ok: true, json: async () => ({ text: "Fix c6-worker" }) };
    },
    ...overrides,
  };
  vm.runInNewContext(source, context);
  const control = context.window.c6Voice.create((raw) => {
    const state = JSON.parse(raw);
    states.push(state);
    for (const waiter of waiters) if (waiter.state === state.state) waiter.resolve();
  }, (text) => texts.push(text), {
    url: "/api/v1/transcribe?session_id=s", headers: { "X-C6-Realm": "test" },
  });
  const until = (state) => states.at(-1)?.state === state ? Promise.resolve()
    : new Promise((resolve) => waiters.push({ state, resolve }));
  return { ...context, control, states, texts, requests, tracks, until, get recorder() { return recorder; } };
}

test("format negotiation supports Safari MP4/AAC and Chrome Opus, without UA sniffing", () => {
  const { window } = fixture();
  for (const type of ["audio/webm;codecs=opus", "audio/mp4;codecs=mp4a.40.2", "audio/mp4", "audio/webm"]) {
    assert.equal(window.c6Voice.chooseType({ isTypeSupported: (t) => t === type }), type);
  }
  assert.equal(window.c6Voice.chooseType({ isTypeSupported: () => true }), "audio/webm;codecs=opus");
  assert.equal(window.c6Voice.chooseType({ isTypeSupported: () => false }), undefined);
});

test("stop forwards audio MIME and realm headers, returns text and releases microphone", async () => {
  const f = fixture();
  await f.control.start();
  assert.equal(f.states.at(-1).state, "recording");
  f.control.stop();
  await f.until("idle");
  assert.deepEqual(f.texts, ["Fix c6-worker"]);
  assert.equal(f.requests[0].headers["Content-Type"], "audio/webm;codecs=opus");
  assert.equal(f.requests[0].headers["X-C6-Realm"], "test");
  assert.equal(await f.requests[0].body.text(), "speech");
  assert.ok(f.tracks.every((t) => t.stopped));
  f.control.dispose();
});

test("permission denial is actionable and not a raw exception", async () => {
  const f = fixture({ navigator: { mediaDevices: { getUserMedia: async () => { throw { name: "NotAllowedError" }; } } } });
  await f.control.start();
  assert.match(f.states.at(-1).error, /Allow microphone access/);
  assert.equal(f.requests.length, 0);
});

test("cancel during permission prompt stops the late stream without recording", async () => {
  let grant;
  let stopped = false;
  const f = fixture({ navigator: { mediaDevices: { getUserMedia: () => new Promise((resolve) => { grant = resolve; }) } } });
  const pending = f.control.start();
  f.control.cancel();
  grant({ getTracks: () => [{ stop() { stopped = true; } }] });
  await pending;
  assert.equal(stopped, true);
  assert.equal(f.recorder, undefined);
  assert.equal(f.states.at(-1).state, "idle");
});

test("transcription failure retains the same audio for Retry", async () => {
  const requests = [];
  const f = fixture({ fetch: async (_, request) => {
    requests.push(request);
    return requests.length === 1
      ? { ok: false, text: async () => JSON.stringify({ error: "Choose a transcription model in Settings → Memex" }) }
      : { ok: true, json: async () => ({ text: "retry worked" }) };
  } });
  await f.control.start(); f.control.stop();
  await f.until("error");
  assert.equal(f.states.at(-1).retry, true);
  assert.match(f.states.at(-1).error, /Settings/);
  await f.control.retry();
  assert.equal(requests[0].body, requests[1].body);
  assert.deepEqual(f.texts, ["retry worked"]);
  f.control.dispose();
});

test("cancel aborts upload and ignores a late response", async () => {
  let respond, request;
  const f = fixture({ fetch: (_, options) => {
    request = options;
    return new Promise((resolve) => { respond = resolve; });
  } });
  await f.control.start(); f.control.stop(); f.control.cancel();
  assert.equal(request.signal.aborted, true);
  respond({ ok: true, json: async () => ({ text: "must not insert" }) });
  await Promise.resolve(); await Promise.resolve();
  assert.deepEqual(f.texts, []);
  assert.equal(f.states.at(-1).state, "idle");
});

test("oversized audio is discarded before upload and releases the microphone", async () => {
  const f = fixture();
  await f.control.start();
  f.recorder.ondataavailable({ data: { size: 25 * 1024 * 1024 + 1 } });
  assert.equal(f.states.at(-1).state, "error");
  assert.match(f.states.at(-1).error, /25 MB/);
  assert.ok(f.tracks.every((t) => t.stopped));
  assert.equal(f.requests.length, 0);
  await f.control.retry();
  assert.equal(f.requests.length, 0);
});

test("ten-minute limit stops recording without wall-clock waits", async () => {
  let tick, now = 0;
  const f = fixture({
    performance: { now: () => now },
    setInterval: (fn) => { tick = fn; return 1; }, clearInterval: () => {},
  });
  await f.control.start();
  now = 600000;
  tick();
  await f.until("idle");
  assert.equal(f.requests.length, 1);
  assert.equal(f.states.at(-1).elapsed, 600);
  assert.ok(f.tracks.every((t) => t.stopped));
});

test("dispose stops recording without uploading or publishing further state", async () => {
  const f = fixture();
  await f.control.start();
  const count = f.states.length;
  f.control.dispose();
  assert.equal(f.requests.length, 0);
  assert.ok(f.tracks.every((t) => t.stopped));
  assert.equal(f.states.length, count);
  await f.control.start();
  assert.equal(f.tracks.length, 1);
});

test("insecure context explains HTTPS requirement", async () => {
  const f = fixture({ window: { isSecureContext: false } });
  await f.control.start();
  assert.match(f.states.at(-1).error, /HTTPS/);
  assert.equal(f.tracks.length, 0);
});

test("silent audio offers an explanation and keeps the recording retryable", async () => {
  const f = fixture({ fetch: async () => ({ ok: true, json: async () => ({ text: "  " }) }) });
  await f.control.start(); f.control.stop();
  await f.until("error");
  assert.match(f.states.at(-1).error, /No speech/);
  assert.equal(f.states.at(-1).retry, true);
  assert.deepEqual(f.texts, []);
});
