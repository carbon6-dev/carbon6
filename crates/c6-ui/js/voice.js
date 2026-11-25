// Audio lives only in this controller: never localStorage, IndexedDB or a draft.
window.c6Voice = (() => {
  const MAX_BYTES = 25 * 1024 * 1024;
  const MAX_SECONDS = 10 * 60;
  // Chrome supports Opus; Safari falls through to MP4/AAC. Probe, don't sniff.
  const chooseType = (Recorder) => [
    "audio/webm;codecs=opus", "audio/mp4;codecs=mp4a.40.2", "audio/mp4", "audio/webm",
  ].find((type) => Recorder.isTypeSupported(type));
  const permissionMessage = (error) => {
    switch (error?.name) {
      case "NotAllowedError": case "SecurityError":
        return "Microphone access was denied. Allow microphone access in your browser's site settings, then try again.";
      case "NotFoundError": case "DevicesNotFoundError":
        return "No microphone was found. Connect a microphone and try again.";
      case "NotReadableError": case "TrackStartError":
        return "The microphone is busy or unavailable. Close other apps using it and try again.";
      default: return "Could not record audio. Check your microphone and try again.";
    }
  };
  return {
    chooseType,
    create(onState, onText, options = {}) {
      let state = "idle", elapsed = 0, generation = 0, disposed = false;
      let stream, recorder, timer, controller, audio;
      const emit = (next, extra = {}) => {
        state = next;
        if (!disposed) onState(JSON.stringify({ state, elapsed, ...extra }));
      };
      const release = () => {
        clearInterval(timer);
        timer = undefined;
        stream?.getTracks().forEach((track) => track.stop());
        stream = undefined;
      };
      const detach = () => {
        if (recorder) {
          recorder.ondataavailable = recorder.onstop = recorder.onerror = null;
          if (recorder.state !== "inactive") recorder.stop();
          recorder = undefined;
        }
        release();
      };
      const cancel = () => {
        generation++;
        controller?.abort();
        controller = undefined;
        detach();
        audio = undefined;
        elapsed = 0;
        emit("idle");
      };
      const upload = async () => {
        if (!audio || disposed || state === "transcribing") return;
        const current = generation;
        controller = new AbortController();
        emit("transcribing");
        try {
          const response = await fetch(options.url || "/api/v1/transcribe", {
            method: "POST", headers: { ...options.headers, "Content-Type": audio.type },
            body: audio, signal: controller.signal,
          });
          if (!response.ok) {
            const raw = await response.text();
            let message;
            try { const body = JSON.parse(raw); message = body.error?.message || body.error || body.message; } catch (_) { /* plain API error */ }
            throw new Error(typeof message === "string" ? message : raw.slice(0, 500) || `Transcription failed (${response.status}).`);
          }
          const result = await response.json();
          if (current !== generation || disposed) return;
          if (typeof result.text !== "string") throw new Error("The transcription provider returned no text. Try again.");
          if (!result.text.trim()) throw new Error("No speech was detected. Retry or make a new recording.");
          audio = undefined;
          controller = undefined;
          emit("idle");
          onText(result.text);
        } catch (error) {
          if (current !== generation || disposed) return;
          controller = undefined;
          emit("error", { error: error instanceof TypeError
            ? "Could not reach transcription. Check your connection, then Retry."
            : error.message || "Transcription failed. Try again.", retry: !!audio });
        }
      };
      const stop = () => {
        if (state !== "recording" || !recorder) return;
        clearInterval(timer);
        timer = undefined;
        emit("transcribing");
        if (recorder.state !== "inactive") recorder.stop();
      };
      const start = async () => {
        if (disposed || ["requesting", "recording", "transcribing"].includes(state)) return;
        cancel();
        if (!window.isSecureContext) {
          emit("error", { error: "Microphone recording needs HTTPS. Open carbon6 using its secure tailnet address." });
          return;
        }
        if (!navigator.mediaDevices?.getUserMedia || !window.MediaRecorder) {
          emit("error", { error: "This browser cannot record audio. Try Safari or Chrome with microphone access enabled." });
          return;
        }
        const mimeType = chooseType(window.MediaRecorder);
        if (!mimeType) {
          emit("error", { error: "This browser has no supported audio recording format. Try Safari or Chrome." });
          return;
        }
        const current = generation;
        emit("requesting");
        try {
          const granted = await navigator.mediaDevices.getUserMedia({ audio: true });
          if (current !== generation || disposed) {
            granted.getTracks().forEach((track) => track.stop());
            return;
          }
          stream = granted;
          recorder = new MediaRecorder(stream, { mimeType, audioBitsPerSecond: 64000 });
          const chunks = [];
          let bytes = 0;
          recorder.ondataavailable = (event) => {
            if (current !== generation || !event.data.size) return;
            bytes += event.data.size;
            if (bytes > MAX_BYTES) {
              cancel();
              emit("error", { error: "The recording is too large (25 MB maximum). Try a shorter recording." });
              return;
            }
            chunks.push(event.data);
          };
          recorder.onerror = () => {
            cancel();
            emit("error", { error: "Audio recording failed. Check your microphone and try again." });
          };
          recorder.onstop = () => {
            if (current !== generation || disposed) return;
            const type = recorder.mimeType || mimeType;
            recorder = undefined;
            release();
            audio = new Blob(chunks, { type });
            if (!audio.size) {
              audio = undefined;
              emit("error", { error: "No audio was recorded. Try recording again." });
              return;
            }
            // stop() already published the waiting state; permit upload to start.
            state = "recording";
            void upload();
          };
          recorder.start(1000);
          const began = performance.now();
          emit("recording");
          timer = setInterval(() => {
            elapsed = Math.floor((performance.now() - began) / 1000);
            if (elapsed >= MAX_SECONDS) stop();
            else emit("recording");
          }, 250);
        } catch (error) {
          if (current !== generation || disposed) return;
          detach();
          emit("error", { error: permissionMessage(error) });
        }
      };
      return { start, stop, cancel, retry: upload, dispose() { disposed = true; cancel(); } };
    },
  };
})();
