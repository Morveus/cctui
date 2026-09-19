# Spawn admission: the per-machine RAM ceiling

A machine can be given a **RAM ceiling**, above which cctui makes new sessions
wait instead of starting them. It is off by default (no ceiling), so an
installation that never sets one behaves exactly as it did before.

Set it in **Settings › Resource monitoring**, per machine, in GB. Empty means
unlimited. The API is `PUT /api/v1/machines/{id}/mem-ceiling` with
`{"mem_ceiling_bytes": 48318382080}` (or `null` to remove it), for the
machine's owner or an admin.

## What it does

A spawn (and the launch of a draft) is let through when, on its machine,

    memory in use + 1.5 GiB per launch of the last 2 minutes + 1.5 GiB ≤ ceiling

The middle term covers the launches the last heartbeat cannot have seen yet: a
session grows for a while after it starts. 1.5 GiB is what an idle Claude Code
session costs with its stdio MCP servers.

Otherwise the session is created with the status `queued`, showing the figures
that hold it back, and the reaper (every 30 s) launches each machine's queue
oldest first, as long as the test passes. A claude-code session launches under
the id its queued row was shown with, so a link to it keeps working.

A human can always **launch it now** (past the ceiling) or **cancel** it, from
the session's page.

## What is guaranteed, and what is not

The request is durable: it holds the spawn payload, the env (encrypted with the
vault key) and the staged files. Rights are **not** frozen with it: at launch
the key that queued it is re-validated and its scopes re-read, so a revocation,
a disabling or a demotion in the meantime applies.

One launch is sent **at most once**. A row is marked `sending` (committed)
before anything goes out, and a row in that state is never picked up again.

What cannot be decided from the server alone is whether a **broken send**
reached the daemon: the frame may have been delivered and the answer lost. Such
a launch is not called a failure and is not silently dropped: the row goes to
`uncertain`, keeping the whole request, and the session shows **Launch outcome
unknown** with what to do. The same happens when the server dies mid-send (the
reaper settles it after 10 minutes).

Recovery, in that case:

1. check the machine (is a session already doing that work?);
2. if it did start, **cancel** the waiting one, it is only a placeholder;
3. if it did not, **launch again** from the session's page. That is the only
   thing that sends an uncertain launch a second time, and it is a human
   decision, because doing it blindly could create a duplicate.

A session that did start reconciles its own placeholder: when a session
registers under the queued id (claude-code), the reaper settles the row as
launched rather than inventing a failure. Codex mints its own thread id, so an
uncertain codex launch waits for that human check.

**Not covered yet.** Making this automatic for every adapter needs the daemon to
deduplicate a repeated `command_id` (persisted across its own restarts) and to
stamp the session it starts with that id. The `command_id` is already stored
with the queued row and stays the same across attempts, which is what such a
daemon would key on.

## What does not go through the queue

`CctuiAgent` children (a parent is waiting on that call), the self-update agent,
and waking a hibernated session. The ceiling covers HTTP spawns and draft
launches, which is what a burst of launches is made of.
