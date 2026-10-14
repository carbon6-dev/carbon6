# Example apps

The worked examples of the OS design (docs/os.md §6), as packages you can install: configuration only, no code.

- `service-factory.json`: finds gaps in app ecosystems, builds and runs small services, with five gates you approve (realm `factory`).
- `decks.json`: presentation decks from a brief: an outline you approve, a theme you choose, slides built as HTML and checked by eye, delivered as HTML and PDF (realm `decks`; docs/decks.md).
- `personal-crm.json`: keeps you in touch with people, drafting check-ins through a grant on home's mail; you send (realm `crm`).

`decks.json` requires the kit in `../packages/decks-render.json` (docs/packages.md): add that to the registry first (Settings → System → Packages, load the file, Add to registry); installing decks then installs it too, in the same decision. The kit is built on a worker the first time it's used, which needs Docker or Podman and downloads Debian packages, Chromium and fonts.

To install one: Settings → System → Packages, load the file, Add and install…, and answer the decision. Or with the API:

```
curl -X POST http://<head>/api/v1/apps -H 'content-type: application/json' --data @examples/apps/personal-crm.json
```

After installing, answer the grant decisions it asks for, set the secrets it lists (in its realm), and give it a worker if it wants one of its own. Export a realm you changed (Settings → System → Apps → Export) to get your version back as a package.
