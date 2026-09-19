# Journeys: the guide-mode contract

The eight `*.journey.ts` files here serve two consumers from one source.
`npm run journey:book` compiles them with `journey compile --public`, seeds the
fixture in `deploy/local/fixture/seed.sh`, logs in as `dev-admin`, and lets a
Playwright robot drive every step to a screenshot under `docs/journeys/`. The
in-app tour (CCT-977) compiles the same files with the same `--public` flag,
mounts `@dorsk/journey/runtime` in `guide` mode, and lets a first-time user
drive them on their own instance.

This document is the decision record for the conversion (CCT-984). CCT-981
executes it, CCT-979 translates the copy it marks as keepers, CCT-983 consumes
the probe registry it defines. Nothing below is left for the implementer to
decide; if a case is missing, extend this file first.

## 1. Runtime facts the decisions rest on

All citations are to `@dorsk/journey` 0.4.0 (`src/runtime/engine.ts`,
`src/runtime/actors.ts`, `src/core/compile.ts`). CCT-977 pins `^0.2.0`; the
behaviours below exist since 0.2.0.

| Fact                                                                                                                                                                                | Where                                                            | Consequence for a guide                                                                                              |
| ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------- |
| `compile --public` drops every step with `qaOnly: true` and every `{ probe: 'qa.*' }` expectation. Nothing else changes.                                                            | `compile.ts` `compile()` / `compileStep()`                       | One file serves both consumers. A `qaOnly` step still runs in the book.                                              |
| The target is resolved **before** the card is shown. No match within `timeout` (default 10 s) throws and ends the run. `optional: true` skips the step instead.                     | `engine.ts` `resolveTarget()`                                    | A guided step's target must exist on an empty instance, or be `optional`, or be gated by a probe on the step before. |
| `guide` defaults to `next` when `do` is `none`, else `wait-for-user`. A `wait-for-user` step shows no Next button.                                                                  | `compile.ts` `compileStep()`; `engine.ts` `showCtx.next`         | `do: click` means the user must really click the spotlit element. The runtime never clicks for them.                 |
| Human actor, `fill` with a **literal** value: the step only completes once the input's value equals that literal exactly. With a `{ $param }` value any `input` event completes it. | `actors.ts` `humanActor.perform`                                 | Literal fills are unusable in a guide. Every guided fill uses `param()` or becomes a `do: none` step.                |
| Expectations are polled for `timeout` after the action, in every mode. Failure throws and ends the run.                                                                             | `engine.ts` `waitExpectations()`                                 | A guided step's `expect` must hold on real data. Fixture counts (`min: 4`, `equals: 1`) are `qaOnly` or dropped.     |
| `{ probe: name }` calls `mount({ probes })[name]` and passes on truthiness, or on deep equality with `equals`. An unregistered probe fails the step.                                | `engine.ts` `checkExpectation()`                                 | Probes live in one host registry, section 2.                                                                         |
| Target paths take `{param}` keys: `user[{me}]` reads `params.me`. Unresolved params throw.                                                                                          | `core/target.ts` `parseTarget()`; `resolve.ts` `resolvePath()`   | The host passes `params` at `start()`; the specs never hardcode a fixture name.                                      |
| A step whose `route` differs from the current URL shows "Open /x to continue" and a button that calls the `navigate` hook.                                                          | `actors.ts` `humanActor.navigate`                                | Cross-route steps are fine. `route` is only needed on the first step of a page.                                      |
| `when: { viewport: 'mobile' }` compares against the variant the host passes at `start()`. The default variant is `{ viewport: 'desktop' }`.                                         | `runtime/index.ts` `DEFAULT_VARIANT`; `engine.ts` `shouldSkip()` | The host derives `viewport` from a media query and passes it, or mobile-only steps never run in the app.             |
| The runtime has no journey-level entry gate and marks "done" only for `autostart.once`.                                                                                             | `runtime/index.ts` `doneKey()`                                   | Entry gating and done state are the host's job, computed from probes (section 2).                                    |
| No spec references `JOURNEY_VARS` or `$token`. The token only reaches `scripts/journey-auth.mjs`.                                                                                   | `grep -l 'param\|\$token' webui/journeys/*.ts` returns nothing   | The "touches `$token`" rule from the ticket matches zero steps. `qaOnly` is decided purely on fixture dependence.    |

Step count: 31 across the eight files. `follow-session/mobile-filters` has
no `say` today and is included in the tables below.

Anchor count: `grep -rho 'data-journey=' webui/src | wc -l` gives 46
`data-journey` values; 22 of those sites also set `data-journey-key`, so
keyed anchors such as `page[…]` and `tile[…]` expand to more than one target
at runtime. The full inventory, with render conditions, is in section 5 and
section 8.

## 2. Probe registry

One module, `webui/src/lib/journeys/probes.ts`, exporting a
`Record<string, () => Promise<boolean | number>>` that is passed to
`mount({ probes })` and imported by the CCT-983 checklist. Probes are plain
functions, not hooks: they read through the TanStack `QueryClient`
(`queryClient.fetchQuery` with the same `queryKey`/`queryFn` the page hooks
use) so the guide and the page never disagree.

| Probe               | Reads                                                                                                                                                                                     | True when                      | Used by                                                           |
| ------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------ | ----------------------------------------------------------------- |
| `me.admin`          | `useMe` (`lib/queries/meta.ts:5`), `role === 'admin'`                                                                                                                                     | the signed-in user is admin    | gates `enroll-machine/machines`, `accounts-pools`                 |
| `accounts`          | `useAccounts` (`lib/queries/accounts.ts:18`), `length > 0`                                                                                                                                | at least one account exists    | done for `accounts-pools`; checklist row 1                        |
| `pools`             | `useAccountPools` (`lib/queries/accounts.ts:52`), `length > 0`                                                                                                                            | at least one pool exists       | gates `accounts-pools/pool`                                       |
| `machines.online`   | admin: `useAllMachines` (`lib/queries/users.ts:32`); otherwise `useMachines(me.user_id)` (`:39`). `liveness === 'online'` and `!revoked_at`, as `routes/home.logic.ts` `machinesOnline()` | a daemon is heartbeating now   | done for `enroll-machine`; gates `spawn-session`; checklist row 2 |
| `machines.enrolled` | same source, `kind !== 'ephemeral' && !revoked_at`, count                                                                                                                                 | a machine has ever enrolled    | `enroll-machine/machines` copy; `tab[machines]`                   |
| `sessions`          | `useSessions` (`lib/queries/sessions.ts:6`), `length > 0`, any status including `draft`                                                                                                   | the instance has a session row | done for `spawn-session`; checklist row 3                         |
| `sessions.drafts`   | `useSessions`, `status === 'draft'`, count                                                                                                                                                | a draft is waiting             | `spawn-session/show-drafts`                                       |
| `sessions.queued`   | `useSessions`, `status === 'queued'`, count                                                                                                                                               | a spawn waits for RAM          | none yet                                                          |
| `sessions.live`     | `useSessionStats` (`lib/queries/sessions.ts:17`), `live > 0`                                                                                                                              | a session is in the registry   | gates `follow-session`, `search-sessions`                         |

`qa.*` probes: none are needed today. The fixture facts the book relies on are
expressed as `qaOnly` steps with count expectations, which `--public` already
strips. Add a `qa.` probe only when a QA step needs host state that no
selector exposes.

Per journey:

| Journey           | Entry gate (host refuses to start, links to the prerequisite guide instead) | Done                 |
| ----------------- | --------------------------------------------------------------------------- | -------------------- |
| `enroll-machine`  | none                                                                        | `machines.online`    |
| `accounts-pools`  | none                                                                        | `accounts`           |
| `spawn-session`   | `machines.online`                                                           | `sessions`           |
| `follow-session`  | `sessions.live`                                                             | ran to the last step |
| `usage-overview`  | none                                                                        | ran to the last step |
| `sessions-list`   | none                                                                        | ran to the last step |
| `settings-tour`   | none                                                                        | ran to the last step |
| `search-sessions` | QA-only, never started in the app                                           | n/a                  |

"Done" for the three onboarding journeys is derived from the probe every time
it is read, not from the runtime's `journey:done` event, so removing the last
account un-does `accounts-pools`. That is the CCT-983 contract. For the other
four, `journey:done` writes `data.onboarding.progress[id] = version` through
the CCT-977 storage adapter.

## 3. Public set and order

Offered to a new user, in this order:

1. `enroll-machine`. Nothing runs without a daemon. The card is visible to
   everyone on an empty Access page (`EnrollMachineCard.svelte:24`, rendered
   unconditionally at `AccessUserList.svelte:93`).
2. `accounts-pools`. A session needs credentials to do work. The draft button
   does not require an account (`SpawnModal.svelte:563`), which is why this
   is second rather than first: a user who skips it can still finish the
   third guide.
3. `spawn-session`. Requires `machines.online` because `draft` is disabled
   without a machine and working directory (`SpawnModal.svelte:713`, `:563`).
4. `follow-session`. Requires `sessions.live`. Offered once the third guide
   has produced something, or on any instance that already has a live
   session.

Replayable from Settings > Guides (CCT-982) but not pushed at first run:
`usage-overview`, `sessions-list`, `settings-tour`. They describe surfaces
rather than produce anything and every one of their targets renders on an
empty instance.

QA-only, never registered in the app: `search-sessions`. All three of its
steps assert fixture counts (`min: 4`, `equals: 1`, `min: 2`) and its target
`section[blocked]` does not render on an empty list
(`routes/sessions/+page.svelte:1041`). Search is one input; a spotlight on it
teaches less than its placeholder does.

The order is the CCT-983 checklist order (machine, account, session) with
the account row moved to the middle for the reason in item 2. CCT-983 must
use the order here.

## 4. Side-effect policy

A guide never performs an action, so every mutation is the user's own. The
rule is: a mutation the guide is _about_ is kept and named in the copy; a
mutation that is incidental is removed from the public tour.

| Journey                         | Mutation                                   | Decision                                                                                                                                                                                                                                             |
| ------------------------------- | ------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `spawn-session/save`            | `draft` click creates a real draft session | **Keep.** The guide exists to leave the user with something in the list. The card says so: "This saves a draft on your instance. Nothing runs until you launch it, and you can delete it from the list." Done is `sessions`, which counts the draft. |
| `spawn-session/name`, `/prompt` | typing into the dialog                     | Keep as `param()` fills so any text completes the step. No literal is prescribed.                                                                                                                                                                    |
| `follow-session/reply`          | typing into the composer, never sent       | **Remove the fill.** A guide that leaves text in a composer on someone's running session is a trap. Becomes `do: none` spotlighting `composer/message`.                                                                                              |
| `accounts-pools/menu`           | opens the card menu                        | Keep. Opening a menu mutates nothing.                                                                                                                                                                                                                |
| `sessions-list/themes`          | opens the theme picker                     | Keep. Picking a theme is the user's choice and already persisted per user.                                                                                                                                                                           |
| `enroll-machine/user`           | selects a user                             | Keep. Selection is URL state.                                                                                                                                                                                                                        |
| `settings-tour/*`               | none                                       | n/a                                                                                                                                                                                                                                                  |

No public step clicks `submit`, `enroll`'s copy button, or anything that
launches an agent or spends tokens.

## 5. Target audit

Every hardcoded name, count or index in the eight files, with its
replacement. Anchors are cited from `webui/src` at the line where
`data-journey` is set.

| File / step                                                                   | Current                                                                                        | Why it fails on a real instance                                                                                                                                                                                                                 | Replacement                                                                                                                                                                                                                                                                                                                      |
| ----------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `accounts-pools`, `LOOSE = 'acme-research'` used by `board`, `handle`, `menu` | `account[acme-research]/…`                                                                     | fixture account name (`seed-api.mjs:33`)                                                                                                                                                                                                        | `account[{account}]/…` with `params.account` = the first account name the host reads from `useAccounts`. The host only starts the journey with the param set; without an account, steps 2-4 are not reached (gate in section 6).                                                                                                 |
| `accounts-pools/pool`                                                         | `pool[production]`                                                                             | fixture pool name (`seed-api.mjs:57`); `PoolZone.svelte:58` only renders pools with members                                                                                                                                                     | `pool[{pool}]`, `params.pool` = first pool name, step `optional: true` so an instance without a pool skips it.                                                                                                                                                                                                                   |
| `enroll-machine/access`                                                       | `user[admin]`                                                                                  | the bootstrap user is literally named `admin` (`crates/cctui-server/src/auth.rs:142`), so this resolves on a fresh instance, but a renamed admin or a non-admin viewer (who sees only their own row, `routes/access/+page.svelte:36`) breaks it | `enroll` (`EnrollMachineCard.svelte:24`), which renders for every role even with zero users. The step becomes the page introduction.                                                                                                                                                                                             |
| `enroll-machine/user`                                                         | `user[admin]`                                                                                  | same                                                                                                                                                                                                                                            | `user[{me}]`, `params.me` = `MeResponse.user_name` (`bindings/MeResponse.ts`), keyed by `AccessUserList.svelte:40`.                                                                                                                                                                                                              |
| `enroll-machine/machines`                                                     | `{ role: 'tab', name: 'Machines 2' }`                                                          | the name embeds the fixture count, produced by `access_tab_machines({ count })` at `AccessDetail.svelte:69`; the runtime compares accessible names exactly (`resolve.ts` `accessibleName(el) === name`), so no regex escape exists              | New anchor: `AccessDetail.svelte` passes nothing to the tsumikit `Tabs` triggers, so add `data-journey="tab-trigger" data-journey-key={id}` on the tab list (see section 8). Until then the step is `qaOnly`. The tab is `disabled: !isAdmin`, so the step is also gated by `me.admin`.                                          |
| `follow-session`, `SESSION = 'a0000000-…0001'`                                | `session[a0000000-…]/title`                                                                    | fixture UUID (`seed.sql`)                                                                                                                                                                                                                       | `session[{session}]/title`, `params.session` = the first live session id from `useSessions`. Gate `sessions.live`.                                                                                                                                                                                                               |
| `follow-session/timeline`                                                     | `count: ['conversation/line', { min: 5 }]`                                                     | fixture transcript length (12 rows)                                                                                                                                                                                                             | `visible: 'conversation'` only.                                                                                                                                                                                                                                                                                                  |
| `follow-session/tools-only`                                                   | `hidden: 'conversation/line[assistant]'`, `visible: 'conversation/line[tool]'`                 | a real session may have no tool line yet, and `line` keys are roles (`ConversationLine.svelte:73`)                                                                                                                                              | `qaOnly`. The filter bar itself is shown by the public `filters` step (section 6).                                                                                                                                                                                                                                               |
| `search-sessions/start`                                                       | `section[blocked]`, `count: ['session', { min: 4 }]`                                           | `section[blocked]` renders only when the status grouping has rows (`+page.svelte:1041`, `:1060`)                                                                                                                                                | journey is QA-only.                                                                                                                                                                                                                                                                                                              |
| `search-sessions/free-text`                                                   | `equals: 1` for "pagination"                                                                   | fixture text                                                                                                                                                                                                                                    | QA-only.                                                                                                                                                                                                                                                                                                                         |
| `search-sessions/facet`                                                       | `min: 2` for `label:backend`                                                                   | fixture labels                                                                                                                                                                                                                                  | QA-only.                                                                                                                                                                                                                                                                                                                         |
| `sessions-list/list`                                                          | `section[blocked]`, `count: ['session', { min: 4 }]`                                           | same as above; `section` also needs `groupBy === 'status'`, the default in `settings.svelte.ts:289` but user-changeable                                                                                                                         | target `sections` (`SectionFilter.svelte:25`, always rendered); expectations reduce to `visible: 'sections'` and the heading. The count moves to a `qaOnly` twin step (section 6).                                                                                                                                               |
| `sessions-list/themes`                                                        | `{ css: '[data-tsu="ThemePicker"]' }`, `expect visible { role: 'group', name: 'dark themes' }` | none. The picker is in the header on every route (`Header.svelte:172`) and the group is `ThemeModePicker.svelte:64`                                                                                                                             | keep.                                                                                                                                                                                                                                                                                                                            |
| `spawn-session/name`, `/prompt`                                               | literal fills and `value:` expectations                                                        | literal fill blocks until the exact string is typed                                                                                                                                                                                             | `do: { kind: 'fill', value: param('var.label') }` and `param('var.prompt')`; expectations drop the `value` check and keep `enabled: 'draft'`. The book passes `label`/`prompt` through `vars` in `journey.config.ts`, which Playwright exposes as `var.<name>` (`src/playwright/fixtures.ts:127`), so screenshots are unchanged. |
| `spawn-session/open`                                                          | `expect enabled: 'draft'`                                                                      | `draft` is disabled until a machine and working directory are set (`SpawnModal.svelte:713`). The fixture seeds spawn memory so the dialog opens pre-filled; a first run does not                                                                | expectation drops to `visible: 'spawn'`, `visible: 'spawn/prompt'`. A new step `where` (target `where`, `MachineFields.svelte:92`, `do: none`) tells the user to pick the machine and folder; `enabled: 'draft'` moves to the `prompt` step, where it is a true precondition of `save`.                                          |
| `spawn-session/show-drafts`                                                   | `count: ['section[drafts]/session', { min: 1 }]`                                               | holds after `save` on a real instance too, since the draft just created is in that section (`+page.svelte:1077`)                                                                                                                                | keep, plus `{ probe: 'sessions.drafts' }`.                                                                                                                                                                                                                                                                                       |
| `usage-overview/*`                                                            | `tiles`, `tile[needs_input]`, `tile[machines]`, `windows`, `analytics`                         | none. All render unconditionally (`routes/+page.svelte:40-55`); the tiles read 0                                                                                                                                                                | keep.                                                                                                                                                                                                                                                                                                                            |
| `settings-tour/*`                                                             | `page[appearance]` … `page[privacy]`, `theme`                                                  | none. All eight `page` panels are always mounted (`SettingsPages.svelte:19-40`)                                                                                                                                                                 | keep.                                                                                                                                                                                                                                                                                                                            |
| `follow-session/mobile-filters`                                               | `when: { viewport: 'mobile' }`, `mobile-panel[filters]`                                        | the toggle is in the DOM on desktop but hidden by CSS (`DrawerToolbar.svelte:77`), and the runtime's default variant is desktop                                                                                                                 | keep, host passes `viewport` (section 1).                                                                                                                                                                                                                                                                                        |

Fixture names that appear only in copy, not in targets: none.

## 6. Per-step decisions

Legend. **guide**: kept in the public tour, user performs it. **qaOnly**:
stays in the book, stripped by `--public`. **rewrite**: kept, but target,
action or expectations change as listed. Copy column: **keep** means CCT-979
translates the existing body; **rewrite** means the body changes first and
the new text in the last column is the one to translate.

### enroll-machine (4 steps → 4 public + 1 qaOnly)

| Step       | Decision                                              | Target                                                          | Action | Expect                                                             | Probe                   | Copy                                                                                                                                                                                     |
| ---------- | ----------------------------------------------------- | --------------------------------------------------------------- | ------ | ------------------------------------------------------------------ | ----------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `access`   | rewrite                                               | `enroll`                                                        | none   | `visible: 'enroll'`                                                |                         | rewrite: "Access lists everyone and everything that can act on this instance. Until a machine has enrolled, this card is the only thing here that matters."                              |
| `enroll`   | rewrite                                               | `enroll`                                                        | none   | `visible: 'enroll'`, `probe: 'machines.online'`, `timeout: 600000` | done: `machines.online` | rewrite: "Copy this command and run it on the computer that will host your agents. Replace the token with one from your user. The guide moves on by itself when the machine reports in." |
| `user`     | guide                                                 | `user[{me}]`                                                    | click  | `visible: 'tab[keys]'`                                             |                         | rewrite: "Open your own user. Keys, machines, tokens and AI accounts each have a tab."                                                                                                   |
| `machines` | qaOnly until the tab-trigger anchor lands, then guide | `{ role: 'tab', name: 'Machines 2' }` → `tab-trigger[machines]` | click  | `visible: 'tab[machines]'`                                         | gate: `me.admin`        | rewrite: "The machine you just enrolled is listed here with its heartbeat. Online means it can host a session right now."                                                                |

The `access` step's original body ("Everything that can act on this
instance is listed here, one user at a time") is a screenshot caption. The
`enroll` body ("A machine joins by running the daemon with a user token")
describes rather than instructs. Both are rewritten.

### accounts-pools (4 steps → 2 public + 3 qaOnly)

| Step      | Decision | Target                                                                                           | Action | Expect                                                                  | Probe            | Copy                                                                                                                |
| --------- | -------- | ------------------------------------------------------------------------------------------------ | ------ | ----------------------------------------------------------------------- | ---------------- | ------------------------------------------------------------------------------------------------------------------- |
| `board`   | rewrite  | `accounts` (`AccountsBoard.svelte:76`, always rendered)                                          | none   | `visible: { role: 'heading', name: 'Accounts' }`, `visible: 'accounts'` |                  | rewrite: "Accounts are the provider credentials your agents run on. This board is empty until you add one."         |
| new `add` | guide    | new anchor `new-account` on the primary button at `routes/accounts/+page.svelte:114` (section 8) | click  | none                                                                    | done: `accounts` | new: "Add your first account. Once it is saved, this guide is complete; pools are for when you have more than one." |
| `pool`    | qaOnly   | `pool[production]`                                                                               | none   | as today                                                                |                  | keep for the book                                                                                                   |
| `handle`  | qaOnly   | `account[acme-research]/drag-handle`                                                             | none   | as today                                                                |                  | keep for the book                                                                                                   |
| `menu`    | qaOnly   | `account[acme-research]/account-menu`                                                            | click  | as today                                                                |                  | keep for the book                                                                                                   |

The pool, drag-handle and menu steps need two accounts and a pool to mean
anything. A user with one account learns nothing from a drag handle. They
stay in the book, where the fixture has three accounts and a pool. When a
"second account" milestone exists, they can return as a separate
`accounts-pools-2` journey gated on `pools`; that is out of scope here.

The journey title "Group accounts into a pool" no longer matches the public
tour. Public title: "Connect a provider account". Description: "Accounts are
the credentials work runs on. Add one so a session has something to run
with." The book keeps the current title through `qaOnly` steps only, so the
title must change in the file; `docs/journeys/accounts-pools/index.md` is
regenerated by `journey:book` and will show the new title. Accept that diff.

### spawn-session (6 steps → 7 public)

| Step          | Decision | Target                                        | Action                        | Expect                                                                                                     | Probe                   | Copy                                                                                                                             |
| ------------- | -------- | --------------------------------------------- | ----------------------------- | ---------------------------------------------------------------------------------------------------------- | ----------------------- | -------------------------------------------------------------------------------------------------------------------------------- |
| `open`        | rewrite  | `new` (`SessionControls.svelte:164`)          | click                         | `visible: 'spawn'`, `visible: 'spawn/prompt'`                                                              | gate: `machines.online` | rewrite: "Everything a run needs is in this one dialog: the machine, the folder, the prompt and the profile."                    |
| new `where`   | guide    | `where` (`MachineFields.svelte:92`)           | none                          | `enabled: 'draft'`                                                                                         |                         | new: "Pick the machine you enrolled and type the folder the agent should work in. The draft button lights up once both are set." |
| `name`        | rewrite  | `spawn/label`                                 | `fill`, `param('var.label')`  | none                                                                                                       |                         | rewrite: "Give the run a name you will recognise in the list."                                                                   |
| `prompt`      | rewrite  | `spawn/prompt`                                | `fill`, `param('var.prompt')` | `enabled: 'draft'`                                                                                         |                         | rewrite: "Say what you want done. The profile below decides which harness and model carry it out."                               |
| `save`        | guide    | `draft`                                       | click                         | `hidden: 'spawn'`, `probe: 'sessions.drafts'`                                                              | done: `sessions`        | rewrite: "This saves a draft on your instance. Nothing runs until you launch it, and you can delete it from the list."           |
| `sections`    | guide    | `sections/toggle` (`SectionFilter.svelte:28`) | click                         | `visible: 'sections/option[drafts]'`                                                                       |                         | keep                                                                                                                             |
| `show-drafts` | guide    | `sections/option[drafts]`                     | click                         | `visible: 'section[drafts]'`, `count: ['section[drafts]/session', { min: 1 }]`, `probe: 'sessions.drafts'` |                         | rewrite: "Your draft is here, holding the machine, folder, profile and prompt until you launch it."                              |

The default section set is `starred, live, dispatched`
(`sessions.logic.ts:49`), so `drafts` is off for a new user and the two
toggle steps are needed, as today. If the user has already switched drafts
on, `option[drafts]` is still rendered while the popover is open and the
click toggles it off; the `show-drafts` expectation then fails. Mark
`sections` and `show-drafts` `optional: true`; the `sessions.drafts` probe
on `save` has already proven the draft exists by the time the toggles run.

Book fidelity: `journey.config.ts` gains
`vars: { label: 'Add pagination to the orders endpoint', prompt: 'Add cursor pagination to GET /orders. Keep the response shape and cover it with a test.' }`
so the `param()` fills reproduce the current screenshots byte for byte. The
new `where` step captures nothing.

### follow-session (5 steps → 5 public + 1 qaOnly)

| Step             | Decision | Target                                 | Action | Expect                                           | Probe                 | Copy                                                                                                                                        |
| ---------------- | -------- | -------------------------------------- | ------ | ------------------------------------------------ | --------------------- | ------------------------------------------------------------------------------------------------------------------------------------------- |
| `open`           | rewrite  | `session[{session}]/title`             | click  | `visible: 'conversation'`, `visible: 'composer'` | gate: `sessions.live` | rewrite: "Open your running session by its name. The conversation opens beside the list on a desktop, over it on a phone."                  |
| `timeline`       | rewrite  | `conversation`                         | none   | `visible: 'conversation'`                        |                       | keep body, expectation loses the `min: 5` count                                                                                             |
| `mobile-filters` | guide    | `mobile-panel[filters]`                | click  | `visible: 'filters/quick[assistant]'`            |                       | none today; add "Open the filters" so the card is not blank.                                                                                |
| new `filters`    | guide    | `filters` (`DrawerToolbar.svelte:104`) | none   | `visible: 'filters/quick[assistant]'`            |                       | new: "These pills hide message kinds. Turning off assistant messages leaves the tool calls, the quickest way to see what an agent touched." |
| `tools-only`     | qaOnly   | `filters/quick[assistant]`             | click  | as today                                         |                       | keep for the book                                                                                                                           |
| `reply`          | rewrite  | `composer/message`                     | none   | `visible: 'composer/message'`                    |                       | rewrite: "Anything you type here goes to the running agent, so you can redirect it without restarting."                                     |

`tools-only` is the screenshot the book wants (tool lines only). In a guide
it would toggle a filter on the user's real session and then assert on line
roles that may not exist. The new `filters` step points at the same bar and
lets the user try it or not.

### usage-overview (3 steps → 3 public)

| Step        | Decision | Target      | Action | Expect   | Copy                                                                                                                                                  |
| ----------- | -------- | ----------- | ------ | -------- | ----------------------------------------------------------------------------------------------------------------------------------------------------- |
| `tiles`     | guide    | `tiles`     | none   | as today | rewrite: "Four numbers: sessions running now, sessions waiting on you, machines online, and the all-time total. They read zero until your first run." |
| `windows`   | guide    | `windows`   | none   | as today | keep                                                                                                                                                  |
| `analytics` | guide    | `analytics` | none   | as today | keep                                                                                                                                                  |

### sessions-list (2 steps → 2 public + 1 qaOnly)

| Step               | Decision | Target                                | Action | Expect                                                                  | Copy                                                                                                                                                |
| ------------------ | -------- | ------------------------------------- | ------ | ----------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------- |
| `list`             | rewrite  | `sections`                            | none   | `visible: { role: 'heading', name: 'Sessions' }`, `visible: 'sections'` | rewrite: "Sessions group by what they need from you: pinned first, then anything waiting on an answer. This filter chooses which groups are shown." |
| new `list-fixture` | qaOnly   | `section[blocked]`                    | none   | today's `count min 4`, `visible: 'section[blocked]'`, `capture: 'list'` | none (screenshot only)                                                                                                                              |
| `themes`           | guide    | `{ css: '[data-tsu="ThemePicker"]' }` | click  | as today                                                                | keep                                                                                                                                                |

The `capture: 'list'` moves to the qaOnly twin so `docs/journeys/sessions-list`
is unchanged. The public `list` step captures nothing.

### settings-tour (4 steps → 4 public)

| Step         | Decision | Copy |
| ------------ | -------- | ---- |
| `appearance` | guide    | keep |
| `sessions`   | guide    | keep |
| `execution`  | guide    | keep |
| `privacy`    | guide    | keep |

All four are `do: none`, cross-route, on always-mounted panels. Nothing
changes except the runtime's "Open /settings/sessions to continue" prompt
between them, which the `navigate` hook from CCT-977 turns into a one-click
"Take me there".

### search-sessions (3 steps → 0 public)

Whole journey QA-only: every step gets `qaOnly: true` and the file is not
registered in the app bundle. No copy is translated for it.

## 7. Copy pass summary for CCT-979

Bodies to translate as-is (**keep**): `spawn-session/sections`,
`follow-session/timeline`, `usage-overview/windows`, `usage-overview/analytics`,
`sessions-list/themes`, all four `settings-tour` steps. Ten strings plus
their titles.

Bodies that change first (**rewrite**, text in section 6):
`enroll-machine` all four, `accounts-pools/board`, `spawn-session/open`,
`/name`, `/prompt`, `/save`, `/show-drafts`, `follow-session/open`, `/reply`,
`usage-overview/tiles`, `sessions-list/list`. Fifteen strings.

New strings: `accounts-pools/add`, `spawn-session/where`,
`follow-session/mobile-filters`, `follow-session/filters`. Four strings plus
titles, plus the new `accounts-pools` title and description.

Titles change on: `accounts-pools` (journey), `enroll-machine/access`
("Start here: enroll a machine"), `enroll-machine/enroll` ("Run this on the
machine"), `spawn-session/save` ("Save it as a draft"),
`follow-session/reply` ("Steer it from here").

Strings in `qaOnly` steps are never shown in the app and are not translated:
`accounts-pools/pool`, `/handle`, `/menu`, `follow-session/tools-only`,
`enroll-machine/machines` while it is qaOnly, all of `search-sessions`.

The recurring defect in the current bodies is the screenshot voice: they
describe what a reader is looking at in someone else's fixture ("The
machines that answered", "Every session you have run is searchable"). The
rewrites above address the person performing the step, name the action, and
say what the empty state means. CCT-979 translates only after CCT-981 has
landed the rewrites, per the sequencing note on that ticket.

## 8. Missing anchors and blocked items

Anchors CCT-981 adds to the components. Values follow the existing
convention: static `data-journey`, dynamic ids in `data-journey-key`.

| Anchor                     | Where                                                                                                                                                                                                                                                                                                                                          | Needed by                 |
| -------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------- |
| `new-account`              | primary button at `routes/accounts/+page.svelte:114`                                                                                                                                                                                                                                                                                           | `accounts-pools/add`      |
| `tab-trigger` + key `{id}` | the tab list rendered by tsumikit `Tabs` at `AccessDetail.svelte:183`. `Tabs` forwards no attributes to its triggers (same limitation as `FilterSearchBar`, noted in `search-sessions.journey.ts`), so this needs either a tsumikit change or a wrapper element around each trigger. Until it exists `enroll-machine/machines` stays `qaOnly`. | `enroll-machine/machines` |

Host wiring CCT-981 provides alongside the specs, none of it in the spec
files themselves:

- `params` at `start()`: `me` (`MeResponse.user_name`), `account` and
  `pool` (first names from the accounts and pools queries), `session` (first
  live session id). A journey whose params cannot be filled is not started;
  the entry gate in section 2 covers every such case.
- `variant.viewport` from a `(max-width: …)` media query matching the
  layout's own breakpoint, or `follow-session/mobile-filters` never runs.
- `vars` in `journey.config.ts` for `label` and `prompt`, so the book
  reproduces today's `spawn-session` screenshots. In the app the host passes
  `params: { 'var.label': '', 'var.prompt': '' }`: `resolveParam()` in
  `runtime/text.ts` throws on a missing param before the card is shown, and
  the human actor treats any `$param` fill as satisfied by any input. The
  guide presenter never displays the fill value.
- `timeout: 600000` on `enroll-machine/enroll`: the runtime default of 10 s
  is the only ceiling on a probe wait, and enrolling a machine takes longer.

Not determinable from the repo and left for CCT-981 to verify at runtime:

- Whether tsumikit `Tabs` triggers carry `role="tab"`. The book resolves the
  current locator through Playwright, which proves it for `getByRole`; the
  runtime's `computedRole()` in `resolve.ts` is a subset of ARIA and was not
  exercised against tsumikit.
- Whether `resolveTarget()` for `sections/option[drafts]` sees the popover
  content in time: the option renders only while `open`
  (`SectionFilter.svelte:46`), and the click on `toggle` is the user's, so
  the 10 s default should suffice.

## 9. Acceptance for CCT-981, restated against this spec

- `journey compile --public` contains no `qaOnly` step and no `qa.*` probe;
  assert on the IR in a unit test, not by eye.
- Each of the four onboarding journeys runs end to end in `guide` mode on a
  fresh instance with `dev-admin` and no fixture, in the order of section 3,
  with the gates of section 2 refusing the out-of-order start.
- `npm run journey:book` produces byte-identical PNGs for every journey
  except `accounts-pools`, whose `index.md` carries the new title.
- `search-sessions` is absent from the app bundle's journey list.
