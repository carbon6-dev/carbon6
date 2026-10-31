# Example apps

The worked examples of the OS design (docs/os.md §6), as packages you can install: configuration only, no code.

- `service-factory.json`: finds gaps in app ecosystems, builds and runs small services, with five gates you approve (realm `factory`).
- `personal-crm.json`: keeps you in touch with people, drafting check-ins through a grant on home's mail; you send (realm `crm`).

Decks were an app here (`decks.json`, with the `decks-render` kit) until 2026-10-31; they are part of carbon6 now (docs/docs.md §5, §13), and an installed decks app's decks come in from Settings → Decks → Bring in decks.

To install one: Settings → System → Packages, **Choose a file…**, Add and install…, and answer the decision. Its templates then show on the Sessions page under **Run a template**, marked with the app's realm (`c6 run -t <name>` finds them too). Or with the API:

```
curl -X POST http://<head>/api/v1/apps -H 'content-type: application/json' --data @examples/apps/personal-crm.json
```

After installing, answer the grant decisions it asks for, set the secrets it lists (in its realm), and give it a worker if it wants one of its own. Export a realm you changed (Settings → System → Apps → Export) to get your version back as a package.
