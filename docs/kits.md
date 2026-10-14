# Kits — Design

Apps need software that workers don't have: python-pptx, LibreOffice, ffmpeg, a data stack. Installing it by hand on every worker isn't sustainable. Instead, carbon6 packages software as **kits**: a container image (or the definition to build one), the tools it offers, and the permissions it needs. A registry keeps kits; workers build and run them in a sandbox shaped by the permissions you approved.

## 1. Words

- **Kit:** a packaged environment with tools in it, named like `decks-render`. It holds a definition (a Dockerfile and its files, or a reference to a prebuilt image), a version, the **tools** it offers, and its **permissions**.
- **Tool:** a command a kit offers to agents, with a description and its arguments. Example: `pptx` in `decks-render`, which turns a slide spec into a .pptx file.
- **Registry:** where kits live. carbon6 has a **local registry** (in the head) for kits you make or install. **Remote registries** come later, synced into the local one.
- **Permissions:** what a kit may touch (files, network, secrets, resources). You approve them when you install it, as on Android, and carbon6 sets up the sandbox to match.

## 2. Goals

- **Fully bootstrappable:** a kit's definition is enough. A worker builds the image itself from the Dockerfile; nothing has to be downloaded from carbon6's makers.
- **Least privilege by default:** no network, only the session's folder, no secrets, bounded CPU, memory and time. Anything more is a permission you approved.
- **Pinned:** a kit version is fixed by its digest (the hash of its definition and files). Changing the definition makes a new version. Permissions are approved per version, and a version that asks for more asks you again.
- **Apps name the kits they need,** and installing the app installs (and asks for) them.

## 3. The kit manifest

```json
{
  "kit": "decks-render",
  "version": "1.0.0",
  "description": "Renders slide decks: HTML to PPTX, SVG to PNG.",
  "build": { "dockerfile": "Dockerfile", "files": ["pptx.py", "fonts/"] },
  "image": null,
  "tools": [
    {
      "name": "pptx",
      "description": "A slide spec (JSON) to an editable .pptx.",
      "command": ["python3", "/opt/kit/pptx.py", "{spec}", "{out}"],
      "args": { "spec": "path", "out": "path" }
    }
  ],
  "shell": false,
  "permissions": {
    "files": "session",
    "network": "none",
    "secrets": [],
    "cpus": 2,
    "memory_mb": 2048,
    "timeout_s": 300
  }
}
```

- **`build`** is a Dockerfile and its files, stored with the kit. **`image`** is the alternative: a pinned reference to a prebuilt image (`registry/name@sha256:…`), for remote registries later.
- **`tools`:** each one is a command template, with its arguments typed. A `path` argument must be inside the session's folder and is mapped into the sandbox.
- **`shell`:** whether agents may run arbitrary commands in the kit (a permission of its own).

## 3a. Kits built from kits

Building a kit often needs other software: a compiler for a tool, a Python base with packages. Those are kits too, so kits depend on kits.

- **Naming them:** a Dockerfile names other kits as `kit:<name>`, as a base (`FROM kit:python-base`) or as a build stage (`FROM kit:rust-builder AS build`).
- **Pinning:** the manifest locks each one to a version: `"uses": {"python-base": "sha256:…"}`. When building, carbon6 replaces `kit:<name>` with the local tag of that exact version.
- **A graph, built bottom-up:** the worker builds a kit's dependencies first, each version once, shared by every kit that uses it.
  - **Digests:** a kit's digest covers its dependencies' digests, so a new base rebuilds only what uses it.
  - **Cycles:** refused when a kit is saved.
- **Roots:** a kit whose Dockerfile starts from a pinned upstream image (`debian:bookworm-slim@sha256:…`), or from `scratch` with a root filesystem archive kept in the registry.
  - **Offline builds:** once a root's base is mirrored in the local registry, builds need nothing outside.
  - **The floor:** someone has to provide a first filesystem; bootstrapping below that isn't attempted.
- **Builds run on the worker's container runtime,** not inside a kit, so the recursion ends there. A builder kit (BuildKit or Buildah) could later make builds possible on workers without a runtime.
- **Permissions don't carry over:** a kit used to build another lends its files, not its permissions. At run time, only the running kit's approved permissions apply.
- **Approval shows build dependencies:** build steps run with network, so approving a kit lists what it is built from ("built from rust-builder 1.2 and python-base 3.1"). Unapproved dependencies are asked for in the same decision.
- **Updates:** a new version of a base kit leaves kits that use it on their locked version until you update them. The update is a new version of each kit, approved again only if its permissions grew.

## 4. Permissions and the sandbox

| Permission | Values | Sandbox |
|---|---|---|
| files | `session` (default): the session's folder, read-write, as /work; `session-ro`; plus named extra folders, each read-only or read-write | bind mounts; everything else read-only (`--read-only`, tmpfs for /tmp) |
| network | `none` (default); `hosts: [...]`; `any` | `--network none`; for hosts, a network with an allow-listing proxy run by the worker; or the default network |
| secrets | names of carbon6 secrets | passed as environment variables to that run only, never baked into the image |
| cpus, memory_mb, timeout_s | numbers, with defaults (1, 1024, 120) | `--cpus`, `--memory`, killed at the timeout |
| shell | true or false | whether `kit_shell` may run commands in it |
| devices | `gpu` (later) | `--gpus` |

Every run also gets `--cap-drop ALL`, `--security-opt no-new-privileges`, a non-root user, `--pids-limit`, and `--rm`.

**Approval:** installing a kit opens a decision listing its permissions in plain words. For example: "Read and write files in the session's folder. No network. Uses up to 2 CPUs and 2 GB of memory, for at most 5 minutes a run." A new version that asks for more lists what changed and asks again; one asking for the same or less needs no new approval. You can revoke a kit, and its tools stop working.

## 5. The registry (local first)

- **The head stores:**
  - each kit version's manifest and files, content-addressed by digest;
  - its approval (the permissions approved, by whom, when);
  - each worker's build status and log.
- **Settings → Kits** lists them, and lets you:
  - create a kit (manifest and Dockerfile, edited in place) or import one from a file;
  - see the tools and permissions;
  - approve or revoke;
  - see where it is built.

  Agents can draft kits through the API, and approving them stays yours.
- **Later, remote registries:** a registry is added by URL and synced (pull kits into the local registry, push yours). Pulled kits keep their digest, and their permissions are approved locally as for any kit.

## 6. Workers

- **A container runtime:** a worker finds one: Podman, Docker, or Apple's `container` on macOS. It reports which, and the explorer and Settings show it. Kits run only on workers with a runtime. A session whose app needs a kit is placed on such a worker, the way realm availability places it.
- **Builds:** on first use (or when you ask), the worker fetches the kit's files from the head and builds the image locally. The image is tagged `c6kit/<kit>:<digest>`, and the log goes to the head. A build has network access, since it needs to fetch packages, but a run never does unless the kit's permissions allow it.
- **Runs:** each tool call is one sandboxed container run with the flags from the approved permissions. The session's folder is mounted at /work, and output and exit status come back to the agent.

## 7. Agents

- **`kit_run {kit, tool, args}`** runs a kit's tool. It's the normal way.
- **`kit_shell {kit, command}`** runs a command in the kit, only if the kit has the `shell` permission.
- **Which kits an agent sees:** a session sees the kits its realm's apps need, plus any you've made available to the realm. Kits are global in the registry and used per realm.

## 8. Build order (local only)

1. **The registry in the head:** kits, versions, files, dependencies (locked, with cycles refused), approvals and builds; the API; Settings → Kits.
2. **Worker runtime detection, local builds in dependency order, and sandboxed runs**, with the permissions mapped to flags.
3. **The agent tools** `kit_run` and `kit_shell`, and kits available per realm.
4. **Apps declare kits** in `needs.kits`; installing an app installs and approves them.
5. **The first kit, `decks-render`:** HTML slides to PPTX with python-pptx, and SVG to PNG.
6. **Later:** remote registries and sync, WASM kits (for workers without a container runtime), network allow-lists by host, GPU.
