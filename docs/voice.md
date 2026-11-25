# Dictating prompts

The microphone beside the composer's attachment button records a prompt, not a
message. Tap to start, tap to stop; recording shows elapsed time and Cancel.
The transcript is inserted at the caret and is never sent automatically. Review
and edit it before Send. Further recordings insert more text.

## Recording and privacy

Recording needs microphone permission and a secure context (HTTPS on the
tailnet, or localhost). The browser probes MediaRecorder support, preferring
WebM/Opus where supported, falling back to MP4/AAC for Safari. Recording is
limited to ten minutes and 25 MB. Cancel stops the microphone and discards the
recording.
During transcription the audio stays in browser memory so a failure can offer
Retry. The head forwards it to the selected transcription provider and does not
store it. That provider's own retention policy still applies.

## Models and vocabulary

`POST /api/v1/transcribe` accepts raw audio with its MIME type and an optional
session ID. It is realm checked and catalogued as `voice.transcribe`, a UI-only
operation. A matching `task:transcribe` routing rule wins; otherwise the Memex
transcription connection and model are used. Configure either in Settings →
Memex or routing rules. The provider must support `/audio/transcriptions`.

For an accessible session, the head builds a best-effort, bounded vocabulary
prompt from its repository, branch and recent transcript's technical words,
paths and crate names. This is vocabulary, not instructions or added content.
It is sent using the transcription API's `prompt` field. The vocabulary is
deduplicated and capped at 224 UTF-8 bytes, conservatively within Whisper's
224-token prompt limit without requiring a tokenizer on the head.

An optional Tidy action rewrites the inserted dictation without adding content,
preserving meaning and technical words and removing fillers. `task:tidy` wins,
otherwise the suggestion model is used. The instruction is editable as
`voice.tidy`. Undo restores the raw text. Neither action sends the composer.

## Failures

Permission denial and missing microphones show actionable messages. While
waiting, the composer shows “Transcribing…” with Cancel. A failed transcription
keeps the audio only in memory and offers Retry; leaving the page loses it.

## Regression checks

```sh
cargo test --locked -p c6-head voice::tests --lib
cargo test --locked -p c6-ui voice::tests
node --test crates/c6-ui/tests/voice.test.cjs
```

The head tests use a local OpenAI-compatible HTTP server and inspect forwarded
audio, MIME type, vocabulary and model routing. They check missing models, the
body-size cap and realm isolation. UI helper tests cover UTF-16 selections,
successive insertions, and safe Tidy/Undo. JS tests inject recorder events,
permission results and time; no real microphone or elapsed-time sleep is needed.
