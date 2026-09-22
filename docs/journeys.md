# Feature tour

A visual record of the web UI: eight flows, captured from a running instance on
a synthetic fixture, at desktop and phone width. It is committed alongside the
code it pictures, so `git log docs/journeys` is a history of how the app looked.

Each flow has a folder per variant (`desktop-dark`, `mobile-dark`, …) holding
one PNG per step, a storyboard, and an `index.md` carrying the captions.

| Flow | What it shows | |
| --- | --- | --- |
| Read the fleet at a glance | Sessions grouped by what they need, and the theme picker | [index](./journeys/sessions-list/index.md) |
| Follow a session while it works | The live transcript, tool calls, filters and the composer | [index](./journeys/follow-session/index.md) |
| Start a new agent | The new-session dialog, and parking it as a draft | [index](./journeys/spawn-session/index.md) |
| Find a session again | Free-text and faceted search over transcripts | [index](./journeys/search-sessions/index.md) |
| See what the fleet is costing | Live counts, token windows, per-model usage | [index](./journeys/usage-overview/index.md) |
| Bring a machine into the fleet | Users, keys, enrolment and machine liveness | [index](./journeys/enroll-machine/index.md) |
| Group accounts into a pool | Account cards, pool membership and the drag handle | [index](./journeys/accounts-pools/index.md) |
| Tune how agents behave | Appearance, session defaults, execution limits, redaction | [index](./journeys/settings-tour/index.md) |

Every flow is recorded in `desktop-dark` and `mobile-dark`. `sessions-list` is
also recorded in `light` and `gruvbox`, which is where the themes are shown.

## Refreshing the record

The screens are captured by hand and committed with the change that moved them,
so a pull request that alters the UI carries its own before/after. Nothing
regenerates them in CI; it only reads the diff and comments — the screens the
branch moved, and any journey it looks like it should have refreshed but did
not. That comment never fails the run.

Start a stack with demo data in it, then shoot the journeys your change affects:

```sh
make local/demo                              # stack on :8088 + fixture
cd webui
npm run journey:shoot -- --changed           # journeys your edits can affect
npm run journey:shoot -- sessions-list       # or name them
npm run journey:book                         # or all of them
```

`--changed` maps changed files to journeys and errs upward: anything under
`webui/src` that no rule claims counts as affecting every flow, so a shared
token or a new common component is never quietly missed.

Capture replays against a production build (`vite preview`), which the shoot
builds and starts for you; a dev server transforms modules on demand and can
lose the first click to hydration.

**A `vite preview` you started yourself must be restarted after every rebuild.**
It resolves its output directory once, at startup, so a server left over from an
earlier build — or from another worktree holding the port — answers with markup
whose hashed assets it no longer has, and the page loads blank. The shoot
defends against committing that twice over: it refuses to run when a
pre-existing server cannot serve the assets its own HTML references, and it
drives the page in a real browser before any capture, failing if no root element
rendered. Left to itself it starts the preview *after* the build, so the server
is never older than what it documents.

Screens are then palette-quantised, which
takes them to roughly a third of their size. That pass runs over the journeys
the shoot just captured, and only those: quantisation is lossy and cannot be
detected after the fact, so re-running it over the whole record would both
degrade screens nobody re-rendered and show them up as a diff.

The admin token defaults to `dev-admin` and only ever reaches the login
endpoint; the browser state it mints lands in `webui/journeys/.auth/`, which is
gitignored.

## The fixture

`deploy/local/fixture/` invents every machine, repository, prompt and account it
writes — no screenshot here comes from a real deployment. It is also just a good
way to run the app locally with something in it:

```sh
make local/demo    # start and seed
make local/seed    # re-seed a running stack
```

Seeded timestamps are relative to now, so sessions read as live right after a
run. That also means two captures of an unchanged UI are never byte-identical;
the record is refreshed deliberately, not diffed.

## What is not covered

**Account usage bars.** The Accounts page reads its 5h/7d windows live from the
provider on each request, so a fixture cannot fill them — a seeded account shows
"configured but not currently reported". Documenting that screen needs a real
credential, so it is left out.
