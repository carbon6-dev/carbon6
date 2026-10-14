# Packages — Design

Everything you install in carbon6 is a **package**: an app with a realm of its own, a kit of software and tools that runs in a sandbox, or a library of configuration. Packages live in a registry, local first. They can require other packages, and installing one asks you once for everything it and its new dependencies need, the way Android asks for an app's permissions.

## 1. Words

- **Package:** a manifest (name, version, what it requires, what it asks permission for) with optional parts. Its role comes from the parts it has:
  - **app:** has a realm and configuration for it (`decks`, `crm`);
  - **kit:** has an environment to build (a Dockerfile and files) and the tools it offers (`decks-render`, with tool `pptx`);
  - **library:** configuration with no realm of its own, added to the realm of whatever requires it (a skill pack such as `writing-style`).

  A package can be more than one.
- **Registry:** where packages are kept, each version content-addressed by a digest. carbon6's head has the **local registry**: packages you make, import, or that carbon6 ships. **Remote registries** come later, synced into the local one.
- **Install:** a package version put to use, either in a realm (apps, libraries) or for every worker to build (kits), with the permissions you approved.
- **Tool:** a command a kit offers to agents, with its arguments typed.

## 2. The manifest

```json
{
  "package": "decks",
  "version": "1.1.0",
  "description": "Presentation decks from a brief.",
  "requires": { "decks-render": "^1.0" },
  "permissions": {
    "secrets": [],
    "workers": "any",
    "grants": [],
    "files": "session",
    "network": "none",
    "cpus": 2,
    "memory_mb": 2048,
    "timeout_s": 300,
    "shell": false
  },
  "realm": "decks",
  "contents": { "collections": [], "skills": [], "templates": [], "views": [], "triggers": [], "records": {}, "memory": [] },
  "exports": { "templates": ["deck"] },
  "kit": {
    "build": { "dockerfile": "Dockerfile", "files": ["render.py"] },
    "tools": [{ "name": "pdf", "description": "…", "command": ["python3", "/opt/kit/render.py", "pdf", "{html}", "{out}"], "args": { "html": "path", "out": "path" } }]
  }
}
```

- **Name and version:** `package` (or `app`, as packages were first written) and a semantic `version`.
- **`requires`:** other packages by name, each with a version range (`^1.2`, `~1.2`, `>=1.0`, `1.2.3`, `*`).
- **`permissions`:** one list for everything the package asks for.
  - **For a realm:** `secrets` it needs set, `workers` (`any` or `dedicated`), and `grants` from other realms.
  - **For kit runs:** `files` (`session` or `session-ro`), `network` (`none` or `any`; allow-lists by host later), `cpus`, `memory_mb`, `timeout_s`, and `shell` (whether agents may run any command in it).
  - **Older packages:** `needs` is still read as part of `permissions`.
- **`realm` and `contents`:** an app's realm and configuration, as before (docs/os.md §5). A library has `contents` but no `realm`.
- **`exports`:** what an app offers to apps that require it (its templates).
- **`kit`:** the environment and its tools.
  - `build.dockerfile` and `build.files` are files stored with the package.
  - The Dockerfile names other kits as `kit:<name>` (`FROM kit:python-base`). Those kits must be in `requires`.
- **The digest:** a hash of the manifest and its files. Two packages with the same name and version but different digests can't both be in the registry.

## 3. Requiring packages

What requiring means depends on the role of what's required:

| Required package | The requirer gets |
|---|---|
| kit | its tools, in the sessions of the requiring realm; and, for a kit requiring a kit, `kit:<name>` in its Dockerfile, resolved to the locked version |
| library | its configuration, added to the requiring realm (and upgraded with it) |
| app | the app, installed in its own realm, and a grant to start its exported templates from the requiring realm |

- **Resolution:** installing a package resolves its requirements, then theirs, picking the newest version in the registry that each range allows.
  - **Locking:** every edge is locked to a digest, kept with the install.
  - **Sharing:** a package version is installed once and shared by everything that requires it.
  - **Cycles** are refused. So is a requirement an installed version doesn't meet, until you upgrade that package.
- **Kits built from kits:**
  - **Build order:** a kit's dependencies are built first, each version once.
  - **Digests:** a kit's digest covers its dependencies' digests, so a new base rebuilds only what uses it.
  - **Roots:** the recursion ends at roots: kits whose Dockerfile starts from a pinned upstream image (`debian:bookworm-slim`, by digest where possible) or `scratch`. Once a root's base image is mirrored locally, builds need nothing outside.
  - **Permissions don't carry over:** a kit used to build another lends its files, not its permissions.

## 4. Permissions, approval and the sandbox

- **One decision per install:** it lists everything the package and its new dependencies ask for, in plain words. For example:
  - "Read and write files in the session's folder. No network. Up to 2 CPUs and 2 GB of memory, 5 minutes a run."
  - "Built from: python-base 1.0."
  - "Needs secrets set: GITHUB_TOKEN."
- **Approval is per version.** An upgrade asks again only for what grew.
- **Revoking** a kit stops its tools.
- **The sandbox for every kit run:**
  - `--rm`, `--network none` (unless `any`), and a read-only root filesystem with a tmpfs at /tmp;
  - the session's folder at /work (read-only for `session-ro`), and nothing else of the host;
  - `--cap-drop ALL`, `--security-opt no-new-privileges`, and a non-root user;
  - `--pids-limit`, `--cpus` and `--memory` from the permissions; the run is killed at the timeout;
  - approved secrets as environment variables for that run only, never baked into images.
- **Builds** run with network access, since they fetch packages. Their code is part of what you approve: the decision shows what a kit is built from.

## 5. Workers

- **Runtimes:** a worker uses Docker or Podman (the same commands); Apple's `container` comes later. It reports which it has. Kit tools run only on workers with a runtime; elsewhere a kit tool says so plainly.
- **Builds:**
  - On first use, the worker fetches a kit's files from the head, then builds its dependencies and the kit locally.
  - In each Dockerfile, `kit:<name>` is replaced by the local tag of the locked version. Images are tagged `c6kit/<name>:<digest12>` and reused while they exist.
  - Build failures come back with the build's log.
- **Runs:**
  - Each tool call is one container run in the sandbox above.
  - **Path arguments** must be inside the session's folder and are mapped to /work.
  - **What comes back:** the tool's output and exit status. Files it writes are in the session's folder, where `read` shows images to the model.

## 6. Agents

- **`kit_run {kit, tool, args}`** runs a tool. With no `kit`, it lists the kits and tools the session may use.
- **`kit_shell {kit, command}`** runs a command in a kit whose permissions include `shell`.
- **`template_start {realm, template, input}`** starts a template:
  - in the session's own realm, or in another realm whose app exports it to this one (the grant made when the requiring app was installed);
  - the started sessions run in that realm, and the caller gets their ids.

## 7. The registry and the UI

- **The head keeps:**
  - packages (manifest, digest, files), in tables of their own;
  - installs (package, version, digest, realm or none, the approved permissions, what requires it, state);
  - the decision waiting on an install or upgrade.
- **The API:**
  - `/api/v1/packages`: list the registry, add a version (manifest and files), read one, read a file, delete an unused version;
  - `/api/v1/installs`: list, plan, install, upgrade, remove;
  - `/api/v1/apps`, kept as before for app packages (it now goes through the registry).
- **Settings → System → Packages** (in place of Apps):
  - the registry by name, with versions and roles;
  - what's installed, where, and what requires it;
  - add a package from a file or paste;
  - plan, install, upgrade, remove;
  - a kit's tools, permissions and Dockerfile.
- **Upgrade:** existing app installs move to installs, and their packages into the registry, on upgrade (migration 0039).

## 8. Build order

1. **The local registry and installs:** packages, files, digests; `requires` with ranges, locking, shared installs and cycle checks; one approval decision per install; apps moved over; the API and Settings page.
2. **Kits:**
   - worker runtime detection, builds in dependency order with `kit:` resolved, and sandboxed runs;
   - `kit_run` and `kit_shell`;
   - a session sees the kits its realm requires.

   *As built:* a kit is in a session when an install in the session's realm requires it, directly or through others; kits that offer no tools and no shell (bases, only built from) aren't listed. The worker uses `docker`, then `podman` (or `C6_CONTAINER_RUNTIME`). A build's context is a temporary folder, removed after; builds stop after 30 minutes. Runs are as the owner of the session's folder (nobody if that's root), with `HOME=/tmp`, 512 processes and a 512 MB /tmp; interrupting a call or reaching the timeout kills the container. Path arguments that leave the session's folder are refused, as are kits in sessions working over SSH. A kit with no tools is allowed (a base to build from).
3. **Libraries,** added to the requiring realm, and **apps requiring apps:** exports, the template grant, `template_start`.
   *As built:* libraries and the template grant came with step 1. `template_start` asks the head, which checks for an operations grant `template:<name>` from that realm to the session's (each use is logged on the grant); the started sessions carry the label `started-by:<session id>`.
4. **`decks-render`:** a kit with headless Chromium, python-pptx and open fonts, offering `screenshot`, `pdf` and `pptx`. **decks** requires it, so a worker needs only a container runtime.
   *As built:* `examples/packages/decks-render.json`, in the `PackageIn` form with `text_files` (a Dockerfile and `render.py` written as text). Tools `pdf`, `slides`, `slide {n}`, `pptx` (a picture per slide; editable PowerPoint is later). Chromium runs with `--no-sandbox` inside the container, which is the sandbox. decks 1.1.0 requires it and its skills and templates use `kit_run`. Built and checked on Docker Desktop (2026-10-14): the image is about 1.5 GB and its first build takes about a minute; a three-slide deck renders to PNGs, PDF and PPTX in seconds under the sandbox. Chromium only takes exact font names (apart from a few metric stand-ins such as Arial → Liberation Sans), so themes name the open fonts in the kit (Inter, P052 for Palatino). The test `decks_render_kit_renders_a_deck` runs only with `C6_TEST_DECKS_RENDER=1`.
5. **Later:** remote registries and sync, network allow-lists by host, Apple's `container`, WASM kits for workers without a runtime, GPU.
