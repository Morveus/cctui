# cctui-webui

Mobile-first standalone SPA for cctui — Svelte 5 + SvelteKit (`adapter-static`,
app mode, no SSR) + TanStack Query. Replaces the legacy single-file
`crates/cctui-server/web/index.html` (kept for side-by-side comparison).

## Layout

- `src/lib/styles/variables.css` — **theme tokens**. Every color/space/font/radius
  is a token here; each selectable theme is a `[data-theme="…"]` palette block.
  `src/lib/theme.svelte.ts` persists/applies the chosen theme, while `reset` +
  base + components live in `app.css` and only consume tokens.
- `src/lib/bindings/` — TypeScript types **generated from the Rust structs** via
  ts-rs (committed). Regenerate with `npm run bindings` (or `make bindings` at
  the repo root) after changing an annotated Rust type. Imported via `@bindings`.
- `src/lib/api.ts` / `queries.ts` / `ws.svelte.ts` — typed fetch client, thin
  TanStack query/action wrappers, and the shared TUI websocket.
- `src/routes/` — `/` overview, `/sessions` (list + conversation drawer + spawn),
  `/users` (machines + tokens admin).

## Dev

```sh
npm install
npm run dev        # http://localhost:5273
npm run check      # svelte-check
npm run build      # static SPA into build/
npm run preview    # serve build/ — restart it after every rebuild
```

`vite preview` resolves the build directory once, at startup: a server left
running across a rebuild serves HTML referencing asset hashes it no longer has,
and the page comes up blank. Restart it rather than reloading.

The API origin is runtime config: `static/config.js` sets
`window.CCTUI_CONFIG.apiBase`. Override that file per-deployment (it is served
with `no-store`) to retarget the API without rebuilding. Auth is a Bearer token
held in `localStorage`; the server allows the cross-origin calls via CORS.

### gh-review connector (CCT-610)

`static/config.js` also carries `window.CCTUI_CONFIG.ghreviewUrl` — the origin of
the gh-review backend (epic CCT-600). When set, a **Review** nav entry mounts the
`ghreview-ui` app (imported as a workspace dependency, `../ghreview-ui`, aliased
`$ghreview` in `vite.config.ts`) under `/review`, passing the backend URL plus a
bearer minted for the signed-in user (`src/lib/ghreview.ts`) — no second login.
When it is empty/unset the connector degrades gracefully: the Review entry hides
and `/review` shows a "not configured" panel. The embed is lazy-loaded, so its
chunk ships only to deployments that enable it.

## Deploy

Built into an nginx image (`webui/Dockerfile`) and shipped as the standalone
`cctui-ui` Deployment (wire it up in your own GitOps/kustomize overlay).
`make ui/image/release` from the repo root builds + pushes the image.
