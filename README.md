# carbon6

An AI coding-agent harness for running agents on many machines from one place.

- **`c6h`** (head) runs centrally. It owns all configuration, credentials and session data, and serves the web UI.
- **`c6w`** (worker) runs headless on each dev machine (macOS or Linux). It attaches to the head and runs agent sessions there.

See [docs/requirements.md](docs/requirements.md) and [docs/architecture.md](docs/architecture.md).

## What it does

- **Sessions on any worker, from one web UI.** You can watch output stream live (text, reasoning, tool calls and shell output), send messages mid-turn, and stop a turn. Sessions can run in a fresh git worktree.
- **Providers:**
  - Anthropic API (Claude)
  - OpenAI API (Responses), or your ChatGPT plan via Sign in with ChatGPT
  - Any OpenAI-compatible server (Ollama, vLLM, OpenRouter, …)
  - **Claude Code**, which runs each worker's own signed-in `claude` CLI
- **Model features:** prompt caching, reasoning carried between steps, effort per session, and automatic compaction for long sessions.
- **Tool sets per model family:** Claude Code-style for Claude, Codex-style for GPT, and a generic set otherwise. Tools include read, write, edit, `apply_patch`, bash (with background jobs), grep, glob, `web_fetch`, the provider's own web search, plans, subagents, MCP servers and skills.
- **Offline-tolerant.** Workers journal every event before sending it. Sessions keep running when the head is unreachable and sync back exactly when it returns. A worker that loses its journal has its sessions restored from the head.
- **IDE-like access to workers:** browse files, review git changes, and open terminals in the browser.
- **Observability:** the head exports OpenTelemetry GenAI traces over OTLP.
- **Next: carbon6 as an operating system for agents.** Realms isolate apps from each other, and apps are built from general primitives (collections, decisions, triggers, series, views) with configuration only. This is a design, not built yet: see [docs/os.md](docs/os.md).

## Build

Run `make` to list the targets.

**With Docker only.** You need nothing else installed:

```bash
make docker-build      # Linux binaries for this machine's architecture → dist/linux-<arch>/
make docker-test       # the full test suite
make docker-head       # run the head in a container on http://127.0.0.1:7700
make docker-token      # an enrollment token from that head
```

- `make docker-build-all` builds both amd64 and arm64.
- `make docker-images` builds the `carbon6/c6h` and `carbon6/c6w` runtime images.
- Docker produces Linux binaries only. A worker on a Mac needs a native `c6w`, built with the local toolchain below.

**Ubuntu packages.** `make docker-deb` builds `c6h`, `c6w` and `c6m` `.deb` packages with their systemd units, and `c6` (the CLI), into `dist/deb/ubuntu-22.04/`. Built on 22.04, they install on 22.04 and every later release (24.04, 26.04, ...); set `UBUNTU=` to build on another. `make docker-deb-all` builds amd64 and arm64. `make docker-deb-test` installs them in an Ubuntu container under systemd and checks the services (`ARCH=amd64` on an arm64 machine tests the emulated one). After `sudo apt install ./c6h_*.deb` (and so on):

- **`c6h`** runs as the `c6h` service (user `c6h`) and starts on install. Its config is `/etc/c6h/c6h.toml`, its state `/var/lib/c6h`. Run admin commands as that user: `sudo -u c6h c6h --config /etc/c6h/c6h.toml login-link` (or `enroll-token`, `api-token`).
- **`c6w`** runs as a user, since sessions work on that user's files. Enroll as that user (`c6w enroll <token>`), then either `sudo systemctl enable --now c6w@$USER` (from boot) or `systemctl --user enable --now c6w` (in your user session). It starts through a login shell, so sessions get your `PATH`.
- **`c6m`** runs as the `c6m` service (user `c6m`, state in `/var/lib/c6m`) once enrolled: `sudo -u c6m c6m --state-dir /var/lib/c6m enroll <token>`, then `sudo systemctl enable --now c6m`.
- **`c6`** is the command line, for any machine that reaches the head: `c6 login <url> --token <token>` with a token from `c6h api-token`.
- Logs: `journalctl -u c6h` (or `c6w@<user>`, `c6m`). Upgrading a package restarts its services; removing one keeps its data.

**With a local toolchain.** You need Rust with the `wasm32-unknown-unknown` target, plus [Trunk](https://trunkrs.dev), which downloads Tailwind and wasm-bindgen on first use. protoc is vendored.

```bash
make build             # web UI, then release c6h and c6w into target/release
make test
```

## Run

On the head machine:

```bash
c6h               # binds private/tailnet addresses only; prints the web UI URL
```

1. Sign in: run `c6h login-link` on the head and open the link it prints (it works once, for 15 minutes). Then add a passkey under **Settings → Sign-in**, and sign in with Touch ID, Face ID or a security key from then on. A new link always gets you back in.
   - To sign in another device (a phone), create a one-time code under **Settings → Sign-in → Sign in another device** and type it into that device's sign-in page. Codes work once, for 10 minutes; five wrong codes cancel all outstanding ones.
2. In the UI, add an LLM connection under **Settings**. For a local Ollama, use `http://127.0.0.1:11434/v1` with no key.
3. On the **Workers** page, click **Add worker**, then run the printed command on the worker machine (or create a token with `c6h enroll-token` on the head):

```bash
c6w enroll c6e1.…
c6w run
```

### Command line (`c6`)

`c6` runs sessions headless, for scripts and terminals. It signs in with an API token: create one under **Settings → Sign-in → API tokens** (or with `c6h api-token` on the head), then:

```bash
c6 login https://head.example:7700 --token c6_… --ca carbon6-ca.pem   # --ca only if the head signs its own certificate
```

- **`c6 run "fix the failing test"`** starts a session in the current folder on the only online worker (or `--worker`, `-C <dir>`, `-c <connection>`, `-m <model>`; the model defaults to your latest session's). It prints the reply as the agent works, with tool calls on stderr, and exits 0 when the turn completes, 1 when it fails, or 130 when interrupted (Ctrl-C interrupts it). The prompt can come from stdin, and `--json` prints events as JSON lines.
- **`c6 send <id> "…"`** continues a session (a unique id prefix is enough). Also: `c6 wait`, `c6 show` (Markdown), `c6 stop`, `c6 sessions`, `c6 search` and `c6 workers`.
- **`c6 ops [words]`** finds operations, meaning everything you can do in carbon6 (with no words, it lists the areas; with an operation's name, it shows its input).
- **`c6 op <name> key=value…`** runs one and prints the result as JSON, for example `c6 op workers.enrollments.create body='{"name":"build-1"}'` or `c6 op connections.models id=<id>`. Destructive operations ask first, or skip the question with `--yes`.
- **API tokens** reach the whole API except sign-in management, or only one realm's data if you limit them (`c6h api-token --realm home`, or the realm choice under Settings → Sign-in → API tokens). Revoke them in Settings. `C6_URL`, `C6_TOKEN` and `C6_CA` override the saved login.
- **The API reference:**
  - `GET /api/v1/openapi.json` describes the whole API as OpenAPI 3.1, generated from the same catalog, for code generation.
  - Agents writing an integration in a session use the `carbon6_api` tool. It gives the base URL, how to authenticate, and each operation's method, path and input, and can save the OpenAPI document into the project. It only reads the reference; the code it writes calls carbon6 with your token.

### The assistant

**✦ Ask** in the header (⌘J) asks carbon6 to do anything you can do in the UI, or to explain your setup. For example:

- "What can I do in carbon6?"
- "Which models are set up for vision?"
- "Create an enrollment token for a new worker"

How it works:

- **It runs in the head**, so it works before any worker is attached.
- **It finds operations** in a catalog of everything the UI does. A test keeps that catalog complete.
- **It asks for missing details with a form** in the conversation, with choices from your actual workers, connections and sessions.
- **Secrets never pass through the model.** Tokens and codes it creates are shown only to you, in a card. Secrets you give it (API keys) stay out of the model too.
- **It waits for your approval before anything destructive**, such as deleting, revoking, sending mail, or replacing a list with fewer entries.
- **Its model is set under Settings → Assistant.** Its conversations have the label `assistant`, so routes, policies and budgets for that label apply.
- **The design is in [docs/assistant.md](docs/assistant.md).**

### Collections

Collections are typed tables of records in a realm, for anything apps and agents keep: candidates, contacts, experiments, inventories. See them under **Collections**.

- **Strict:** each collection has a JSON Schema. A record must match it exactly: no fields the schema doesn't name, every required one, the right types and values. Optional unique keys (such as `name`) are kept unique.
- **Safe to share:** an update gives the version it read, so two writers can't silently overwrite each other. Every change is kept, with who made it (you, a session, or a token).
- **Agents:** sessions use `records_query`, `records_get`, `records_put` and `records_delete`, within their realm. Records are also searchable in the memex and can appear in session briefs.
- **Code:** `/api/v1/collections…` (see `carbon6_api` or `/api/v1/openapi.json`). A query filters by field (`{"where": {"score": {"$gte": 8}}}`), by words, sorts, pages, or counts per value.
- **Changing a schema:** a new schema is accepted if every record already there still fits.

### Environment knowledge and Decisions

- **Environment knowledge:** agents record what they learn about the systems around the code (CI, clusters, networks, hosts) as facts in the memex. Each fact says how it is known (reviewed, verified, observed, inferred), where it applies, and how to re-check it. Every session starts with the relevant facts, and contradictions become decisions.
- **Decisions:** everything that needs you is in one list under **Decisions**:
  - tool calls waiting for approval;
  - workflow steps waiting for a go-ahead;
  - the assistant's questions;
  - questions agents asked without stopping (`decision_request`: the answer comes back to the session as a message);
  - facts to confirm, and disputes.

  Answer with an option, a form or a note, or start a session that helps you settle one. Answering one answers what waits on it, and answering in the session settles the decision. With options, a notification carries a link that answers it from your phone. With a deadline, an unanswered decision takes its default answer.
- **The design:** see [docs/os.md](docs/os.md) §4.2 and [docs/follow-through.md](docs/follow-through.md) §6.

### Configuration

- **Head:** `c6h.toml`, optional (see `crates/c6-head/src/config.rs`): `~/Library/Application Support/c6h/c6h.toml` on macOS, `~/.config/c6h/c6h.toml` on Linux, or `--config`.
  - State (database, `master.key`, CAs) lives in `data_dir`: by default `~/Library/Application Support/c6h` (macOS) or `~/.local/share/c6h` (Linux). Back up `c6h.db` and `master.key` together.
  - Add any names you reach the head by to `extra_names`; the head refuses requests addressed to other names. Its Tailscale MagicDNS name is added automatically.
  - Passkeys need HTTPS (or `http://localhost`). `ui_tls_head_ca = true` makes the head issue its own certificate from a private CA, so its names never appear in public Certificate Transparency logs (as they would with Let's Encrypt or Tailscale certificates). Trust that CA once per device:
    - Mac (Chrome and Safari use the macOS Keychain): `curl -so /tmp/carbon6-ca.pem http://<head>:7700/ca.pem`, then `sudo security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain /tmp/carbon6-ca.pem`, and restart the browser. (Or open the file in Keychain Access and set Trust → Always Trust.)
    - iPhone, iPad: open `http://<head>:7700/ca.mobileconfig`, install the profile in Settings, then enable it under General → About → Certificate Trust Settings.
    - Android: download `http://<head>:7700/ca.crt` in Chrome, then Settings → Security & privacy → More security settings → Encryption & credentials → Install a certificate → CA certificate, and pick `carbon6-ca.crt` (menu names vary a little by phone; search Settings for "CA certificate"). Android then shows a "network may be monitored" notice; that is expected for any user-installed CA. Then force-stop Chrome (Settings → Apps → Chrome → Force stop): if you ever clicked through the certificate warning, Chrome keeps showing "Not secure" until it restarts.
    - Elsewhere: add `http://<head>:7700/ca.pem` to the system's trusted certificates (Linux: `/usr/local/share/ca-certificates` + `update-ca-certificates`).
    - The CA is name-constrained to the head's names and private IP ranges, so it cannot vouch for any other site.
  - Or set `ui_tls_cert`/`ui_tls_key` to PEM files. With HTTPS on, plain HTTP on the UI port redirects to the HTTPS name.
  - Sign-in is on by default; `auth = false` turns it off.
  - Set `otlp_endpoint` (or `OTEL_EXPORTER_OTLP_ENDPOINT`) to export traces.
  - MCP servers, slash commands, agents, skills and hooks are configured in the UI (Settings) and stored on the head.
- **Worker:** its identity lives in `~/.local/state/c6w` (Linux) or `~/Library/Application Support/c6w` (macOS).
  - For Claude Code sessions, install `claude` on the worker and sign in there; set `C6_CLAUDE_BIN` if it is not on the worker's PATH.

### Upgrades

A new version of carbon6 upgrades its databases when it starts: the head's (`c6h.db`) and the memex's (`memex.db`). Workers' journals need no upgrading.

- **A backup first:** the database is copied to `backups/` next to it, named with the versions and the time, before anything changes. Backups are kept until you delete them.
- **All or nothing:** if a step fails, or a check finds data missing afterwards, the backup is put back and carbon6 refuses to start, saying why. The database is as it was, so the previous version still runs on it.
- **Nothing lost silently:** every table must keep its rows, unless the upgrade says it moves or removes them.
- **Try it first:** `c6h upgrade --check` (and `c6m upgrade --check`) upgrades a copy and prints what would change, leaving your database untouched. `c6h upgrade` upgrades without starting the head.
- **History:** **Settings → System** lists each upgrade with what changed and where its backup is.
- **Going back:** stop carbon6 and put the backup in place of the database.

### Labels

Sessions carry labels, which group them and decide which settings apply.

- **Automatic labels:**
  - `worker:<name>`;
  - `repo:<host/owner/name>`, from the git remote, so a repository is the same on every machine;
  - `folder:<path>`;
  - `model:<model>`.
- **Your own labels:** anything else, like `client:acme`, `spike` or `rust`. Add them when starting a session (they're remembered per folder), in a session's header with **+ label**, or with `c6 run -l`. A side chat keeps its parent's labels, and `/clear` keeps them for the fresh session.
- **Finding sessions.** The session list, search, the inbox, mission control and the activity dock share one label filter, kept in your browser. Type labels into it (suggestions come from your sessions; `repo:github.com/acme` covers the whole org), or click any label on a session. The inbox and the dock note how many sessions the filter hides (the dock shows the filter, with a clear button), the Inbox badge counts only what the filter shows (its tooltip gives the rest), browser notifications come only for sessions the filter shows (the toggle reads "🔔 filtered"), and mission control falls back to the most recent matching sessions. The session list's search also matches labels. On the command line, `c6 sessions -l <label>` and `c6 search -l <label> …` filter the same way.
- **"Applies to".** Commands, agents, skills and hooks each have an **Applies to** field in Settings:
  - empty means everywhere;
  - otherwise, labels a session must *all* have, e.g. `repo:github.com/acme/app` or `client:acme, rust`;
  - `repo:` and `folder:` match path prefixes, so `repo:github.com/acme` covers a whole organization and `folder:~/code` everything under it.
- **Same name, different labels.** Several commands, agents or skills can share a name if their labels differ; a session gets the most specific one that matches. Every matching hook runs.
- **Config tab.** A session's **Config** tab lists what applies to it, and why anything doesn't.
- **When changes apply.** Commands follow label changes at once. Agents, skills and hooks are fixed when a session starts.

### Memory

Notes kept across sessions (**Settings → Memory**). Each has a scope: one label, like `host:web1`, `repo:github.com/acme/app` or `client:acme`, or none for every session.

- **Where they go:** a new session gets the notes for its labels in its instructions, most specific and newest first.
- **The agent keeps them current:** it saves durable facts with `remember` (how a machine is set up, where things are, your preferences), corrects a note by replacing it, and removes stale ones with `forget`. Each change shows in the transcript.
- **Editing:** you can add, edit and delete notes in Settings. Notes written by an agent link to the session that wrote them.

### Policies and approvals

Policies (**Settings → Policies**) are rules for the agent's tool calls in sessions with given labels.

- **What a rule matches:** a tool-name pattern (`Bash|Write|Edit`; Claude Code's names work with every model) and an optional pattern over the call's input (`rm\s+-rf`, `git\s+push`).
- **What it does:**
  - **deny:** the agent is told why.
  - **ask:** the call waits for you. The transcript shows what it would run with **Allow** / **Deny**. The session moves to **Needs your approval** at the top of the inbox and the badge, the dock marks it, and a notification fires if the tab is in the background. Denying tells the agent you said no; an hour with no answer counts as no.
- **Order:** the first matching policy applies.
- **Presets:** ask before any change on `env:prod`, never run destructive commands on prod, ask before package changes or `git push`, keep `research` sessions read-only.
- **Scope:** policies apply to subagents' calls too. Claude Code sessions aren't covered.

### Memex

The memex is your knowledge store: a separate service, `c6m`, on the same fabric as the workers. It can run on any machine (this one is fine). Its design, and what comes next (documents and papers, Gmail, calendar and contacts, photos and videos), is in [docs/memex.md](docs/memex.md).

- **Connecting one:** **Settings → Memex → Create an enrollment token**, then on the machine that should hold it, `c6m enroll <token>` and `c6m run`. It keeps its store under its state folder.
- **carbon6 → memex:**
  - every session's digest (what was asked, what tools did, what it concluded);
  - published outputs;
  - pages your agents read with the browser or web fetch;
  - your memory notes.

  Each comes with the session's labels, which become things in the memex's graph (`client:acme`, `host:web1`, `repo:…`). Updates queue on the head while no memex is connected.
- **memex → carbon6:**
  - **Briefs:** a new session starts with a brief from the memex about its labels and its first message (notes, recent sessions about the same things, what else matches), in place of the memory section.
  - **Tools:** agents get `memex_search` (keywords and meaning), `memex_get` (an item in full, or a thing with its notes, items and related things) and `memex_note`.
- **@-mentions:** typing `@` in a message also offers things from the memex (people, clients, hosts, topics); `@memex:client:acme` attaches what the memex knows about it to the message.
- **Documents and papers:** add files on the Memex page (**Add files**) or keep folders in sync (**Settings → Memex folders**, on the memex's machine).
- **Bringing in data from other tools:** `memex.ingest` (on `POST /api/v1/memex/rpc`, with an API token) takes batches of items typed by MIME type: mail as `message/rfc822` (an old archive, for example), and documents as PDF, Word, Markdown, text or HTML. Each item comes back created, updated, duplicate or rejected with the reason. `memex.types` lists what each type accepts. Imported mail is threaded and searchable like synced mail, is never written back to Gmail, and becomes the synced copy if Gmail later has the same message. Design: [docs/memex.md](docs/memex.md) §4.5.
  - **Formats:** PDF, Word (.docx), Markdown, HTML and text are read.
  - **Papers** are recognized, with their title, authors, DOI or arXiv id, and the papers they cite. Cited papers become things too, so the memex holds a citation graph. For example, a paper's page lists the papers in your memex that cite it.
  - **Originals** open from the item.
- **Trails:** named paths through items and things, each step with a note on why it matters. Add steps from any item, or have agents add them with `memex_trail`. See them under **Memex → Trails**.
- **Search by meaning:** **Settings → Memex** picks an embedding model from one of your connections (Ollama's `nomic-embed-text`, OpenAI's `text-embedding-3-small`, …). Without one, search is by keywords.
- **Scopes:** sessions see sessions, outputs, web pages, notes, documents and papers. **Settings → Memex scopes** lets sessions with given labels see more (email, photos, once those are in). You see everything on the **Memex** page, where you can also search, open items and things, delete items, and write notes.

### Mail, calendar and contacts

With a memex connected, carbon6 is a client for your Gmail and Google Workspace accounts (☰ → **Mail**, **Calendar**, **Contacts**).

- **Setup** (**Settings → Mail & calendar**):
  - Create an OAuth client in your Google Cloud project (Web application, with the redirect URI shown).
  - Enable the Gmail, Calendar and People APIs.
  - Set the app to "In production", or "Internal" for Workspace.
  - Paste the client ID and secret, then **Add Google account**.

  The head keeps the sign-in encrypted; the memex gets short-lived access only.
- **Mail:**
  - **Import:** your whole mailbox is imported, newest first and resumable, then kept in sync every minute. Labels, read state, archive and drafts sync both ways.
  - **Views:** Inbox, **Needs reply** (someone else wrote last, to you directly, not a list), **Waiting** (you wrote last, over two days ago), Starred, Sent, Drafts, Archive, Spam, Trash, and your labels.
  - **Reading:** conversations, with HTML sandboxed and remote images off until you allow them.
  - **Writing:** Markdown compose, reply and reply all, attachments, drafts, **send later**, and **undo** (messages wait 10 seconds).
  - **In the graph:** every message is in the memex. Senders and recipients become people (one person across all their addresses), and company domains become organizations.
  - **Inbox:** carbon6's inbox shows mail that needs a reply.
- **Calendar:**
  - **Sync:** every calendar on every account, synced both ways.
  - **Views:** agenda, week and month, with recurring events expanded (exceptions included).
  - **Events:** details, RSVP, delete (this event, this and following, or all), and new events with invitations.
- **Contacts:**
  - Synced both ways.
  - People you write to (and hear from) but who aren't contacts are listed, and **Add to Google contacts** files them in a `carbon6` group. **Settings → Memex** can do this on its own.
- **Agents:** sessions whose scope includes email (**Settings → Memex scopes**, for example labels `mail`) get:
  - `mail_threads`, `mail_thread` (mail is marked as untrusted data, never instructions), `mail_draft` and `mail_send`;
  - `calendar_events` (with free/busy), `calendar_create` and `calendar_rsvp`.

  A built-in policy holds `mail_send`, `calendar_create` and `calendar_rsvp` for your approval (switch it off in **Settings → Memex**).
- **Automations on mail:** the trigger **When mail arrives** starts a template or workflow for matching mail (`mail.received from:@acme.com subject:invoice`), with the message as its input.

### Browser

Where the worker's machine has Chrome, Chromium, Brave or Edge (or `C6_CHROME` names one), sessions get a `browser` tool: a headless browser of their own.

- **Actions:** open, read, click, type, select, press, scroll, back, screenshot, evaluate, close.
- **Reading:** a page comes back as its text plus a numbered list of links, buttons and fields, which the agent clicks and types into by number.
- **Secrets:** `type` with `secret: NAME` types a secret's value without the model ever seeing it. Secret values are also hidden from everything the browser returns.
- **Profiles:** each session has its own profile under the worker's state folder, so sign-ins last while it works. Profiles unused for 30 days are removed, and a browser idle for 10 minutes is closed (and reopens on the next action).
- **Screenshots** go to the session's Outputs.
- **Policies** apply as to any tool. The presets include asking before the browser types into or picks options in a page.
- **SSH sessions:** the browser runs on the worker, not on the target.

### Notifications

Channels (**Settings → Notifications**) tell you about your sessions while you're away.

- **Kinds:** push (your own browsers and phones, see below), ntfy (a phone app; the URL is your topic), a Slack or Discord webhook, any webhook (JSON), or email over SMTP. Tokens and SMTP passwords are secrets.
- **Events:** `approval`, `failed`, `finished`, `notice`, `output`, `automation`, `budget`. A channel without a list gets approvals, failures, notices, automation failures and budgets.
- **Scope:** `applies_to` labels limit a channel to matching sessions (`env:prod`, `automation:`).
- **Approvals from your phone:** an approval notification links to a page showing the held call with **Allow** / **Deny**. The link is signed, works without signing in, and expires after an hour; answering takes a button press, so chat link previews can't answer for you. Links use the address under the channel list (taken from your browser the first time you save a channel).
- **The agent's `notify` tool:** agents can send you a notice (a title, a few lines, a priority), for example from an automation that finds a problem.
- **Testing:** **Send a test** sends a message on a channel and shows any error.

**Web push (no third party):** on each device, **Settings → Notifications → Turn on here**, then add a channel of kind `push`. The head encrypts each message for each device and sends it to the browser's own push service, signed with its own key. Tapping a notification opens the session, or for an approval its Allow / Deny page.

- **Requirements:** HTTPS, or `localhost`. With the head's own certificate authority, that means a device that trusts `/ca.pem`.
- **iPhone and iPad:** add carbon6 to the Home Screen (Share → Add to Home Screen) and open it from there; iOS only allows web push for home-screen apps.
- **Devices:** listed with when they last got a message. A subscription the push service says is gone is removed.

**On a phone:** the head works as a home-screen app (manifest, icon, standalone). Fields use 16px text so iOS doesn't zoom in on them, the composer folds its model picker away, and Usage and mission control fit small screens.

### Hosts

The host inventory (**Settings → Hosts**) lists machines your workers reach over SSH. Each host has a name, an SSH target (`user@host[:port]` or an `~/.ssh/config` alias on the worker), an optional worker, labels like `role:web` and `env:prod`, and optionally a check interval. carbon6 never handles SSH keys; the worker's own SSH setup is used.

- **Sessions:** use a host's name as the SSH target (the new-session form suggests them). The session gets the host's labels, so policies, budgets, routing, memory and secrets for `env:prod` apply without tagging each run.
- **Fleet runs:** a template's **Hosts by label** (`role:web, env:prod`, or `*`) starts one session per matching host, through that host's worker.
- **Checks:** the **Workers** page lists hosts with their last check (reachable, latency and system, or why not) and a **Check** button. With a check interval, the head checks on its own and sends a `host` notification when a host stops answering or comes back.

### Usage and budgets

**Usage and budgets** (in the ☰ menu) shows tokens and cost for the sessions matching the label filter, over today or the last 7, 30 or 90 days: totals, a daily chart, and breakdowns by model, connection, label, and the most expensive sessions (subagents count toward the session that started them).

- **Prices** (**Settings → Prices**): USD per million tokens, per model id or prefix (`claude-sonnet-*`); cache reads and writes default to the input price. Models without a price count as $0, and Usage lists them.
- **Budgets** (**Settings → Budgets**): a limit per day, week or month for sessions with given labels (`automation:`, `client:acme`, or everything).
  - **notify:** a `budget` notification once per period when it's used up.
  - **block:** the notification, plus running turns with those labels are stopped, and new sessions and messages with them are refused until the period ends.
- Usage from before this existed is filled in from the session logs once.

### Model routing

Routing rules (**Settings → Model routing**) choose the connection and model for sessions started without one: from the new-session form on **Auto**, `c6 run` without `--model`, and templates or automations that leave the model empty.

- **A rule:** labels it applies to (yours, or `worker:`, `folder:`, `host:`, `template:`; a bare `host:` means any SSH host), a connection (optional), a model, an effort and, optionally, a cheaper model for subagents. A named agent's own model still wins.
- **Order:** the first enabled rule that matches wins. A session started on a given connection only takes rules for that connection (or for any).
- **No match:** the connection of your latest session (if you didn't pick one), its default model, else the model of your latest session on it.
- **Presets:** Opus with Haiku subagents everywhere, remote hosts on a fast model at low effort, research at high effort, a template on a local model.

### Templates and fleet runs

A template (**Settings → Templates**) is a saved way to start a session:

- **Where:** a worker; `*` for every online worker; or a list of SSH hosts, reached through a worker.
- **How:** connection, model and labels.
- **What to ask:** a prompt, where `{{input}}`, `{{target}}`, `{{date}}` and `{{time}}` are filled in.

**Running one:** run a template from the Sessions page ("Run a template"; an app's templates are marked with its realm and run there), with `c6 run -t <name> [input]` (it finds the realm that has the template), or on a schedule or from a webhook (see Automations).

**Fleet runs:** a template with several targets starts one session per target, all sharing a `run:<id>` label. The UI opens mission control filtered to the run, and `c6 run -t` waits for them all and prints each one's last reply.

Settings has examples: a health check across hosts, updating every worker, a research digest.

### Automations: schedules and webhooks

An automation (**Settings → Automations**) starts a template:

- **On a schedule:** cron fields in the head's local time, with presets like "weekdays at 9:00" and `@daily`. Each automation shows its next run.
- **When its webhook is called:** `POST` any body to the URL you're shown once. JSON is passed to the template's `{{input}}` pretty-printed. Point CI, an alert manager or a form at it.

**Webhook URLs:** the URL is the webhook's only credential, so treat it like a password. **New URL** replaces it, and a wrong or switched-off URL answers 404.

**Runs:** each run is labelled `automation:<name>` and `run:<id>` and lands in your inbox when it finishes. Every automation shows its last run, any error, and **Run now**. Times missed while the head was down are skipped, not made up.

**History and health:**

- A run's outcome comes from how each of its sessions' first turn ended: **ok**, **failed** (an error, a session that could not start, or a target that did not start), or **stopped** (interrupted).
- Each automation shows dots for its last ten runs. **History** lists runs with their trigger (schedule, webhook or manual), duration, model calls and cost, and their sessions.
- A failed run sends an `automation` notification.
- **Switch off after** N failed runs in a row (the form suggests 3; never, if unset) turns the automation off and tells you.

### Workflows

A workflow (**Settings → Workflows**) is a list of steps, run in order:

- **run:** start a template (fanning out to its hosts or workers as usual). The step's output is its sessions' last replies.
- **approve:** wait for your go-ahead. Answer in the panel under the editor, or from the approval notification's link; "Stop" ends the run.
- **notify:** send a notification.

Inputs, titles and messages can use `{{input}}` (the workflow's input) and `{{steps.<id>.output}}`. A step with `when` runs only if an earlier step's output `contains` or does `not_contains` some text, or `changed` since the last run. `changed` compares the output's last `RESULT:` line, so a check that answers in different words each time only counts as changed when its result does.

- **Failures:** a failed run step ends the run (unless `continue_on_failure`) and sends an `automation` notification.
- **Starting one:** run it from the panel, or from an automation by choosing the workflow as its template. The automation's history then follows the workflow's outcome.
- **Durability:** runs are stored on the head and pick up again after a restart.
- **Presets:** tell me only when a check's result changes; check, ask me, then fix; research, then write it up.

### Connectors (MCP servers)

**Settings → MCP servers** has an **Add a connector…** menu:

- **GitHub:** issues, PRs, code.
- **Slack:** read and post messages.
- **PostgreSQL:** read-only queries.
- **A headless browser:** Playwright, for research and web tasks.
- **A generic remote HTTP server** with a bearer token.

**Secrets in configs:** `${NAME}` anywhere in a server's command, arguments, environment, URL or bearer token is filled in from that secret, so tokens stay in the secret store. A server that needs a secret the session doesn't have isn't started, and the session says which secret is missing.

**Scoping:** `"applies_to": [labels]` limits a server to matching sessions, like other settings.

### Outputs: decks, reports and other deliverables

When the agent finishes a deliverable (a slide deck, a report, a chart, a document), it publishes it with the `publish_file` tool, and it appears in the session's **Outputs** tab:

- **Preview by type:** HTML pages (decks included) in a sandbox, PDFs, images, Markdown and text. Anything else is offered for download.
- **Versions:** every publish is kept as a version, with a picker. Open and Download are next to the preview.
- **In the transcript:** a "📦 Published …" line marks each publish, with images shown inline.
- **Where content lives:** each version's content is stored in the session itself (up to 20 MB), so it's still there when the worker is offline or the file has changed since.
- **Getting started:** Settings → Skills has examples to start from. **Slide deck** builds one self-contained HTML deck that prints to PDF. **Research report** searches, cross-checks and writes a cited Markdown report.

### Web search with any model

Claude and OpenAI models search the web through their own provider. For the rest (Ollama and other OpenAI-compatible servers), choose a provider under **Settings → Web search**, and sessions get a `web_search` tool. The options:

- **Brave Search API** or **Tavily:** the API key is a secret, picked by name.
- **SearXNG:** self-hosted, so it needs only its URL, with the JSON format enabled.

To give every model the same search, tick "Offer it to every model".

### Remote machines over SSH

For a machine you can reach by SSH but don't want to install `c6w` on, start a session there *through* a worker. In the new-session form choose **SSH host, through the worker** and enter `user@host` (or an alias from the worker's `~/.ssh/config`) and a folder there (default: home). On the command line: `c6 run --ssh admin@web1 "install and start nginx"`.

- **What runs where.** The agent's shell commands, file reads and edits, grep and glob, and background commands all run on that machine.
- **Connection.** The worker connects with its own SSH setup: keys, agent and `~/.ssh/config`, never passwords (BatchMode). It keeps one multiplexed connection per host.
- **What the agent is told.** The session's system prompt names the machine and its OS. The agent is told it works non-interactively (`sudo -n`).
- **Secrets** reach remote commands through stdin, never through a command line.
- **Session view.** The session shows `⇄ user@host` and gets a `host:` label. Its **Terminal** tab opens an SSH shell in the session's folder there.
- **What stays on the worker.** Hooks and MCP servers run on the worker. Files on the target aren't checkpointed for rewind, and @-mentions refer to the worker's files, so they're off for SSH sessions.

### Sessions without a folder

Not every task belongs in a repository. Leave the folder empty when starting a session (or use `c6 run --scratch`), and the worker gives it a scratch workspace of its own, `scratch/<session>` in the worker's state directory. It's where the agent downloads, drafts and builds things for research, documents and one-off jobs.

### Secrets

Keep API keys, tokens and passwords under **Settings → Secrets**. Each one is an environment variable (`$GITHUB_TOKEN`) for the agent's shell commands, background commands included, in the sessions it applies to (an "Applies to" label list, like other settings).

- **The agent never sees a value.** The system prompt lists only names and descriptions, and a value that appears in command output is replaced by `[secret:NAME]` before the model or the UI sees it.
- **Storage.** Values are encrypted on the head and can only be replaced, never read back.
- **Delivery.** Workers fetch values when a turn starts, keep them in memory for a few minutes, and never write them to their journal. A secret scoped to a `worker:` label is only handed to that worker.

### Background tasks

The agent can start work in the background and carry on: long builds, test suites, servers (`bash` with `run_in_background`), and subagents (Task with `run_in_background`).

- **When a task ends, the agent is told.** It gets the exit code, how long the task ran and the end of its output (for a subagent, its report). If a turn is running, the agent sees it at its next step. If the session is idle, a new turn starts so the agent can react by itself. "Start the build and tell me when it's done" works without you checking back.
- **Opting out.** A task started with `notify: false` doesn't wake anyone. Use it for servers and watchers meant to keep running.
- **The running-tasks control.** The session header shows **⚙ N running**: each task, how long it has run, and a **Stop** button. The agent is told when you stop one.
- **In the transcript**, each ending appears as a one-line entry, with the agent's notice folded underneath.
- **Limit.** Notices start at most 20 turns per hour in an idle session. Beyond that they're held until your next message, and the session says so.
- **Worker restarts.** Tasks die with the worker; the agent is told they were lost, and a command left running by the old worker process is stopped.

### Subagents

The agent can hand work to a subagent with the Task tool. A subagent is a session of its own, shown as a thread under the call that started it (docs/subagents.md).

- **Watching one.** While it works, the thread shows its last few steps live. Click the thread to open its whole conversation, or ↗ to open it in its own column. The session header's **◆** button lists every subagent in the session, at any depth, with its state. Pick one to go to its thread.
- **Stopping one.** **■ Stop** in its thread ends its turn without stopping the agent that started it. That agent gets a summary of what the subagent did (its last message, the files it changed, its errors). Stopping the main turn stops the subagents it waits on too.
- **Continuing one.** Its conversation is kept, so it can pick up where it was:
  - after a stop, a failure or a worker restart, **↻ Resume** sends it the resume prompt (Settings → Prompts);
  - you can reply to it at any time from its thread;
  - the agent that started it can send it a message with `SendMessage`, as a follow-up or to steer one running in the background.
- **Where its report goes.** A report from a turn the agent waited on comes back as the call's result. For a turn you started, the report goes to the agent that started the subagent: at its next step if it's working, or with its next message if it's idle. Tick **Wake the agent that started it** to have it react right away.
- **Nesting.** Subagents can start subagents of their own. **Settings → Agents → Subagent limits** sets how deep (3 by default) and how many may work at once below one session (16).
- **Asking about one.** **↳ Ask** works in a subagent's thread too: the side chat sees its conversation and doesn't disturb it.
- **Claude Code sessions.** Claude Code's own subagents show as threads too, read-only: Claude Code runs and continues them.

### Rewind and edit

Hover one of your messages: **↶ Rewind** takes the conversation back to before it, and **✎ Edit** changes it and sends it again. Either can also restore files: before each turn the worker snapshots the session's folder into a private repository in its state directory (your own git repository, if any, is never touched; folders without git work too; `node_modules/`, `target/` and the like are skipped, as are files your `.gitignore` ignores). Rewound messages stay in the transcript, folded under a marker.

### Attachments and @-mentions

Paste, drop or pick (📎) images and text files into the message box; they go with the message (images as images, for models that see them; text files inline). Type `@` to mention a file or folder in the session's folder: a fuzzy finder completes the path, and the worker attaches the file (or a folder's listing) when the message arrives, so the model has it without a tool call. Files over 256 KB are mentioned but not inlined. Limits: 10 MB per upload, 20 MB per message. The Claude Code backend gets text only.

### Slash commands

Type `/` in the message box for the command list.

- **Built in:** `/compact [focus]` summarizes the conversation so far (optionally focused on something) to free context; `/init` writes an AGENTS.md describing the codebase; `/review [what]` reviews the uncommitted changes, or a branch or PR you name; `/clear` starts a fresh session in the same folder with the same model; `/export` downloads the conversation as Markdown.
- **Your own**, under **Settings → Slash commands**: a name, a description, an arguments hint and the prompt. In the prompt, `$ARGUMENTS` and `$1`…`$9` take the arguments, `` !`cmd` `` the output of a shell command run in the session's folder, and `@path` attaches a file. Changes apply to the next message, and a command named like a built-in replaces it. Start from an example (fix a GitHub issue, write a commit message, explain code).

### Agents and skills

Both live in Settings and are copied into each new session.

- **Agents** (**Settings → Agents**) are specialized subagents the model can launch with the Task tool. Each has:
  - a name;
  - a description, which the model reads to pick one;
  - optionally, the tools it may use (Claude Code's names, e.g. `Read, Grep, Glob`, work with every model; all tools when empty);
  - optionally, a model (`opus`, `sonnet`, `haiku` or a model id; honored on Anthropic connections);
  - its instructions, which become its system prompt.
- **Skills** (**Settings → Skills**) are instructions the agent loads by name when they apply. Only the name and description sit in its prompt until then.
- **On/off.** Each command, agent and skill can be switched off without deleting it.

### Hooks

Hooks are shell commands that run on the worker at set points in every session. Set them up under **Settings → Hooks**; the head stores them and each new session takes a copy. Start from an example (refuse `rm -rf`, protect `.env` files, format after edits, tell the agent the git branch, keep going until tests pass) or write your own.

- **Events:** `PreToolUse` and `PostToolUse` (narrowed by a tool-name regex; Claude Code's names like `Bash` and `Edit` work with every model), `UserPromptSubmit`, `Stop` and `SessionStart`. A hook can be limited to sessions in one folder, and switched off without deleting it.
- **How a command runs.** It runs in the session's folder, gets the event as JSON on stdin (`tool_name`, `tool_input`, `prompt`, …), and has `C6_PROJECT_DIR` set to the repository root.
- **Exit code 2 blocks**, with stderr as the reason:
  - `PreToolUse`: the call is refused and the agent is told why;
  - `UserPromptSubmit`: the message is shown struck through and never sent;
  - `Stop`: the agent keeps working, with the reason as feedback;
  - `PostToolUse`: stderr is added to the tool's result.
- **Exit 0.** Printing `{"decision": "block", "reason": …}` also blocks. Plain output from `UserPromptSubmit` and `SessionStart` is given to the agent as context.
- **Claude Code sessions** get the same hooks, passed to the `claude` CLI as settings.

### Workspace

The UI is a workspace of up to four tiles side by side, each a stack of columns:

- **Columns:** opening something (a session from the list, a search result, a side chat's parent) puts it in a column beside where you clicked, so where you came from stays open — a search stays beside the result you are reading. Earlier columns fold into spines; click one to go back to it (the columns after it fold to the right until you open something new). The header's nav swaps sections in place.
- **Sessions side by side:** from a list (Sessions, Inbox, Mission, search results), the first session opens beside the list; each further one opens in a new tile at the right, up to four (then it replaces the least recently used tile). A session that is already open is shown where it is instead of opening twice.
- **Tiles:** ⌘/Ctrl-click any link (or ⧉ in a column header, or **+ Tile**) to open it in a new tile. ⌘1–4 focus a tile; ⌘[ and ⌘] step back and forward in the focused tile.
- **Activity dock** (left, on wide screens): running sessions, ones that finished or failed while not on screen (amber/red), and pinned ones (☆ in a session's header). **Inbox** lists the same for triage; **Mission** shows them as live cards you can reply from.
- **Phones:** one tile at a time with a switcher, Inbox and **+** at the bottom; a column fills the screen, with ‹ › in its header and the nav in ☰.
- The workspace is saved in the browser; the address bar shows the focused column, so links and reloads land in the right place.

### Search

The search box in the header (⌘K) searches every message, tool call, tool output, thinking block and error in all sessions, including side chats and subagents. Words and `"quoted phrases"` must all match, as substrings (3+ characters each, case-insensitive); `-word` excludes. Results are grouped by session; each match links to its place in the transcript, opened and highlighted. In a session, the find bar (it opens from a search result, or with **Find**) highlights every match on the page (the current one in orange) and steps through every match in the session and its side chats: Enter / ⌘G for the next, Shift+Enter / ⇧⌘G for the previous, Esc to close. The index (SQLite FTS5) is kept in the head's database, updated as events arrive, and rebuilt automatically when its format changes.

## Local development

```bash
make dev-head                     # head on all interfaces using dev/c6h.toml; state in the standard data directory
make dev-token                    # prints a token
make dev-worker TOKEN=c6e1.…      # enrolls and runs a worker against it (later: just make dev-worker)
make dev-memex TOKEN=c6e1.…       # enrolls and runs the memex (token from Settings → Memex; later: just make dev-memex)
make dev-login-link               # a one-time link to sign in to the dev UI
```

- For UI work, `cd crates/c6-ui && trunk serve` proxies `/api` to the dev head.
- Evals: see [evals/README.md](evals/README.md).
